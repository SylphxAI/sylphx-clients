//! Plan (spec §3) and apply (spec §4): the planner reads each declared
//! Resource with Get, asks the owning service what the declaration would
//! become with Update `validate_only`, and decides an action from the table;
//! the writer sends the ordinary standard methods and waits until they
//! settle.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sylphx::Error;
use tokio::time::Instant;

use crate::check::check;
use crate::declare::{Declaration, Declarations, Removed};
use crate::diff::{json_patch, view_of, PatchOp};
use crate::eligibility::Declarable;
use crate::error::ApplyError;
use crate::hash::{managed_object, resource_hash, to_hex, Mode, RESERVED_PREFIX};
use crate::registry::{Registry, ResourceType};
use crate::wire::{self, Wire};

/// The label naming the manager of a Resource.
pub const MANAGED_BY: &str = "sylphx-managed-by";
/// This tool's value of [`MANAGED_BY`].
pub const MANAGER: &str = "apply";
/// Annotation: the hash of the desired state the last apply wrote.
pub const ANN_APPLIED: &str = "sylphx-applied";
/// Annotation: `<repository>@<commit>:<manifest path>`.
pub const ANN_SOURCE: &str = "sylphx-apply-source";
/// Annotation: the Release (UUIDv7) that wrote the Resource.
pub const ANN_RELEASE: &str = "sylphx-apply-release";

/// What a plan decides for one Resource (spec §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    Create,
    Update,
    NoOp,
    Drift,
    Adopt,
    Orphan,
    Forget,
    Destroy,
    Superseded,
    Refused,
}

impl Action {
    /// The action writes the Resource.
    pub fn writes(self) -> bool {
        matches!(
            self,
            Action::Create | Action::Update | Action::Drift | Action::Adopt | Action::Forget
        )
    }
}

/// One Resource of a plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub action: Action,
    pub kind: String,
    /// The Resource's full name.
    pub name: String,
    /// JSON Patch from the live managed state to the desired one; sensitive
    /// values are redacted.
    #[serde(default)]
    pub diff: Vec<PatchOp>,
    pub reason: String,
}

/// A plan: one item per Resource.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Plan {
    pub items: Vec<PlanItem>,
}

impl Plan {
    /// Anything a write would change (`--detailed-exitcode` 2).
    pub fn has_changes(&self) -> bool {
        self.items.iter().any(|i| i.action.writes())
    }

    /// The reasons of the items the plan refuses.
    pub fn refusals(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.action == Action::Refused)
            .map(|i| format!("{}: {}", i.name, i.reason))
            .collect()
    }

    /// The plan as the result document, with no writes made.
    pub fn report(&self) -> Report {
        Report {
            resources: self
                .items
                .iter()
                .map(|i| Applied {
                    action: i.action,
                    kind: i.kind.clone(),
                    name: i.name.clone(),
                    reason: i.reason.clone(),
                    code: None,
                    diff: i.diff.clone(),
                    outcome: if i.action == Action::Superseded {
                        Outcome::Superseded
                    } else {
                        Outcome::Unchanged
                    },
                })
                .collect(),
            unsettled: Vec::new(),
            error: None,
        }
    }
}

/// What happened to a Resource of an apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Written, and settled.
    Written,
    /// Written; still reconciling at the timeout (not a failure).
    Unsettled,
    /// A newer Release wrote it; nothing was written.
    Superseded,
    /// Nothing to write, or refused.
    Unchanged,
    /// The write failed; the run stopped here.
    Failed,
}

/// One Resource of a result document.
#[derive(Debug, Clone, Serialize)]
pub struct Applied {
    pub action: Action,
    pub kind: String,
    pub name: String,
    pub reason: String,
    /// The API error code of a failed write; null otherwise.
    pub code: Option<String>,
    pub diff: Vec<PatchOp>,
    #[serde(skip)]
    pub outcome: Outcome,
}

/// The result of a plan or an apply. It serialises as
/// `{"resources": [{action, kind, name, reason, code, diff}], "unsettled":
/// [names]}`; the Release step reads it.
#[derive(Debug, Serialize)]
pub struct Report {
    pub resources: Vec<Applied>,
    /// Names of Resources still reconciling at the timeout
    /// (`apply_unsettled:<name>`).
    pub unsettled: Vec<String>,
    /// The failure that stopped the run, if any (also in `resources`).
    #[serde(skip)]
    pub error: Option<ApplyError>,
}

impl Report {
    /// The result document as JSON: what the CLI prints and writes to the
    /// termination log.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".into())
    }

    /// The run failed: a write failed, or the plan refused a Resource.
    pub fn failed(&self) -> bool {
        self.error.is_some() || self.resources.iter().any(|r| r.action == Action::Refused)
    }

    /// `applied: 1 created, 2 updated, 1 drift put back, 1 orphan`.
    pub fn summary(&self) -> String {
        let count = |a: Action| {
            self.resources
                .iter()
                .filter(|r| r.action == a && r.outcome != Outcome::Failed)
                .count()
        };
        let parts: Vec<String> = [
            (count(Action::Create), "created"),
            (count(Action::Update), "updated"),
            (count(Action::Adopt), "adopted"),
            (count(Action::Drift), "drift put back"),
            (count(Action::Forget), "forgotten"),
            (count(Action::Orphan), "orphan"),
            (count(Action::Superseded), "superseded"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, w)| format!("{n} {w}"))
        .collect();
        if parts.is_empty() {
            "applied: no changes".into()
        } else {
            format!("applied: {}", parts.join(", "))
        }
    }
}

/// How a run behaves.
#[derive(Debug, Clone)]
pub struct Options {
    /// The Release this run belongs to (UUIDv7). `None` outside a Release: a
    /// fresh id names the run, Resources carrying `sylphx-apply-release` are
    /// refused unless `force`, and that annotation is left unchanged.
    pub release_id: Option<String>,
    /// `<repository>@<commit>:<manifest path>`.
    pub source: String,
    /// Write a Resource a Release manages, from outside a Release.
    pub force: bool,
    /// Per Resource: how long to retry etag races and wait for it to settle.
    pub timeout: Duration,
    /// Pause between polls and retries.
    pub poll_interval: Duration,
    /// The environment's full name; read from `GET /v1/whoami` when `None`.
    pub env: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            release_id: None,
            source: String::new(),
            force: false,
            timeout: Duration::from_secs(600),
            poll_interval: Duration::from_secs(2),
            env: None,
        }
    }
}

/// Where a step came from, so it can be planned again.
#[derive(Debug, Clone, Copy)]
enum Src {
    Decl(usize),
    Removed(usize),
    Orphan,
}

/// The write a step makes.
#[derive(Debug, Clone)]
struct Write {
    kind: String,
    name: String,
    reconciled: bool,
    body: Value,
    mask: Vec<String>,
    allow_missing: bool,
    l_etag: String,
    l_hash: Option<String>,
    l_release: Option<String>,
}

#[derive(Debug, Clone)]
struct Step {
    item: PlanItem,
    src: Src,
    write: Option<Write>,
}

struct Live {
    value: Value,
    etag: String,
}

/// Plans and applies declarations through one `Wire`.
pub struct Applier<'a, W: Wire> {
    reg: &'a Registry,
    declarable: &'a Declarable,
    wire: &'a W,
    opts: Options,
    run_id: String,
}

fn labels_of(v: &Value) -> BTreeMap<String, String> {
    v.pointer("/meta/labels")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn annotation<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
    v.pointer("/meta/annotations")?
        .get(key)?
        .as_str()
        .filter(|s| !s.is_empty())
}

fn etag_of(v: &Value) -> String {
    v.pointer("/meta/etag")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The `sylphx-*` labels of `l` other than the manager's: kept on a write.
fn reserved_labels(l: &Value) -> BTreeMap<String, String> {
    labels_of(l)
        .into_iter()
        .filter(|(k, _)| k.starts_with(RESERVED_PREFIX) && k != MANAGED_BY)
        .collect()
}

/// The API error code of a failed call.
fn code_of(e: &ApplyError) -> Option<&str> {
    match e {
        ApplyError::Api(Error::Api { code, .. }) => Some(code.as_str()),
        _ => None,
    }
}

fn is_operation(v: &Value) -> bool {
    v.get("spec").is_none()
        && v.get("meta").is_none()
        && (v.get("done").is_some() || v.get("target_generation").is_some())
}

fn int_of(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn condition<'v>(resource: &'v Value, ty: &str) -> Option<&'v Value> {
    resource
        .pointer("/status/conditions")?
        .as_array()?
        .iter()
        .find(|c| {
            c.get("type").and_then(Value::as_str) == Some(ty)
                && c.get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("true"))
        })
}

enum Settled {
    Yes,
    Pending,
    Stalled(String),
}

/// Whether a written Resource has settled (parent spec §3.3): observed
/// generation equals generation and `Reconciling` is not `TRUE`.
fn settled(resource: &Value, reconciled: bool) -> Settled {
    if let Some(c) = condition(resource, "Stalled") {
        let msg = c.get("message").and_then(Value::as_str).unwrap_or_default();
        return Settled::Stalled(msg.to_string());
    }
    if !reconciled {
        return Settled::Yes;
    }
    let generation = int_of(resource.pointer("/meta/generation"));
    let observed = int_of(resource.pointer("/status/observed_generation"));
    if observed == generation && condition(resource, "Reconciling").is_none() {
        Settled::Yes
    } else {
        Settled::Pending
    }
}

impl<'a, W: Wire> Applier<'a, W> {
    pub fn new(
        reg: &'a Registry,
        declarable: &'a Declarable,
        wire: &'a W,
        opts: Options,
    ) -> Applier<'a, W> {
        let run_id = opts
            .release_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
        Applier {
            reg,
            declarable,
            wire,
            opts,
            run_id,
        }
    }

    /// The plan for `decls`, with no writes. `previous` is the plan stored
    /// with the live Release, if any: Resources it named that no declaration
    /// names any more are looked for as orphans.
    pub async fn plan(
        &self,
        decls: &Declarations,
        previous: &[PlanItem],
    ) -> Result<Plan, ApplyError> {
        let (_, steps) = self.plan_steps(decls, previous).await?;
        Ok(Plan {
            items: steps.into_iter().map(|s| s.item).collect(),
        })
    }

    /// Plans, then writes every Resource in order and waits for each to
    /// settle. A plan that refuses anything writes nothing. A failed write
    /// stops the run; Resources already written stay written, and the
    /// failure is in the report.
    pub async fn apply(
        &self,
        decls: &Declarations,
        previous: &[PlanItem],
    ) -> Result<Report, ApplyError> {
        let (env, steps) = self.plan_steps(decls, previous).await?;
        let plan = Plan {
            items: steps.iter().map(|s| s.item.clone()).collect(),
        };
        if !plan.refusals().is_empty() {
            let mut report = plan.report();
            report.error = Some(ApplyError::Refused(plan.refusals()));
            return Ok(report);
        }
        let mut report = Report {
            resources: Vec::new(),
            unsettled: Vec::new(),
            error: None,
        };
        for step in steps {
            match self.execute(&env, decls, step).await {
                Ok(applied) => {
                    if applied.outcome == Outcome::Unsettled {
                        report.unsettled.push(applied.name.clone());
                    }
                    report.resources.push(applied);
                }
                Err((applied, error)) => {
                    report.resources.push(applied);
                    report.error = Some(error);
                    break;
                }
            }
        }
        Ok(report)
    }

    async fn env(&self) -> Result<String, ApplyError> {
        if let Some(e) = &self.opts.env {
            return Ok(e.clone());
        }
        let me: Value = wire::client(self.wire, Vec::new())
            .call(wire::get("whoami", Vec::new()))
            .await?;
        let seg = |key: &str| {
            me.get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .rsplit('/')
                .next()
                .unwrap_or_default()
                .to_string()
        };
        let env = me.get("env").and_then(Value::as_str).unwrap_or_default();
        if env.starts_with("orgs/") {
            return Ok(env.to_string());
        }
        let (org, project, env) = (seg("org"), seg("project"), seg("env"));
        if org.is_empty() || project.is_empty() || env.is_empty() {
            return Err(ApplyError::Unexpected(
                "the key is not scoped to an environment (GET /v1/whoami has no env)".into(),
            ));
        }
        Ok(format!("orgs/{org}/projects/{project}/envs/{env}"))
    }

    async fn get_live(&self, name: &str, rt: &ResourceType) -> Result<Option<Live>, ApplyError> {
        let got: Result<Value, Error> = wire::client(self.wire, Vec::new())
            .call(wire::get(name, Vec::new()))
            .await;
        match got {
            Ok(value) => {
                let etag = etag_of(&value);
                // A singleton's Get has no etag before its first write.
                if rt.is_singleton() && etag.is_empty() {
                    return Ok(None);
                }
                Ok(Some(Live { value, etag }))
            }
            Err(Error::Api { status, code, .. })
                if status == 404 || code.as_str() == "RESOURCE_NOT_FOUND" =>
            {
                Ok(None)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn managed_mask(&self, rt: &ResourceType) -> Vec<String> {
        let mut mask: Vec<String> = self
            .reg
            .managed_fields(rt)
            .iter()
            .map(|f| format!("spec.{}", f.name))
            .collect();
        mask.push("meta.labels".into());
        mask.push("meta.display_name".into());
        mask
    }

    /// The Resource a declaration asks for. With `applied` set it carries
    /// the markers of a write.
    fn desired_body(
        &self,
        name: &str,
        d: &Declaration,
        keep: &BTreeMap<String, String>,
        applied: Option<&str>,
    ) -> Value {
        let mut labels = Map::new();
        for (k, v) in d.meta.labels.iter().chain(keep.iter()) {
            labels.insert(k.clone(), Value::String(v.clone()));
        }
        let mut meta = Map::new();
        if let Some(hash) = applied {
            labels.insert(MANAGED_BY.into(), Value::String(MANAGER.into()));
            let mut ann = Map::new();
            ann.insert(ANN_APPLIED.into(), Value::String(hash.to_string()));
            ann.insert(ANN_SOURCE.into(), Value::String(self.opts.source.clone()));
            if let Some(release) = &self.opts.release_id {
                ann.insert(ANN_RELEASE.into(), Value::String(release.clone()));
            }
            meta.insert("annotations".into(), Value::Object(ann));
        }
        meta.insert("labels".into(), Value::Object(labels));
        if let Some(dn) = &d.meta.display_name {
            meta.insert("display_name".into(), Value::String(dn.clone()));
        }
        json!({"name": name, "meta": Value::Object(meta), "spec": Value::Object(d.spec.clone())})
    }

    /// Update `validate_only`: the normalised desired state.
    async fn validate(
        &self,
        name: &str,
        mask: &[String],
        body: Value,
    ) -> Result<Value, ApplyError> {
        let query = vec![
            ("update_mask".to_string(), mask.join(",")),
            ("allow_missing".to_string(), "true".to_string()),
            ("validate_only".to_string(), "true".to_string()),
        ];
        let answer: Value = wire::client(self.wire, Vec::new())
            .call(wire::patch(name, query, body))
            .await?;
        if answer.get("spec").is_some() || answer.get("meta").is_some() {
            return Ok(answer);
        }
        // A reconciled type answers with an Operation whose response is the
        // Resource it would produce.
        match answer.get("response") {
            Some(r) if r.is_object() => Ok(r.clone()),
            _ => Err(ApplyError::Unexpected(format!(
                "validate_only on {name} returned no Resource"
            ))),
        }
    }

    /// A guard every write of a Resource passes first (spec §7.2).
    fn guard(&self, live: &Value) -> Option<(Action, String)> {
        let release = annotation(live, ANN_RELEASE)?;
        match &self.opts.release_id {
            Some(current) => (release.to_ascii_lowercase() > current.to_ascii_lowercase())
                .then(|| {
                    (
                        Action::Superseded,
                        format!("written by the newer Release {release}"),
                    )
                }),
            None if !self.opts.force => Some((
                Action::Refused,
                format!(
                    "carries {ANN_RELEASE}={release}: a Release manages it; use --force to write from outside one"
                ),
            )),
            None => None,
        }
    }

    async fn plan_steps(
        &self,
        decls: &Declarations,
        previous: &[PlanItem],
    ) -> Result<(String, Vec<Step>), ApplyError> {
        let issues = check(self.reg, self.declarable, decls);
        if !issues.is_empty() {
            return Err(ApplyError::Invalid(issues));
        }
        let env = self.env().await?;
        let mut steps = Vec::new();
        for (i, d) in decls.resources.iter().enumerate() {
            steps.push(self.plan_decl(&env, i, d).await?);
        }
        for (i, r) in decls.removed.iter().enumerate() {
            steps.push(self.plan_removed(&env, i, r).await?);
        }
        let orphans = self.plan_orphans(&env, decls, previous).await?;
        steps.extend(orphans);
        Ok((env, steps))
    }

    fn step(action: Action, kind: &str, name: &str, reason: impl Into<String>, src: Src) -> Step {
        Step {
            item: PlanItem {
                action,
                kind: kind.to_string(),
                name: name.to_string(),
                diff: Vec::new(),
                reason: reason.into(),
            },
            src,
            write: None,
        }
    }

    async fn plan_decl(&self, env: &str, i: usize, d: &Declaration) -> Result<Step, ApplyError> {
        let src = Src::Decl(i);
        let rt = self
            .reg
            .resource(&d.kind)
            .ok_or_else(|| ApplyError::Unexpected(format!("unknown kind `{}`", d.kind)))?;
        let name = rt
            .name_in(env, d.name.as_deref())
            .map_err(ApplyError::Unexpected)?;
        let live = self.get_live(&name, rt).await?;
        let mask = self.managed_mask(rt);
        let keep = live
            .as_ref()
            .map(|l| reserved_labels(&l.value))
            .unwrap_or_default();

        let n = self
            .validate(&name, &mask, self.desired_body(&name, d, &keep, None))
            .await?;
        let n_hash = resource_hash(self.reg, rt, &n);
        let n_view = view_of(&managed_object(self.reg, rt, &n, Mode::Display));

        let decided = |action: Action, reason: &str, l: Option<&Live>| -> Step {
            let mut step = Self::step(action, &d.kind, &name, reason, src);
            if action.writes() || action == Action::NoOp {
                let l_view = l
                    .map(|l| view_of(&managed_object(self.reg, rt, &l.value, Mode::Display)))
                    .unwrap_or_else(|| json!({}));
                step.item.diff = json_patch(&l_view, &n_view);
            }
            if action.writes() {
                let mut write_mask = mask.clone();
                write_mask.push(format!("meta.annotations.{ANN_APPLIED}"));
                write_mask.push(format!("meta.annotations.{ANN_SOURCE}"));
                if self.opts.release_id.is_some() {
                    write_mask.push(format!("meta.annotations.{ANN_RELEASE}"));
                }
                step.write = Some(Write {
                    kind: d.kind.clone(),
                    name: name.clone(),
                    reconciled: rt.reconciled,
                    body: self.desired_body(&name, d, &keep, Some(&n_hash)),
                    mask: write_mask,
                    allow_missing: true,
                    l_etag: l.map(|l| l.etag.clone()).unwrap_or_default(),
                    l_hash: l.map(|l| resource_hash(self.reg, rt, &l.value)),
                    l_release: l
                        .and_then(|l| annotation(&l.value, ANN_RELEASE))
                        .map(str::to_string),
                });
            }
            step
        };

        let Some(l) = live else {
            return Ok(decided(Action::Create, "the Resource does not exist", None));
        };
        if let Some((action, reason)) = self.guard(&l.value) {
            return Ok(Self::step(action, &d.kind, &name, reason, src));
        }
        let l_hash = resource_hash(self.reg, rt, &l.value);
        let labels = labels_of(&l.value);
        Ok(match labels.get(MANAGED_BY).map(String::as_str) {
            Some(m) if m != MANAGER => Self::step(
                Action::Refused,
                &d.kind,
                &name,
                format!("managed by `{m}`, which apply never takes over"),
                src,
            ),
            None => {
                if l_hash == n_hash {
                    decided(
                        Action::Adopt,
                        "unmanaged; its managed fields match the declaration",
                        Some(&l),
                    )
                } else if d.import {
                    decided(
                        Action::Adopt,
                        "unmanaged; taking over with import = true",
                        Some(&l),
                    )
                } else {
                    Self::step(
                        Action::Refused,
                        &d.kind,
                        &name,
                        "exists unmanaged with different managed fields; set import = true to take it over",
                        src,
                    )
                }
            }
            Some(_) => {
                if l_hash == n_hash {
                    decided(Action::NoOp, "matches the declaration", Some(&l))
                } else if annotation(&l.value, ANN_APPLIED) != Some(l_hash.as_str()) {
                    decided(
                        Action::Drift,
                        "written after the last apply; apply puts the declaration back",
                        Some(&l),
                    )
                } else {
                    decided(Action::Update, "the declaration changed", Some(&l))
                }
            }
        })
    }

    async fn plan_removed(&self, env: &str, i: usize, r: &Removed) -> Result<Step, ApplyError> {
        let src = Src::Removed(i);
        let rt = self
            .reg
            .resource(&r.kind)
            .ok_or_else(|| ApplyError::Unexpected(format!("unknown kind `{}`", r.kind)))?;
        let name = rt
            .name_in(env, r.name.as_deref())
            .map_err(ApplyError::Unexpected)?;
        let Some(l) = self.get_live(&name, rt).await? else {
            return Ok(Self::step(
                Action::NoOp,
                &r.kind,
                &name,
                "removed, and it does not exist",
                src,
            ));
        };
        if r.destroy {
            return Ok(Self::step(
                Action::Refused,
                &r.kind,
                &name,
                "destroy is not available yet",
                src,
            ));
        }
        let labels = labels_of(&l.value);
        if labels.get(MANAGED_BY).map(String::as_str) != Some(MANAGER) {
            return Ok(Self::step(
                Action::NoOp,
                &r.kind,
                &name,
                "not managed by apply: nothing to forget",
                src,
            ));
        }
        if let Some((action, reason)) = self.guard(&l.value) {
            return Ok(Self::step(action, &r.kind, &name, reason, src));
        }
        let kept: Map<String, Value> = labels
            .into_iter()
            .filter(|(k, _)| k != MANAGED_BY)
            .map(|(k, v)| (k, Value::String(v)))
            .collect();
        let mut step = Self::step(
            Action::Forget,
            &r.kind,
            &name,
            "removed with destroy = false: the markers are removed, the Resource stays",
            src,
        );
        step.write = Some(Write {
            kind: r.kind.clone(),
            name: name.clone(),
            reconciled: rt.reconciled,
            body: json!({"name": name, "meta": {"labels": Value::Object(kept)}}),
            mask: vec![
                "meta.labels".into(),
                format!("meta.annotations.{ANN_APPLIED}"),
                format!("meta.annotations.{ANN_SOURCE}"),
                format!("meta.annotations.{ANN_RELEASE}"),
            ],
            allow_missing: false,
            l_etag: l.etag.clone(),
            l_hash: Some(resource_hash(self.reg, rt, &l.value)),
            l_release: annotation(&l.value, ANN_RELEASE).map(str::to_string),
        });
        Ok(step)
    }

    /// Managed Resources no declaration names, looked for only in the kinds
    /// the file mentions and among the names of the previous plan.
    async fn plan_orphans(
        &self,
        env: &str,
        decls: &Declarations,
        previous: &[PlanItem],
    ) -> Result<Vec<Step>, ApplyError> {
        let mut known: BTreeSet<(String, String)> = BTreeSet::new();
        let mut kinds: BTreeSet<String> = BTreeSet::new();
        for d in &decls.resources {
            kinds.insert(d.kind.clone());
            if let Some(rt) = self.reg.resource(&d.kind) {
                if let Ok(n) = rt.name_in(env, d.name.as_deref()) {
                    known.insert((d.kind.clone(), n));
                }
            }
        }
        for r in &decls.removed {
            kinds.insert(r.kind.clone());
            if let Some(rt) = self.reg.resource(&r.kind) {
                if let Ok(n) = rt.name_in(env, r.name.as_deref()) {
                    known.insert((r.kind.clone(), n));
                }
            }
        }

        let mut found: BTreeSet<(String, String)> = BTreeSet::new();
        for kind in &kinds {
            let Some(rt) = self.reg.resource(kind) else {
                continue;
            };
            if rt.is_singleton() {
                continue;
            }
            for name in self.list_managed(env, rt).await? {
                if !known.contains(&(kind.clone(), name.clone())) {
                    found.insert((kind.clone(), name));
                }
            }
        }
        for item in previous {
            let key = (item.kind.clone(), item.name.clone());
            if known.contains(&key) || found.contains(&key) {
                continue;
            }
            let Some(rt) = self.reg.resource(&item.kind) else {
                continue;
            };
            if let Some(l) = self.get_live(&item.name, rt).await? {
                if labels_of(&l.value).get(MANAGED_BY).map(String::as_str) == Some(MANAGER) {
                    found.insert(key);
                }
            }
        }
        Ok(found
            .into_iter()
            .map(|(kind, name)| {
                Self::step(
                    Action::Orphan,
                    &kind,
                    &name,
                    "managed by apply, and no declaration names it any more; nothing is written",
                    Src::Orphan,
                )
            })
            .collect())
    }

    /// The names of the Resources of `rt` carrying the apply marker.
    async fn list_managed(&self, env: &str, rt: &ResourceType) -> Result<Vec<String>, ApplyError> {
        let mut names = Vec::new();
        let mut token = String::new();
        for _ in 0..1000 {
            let mut query = vec![
                (
                    "filter".to_string(),
                    format!("meta.labels.{MANAGED_BY} = \"{MANAGER}\""),
                ),
                ("page_size".to_string(), "1000".to_string()),
            ];
            if !token.is_empty() {
                query.push(("page_token".to_string(), token.clone()));
            }
            let page: Value = wire::client(self.wire, Vec::new())
                .call(wire::get(&format!("{env}/{}", rt.collection), query))
                .await?;
            if let Some(items) = page.get(&rt.collection).and_then(Value::as_array) {
                names.extend(
                    items
                        .iter()
                        .filter_map(|i| i.get("name").and_then(Value::as_str))
                        .map(str::to_string),
                );
            }
            token = page
                .get("next_page_token")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if token.is_empty() {
                break;
            }
        }
        Ok(names)
    }

    fn finished(step: &Step) -> Applied {
        Applied {
            action: step.item.action,
            kind: step.item.kind.clone(),
            name: step.item.name.clone(),
            reason: step.item.reason.clone(),
            code: None,
            diff: step.item.diff.clone(),
            outcome: if step.item.action == Action::Superseded {
                Outcome::Superseded
            } else {
                Outcome::Unchanged
            },
        }
    }

    /// Writes one step and waits until it settles. The error carries the
    /// result row of the failed write.
    // The error is the finished response/outcome the caller returns as-is; boxing it would only move the allocation.
    #[allow(clippy::result_large_err)]
    async fn execute(
        &self,
        env: &str,
        decls: &Declarations,
        first: Step,
    ) -> Result<Applied, (Applied, ApplyError)> {
        let fail = |step: &Step, error: ApplyError| -> (Applied, ApplyError) {
            let mut applied = Self::finished(step);
            applied.outcome = Outcome::Failed;
            applied.reason = error.to_string();
            applied.code = match &error {
                ApplyError::Stalled { .. } => Some("RECONCILE_FAILED".to_string()),
                e => code_of(e).map(str::to_string),
            };
            (applied, error)
        };

        let deadline = Instant::now() + self.opts.timeout;
        let mut step = first;
        let mut replanned = false;
        let mut raced = false;
        loop {
            let Some(mut w) = step.write.clone() else {
                if step.item.action == Action::Refused {
                    let e = ApplyError::Refused(vec![format!(
                        "{}: {}",
                        step.item.name, step.item.reason
                    )]);
                    return Err(fail(&step, e));
                }
                return Ok(Self::finished(&step));
            };
            let error = match self.send_write(&w).await {
                Ok(answer) => {
                    return match self.settle(&w, answer, deadline).await {
                        Ok(outcome) => {
                            let mut applied = Self::finished(&step);
                            applied.outcome = outcome;
                            Ok(applied)
                        }
                        Err(e) => Err(fail(&step, e)),
                    };
                }
                Err(e) => e,
            };
            let code = code_of(&error).map(str::to_string);
            match code.as_deref() {
                Some("ETAG_MISMATCH") => {
                    // Status writes change the etag too, so a mismatch alone
                    // means nothing: look at what changed.
                    let Some(rt) = self.reg.resource(&w.kind) else {
                        return Err(fail(&step, error));
                    };
                    let live = match self.get_live(&w.name, rt).await {
                        Ok(l) => l,
                        Err(e) => return Err(fail(&step, e)),
                    };
                    if let Some(l) = &live {
                        if let Some((Action::Superseded, reason)) = self.guard(&l.value) {
                            let mut s = Self::step(
                                Action::Superseded,
                                &step.item.kind,
                                &step.item.name,
                                reason,
                                step.src,
                            );
                            s.item.diff = step.item.diff.clone();
                            return Ok(Self::finished(&s));
                        }
                        let unchanged = Some(resource_hash(self.reg, rt, &l.value)) == w.l_hash
                            && annotation(&l.value, ANN_RELEASE).map(str::to_string) == w.l_release;
                        if unchanged {
                            if Instant::now() >= deadline {
                                let e = ApplyError::Conflict(format!(
                                    "{}: still ETAG_MISMATCH at the timeout",
                                    w.name
                                ));
                                return Err(fail(&step, e));
                            }
                            w.l_etag = l.etag.clone();
                            step.write = Some(w);
                            tokio::time::sleep(
                                self.opts.poll_interval.min(Duration::from_millis(250)),
                            )
                            .await;
                            continue;
                        }
                    }
                    if replanned {
                        let e = ApplyError::Conflict(format!(
                            "{}: changed again while apply was writing it",
                            w.name
                        ));
                        return Err(fail(&step, e));
                    }
                    replanned = true;
                }
                Some("RESOURCE_ALREADY_EXISTS") if !raced => {
                    // A concurrent create at the same name: re-read, update.
                    raced = true;
                }
                _ => return Err(fail(&step, error)),
            }
            step = match self.replan(env, decls, &step).await {
                Ok(s) => s,
                Err(e) => return Err(fail(&step, e)),
            };
        }
    }

    async fn replan(
        &self,
        env: &str,
        decls: &Declarations,
        step: &Step,
    ) -> Result<Step, ApplyError> {
        match step.src {
            Src::Decl(i) => self.plan_decl(env, i, &decls.resources[i]).await,
            Src::Removed(i) => self.plan_removed(env, i, &decls.removed[i]).await,
            Src::Orphan => Ok(step.clone()),
        }
    }

    async fn send_write(&self, w: &Write) -> Result<Value, ApplyError> {
        let mut query = vec![("update_mask".to_string(), w.mask.join(","))];
        if w.allow_missing {
            query.push(("allow_missing".to_string(), "true".to_string()));
        }
        // sha256(run id ‖ name ‖ etag of L): a retried Job of the same
        // Release replays its answers; another Release never collides.
        let key = to_hex(&Sha256::digest(
            format!("{}{}{}", self.run_id, w.name, w.l_etag).as_bytes(),
        ));
        let mut headers = vec![("Idempotency-Key".to_string(), key)];
        if !w.l_etag.is_empty() {
            headers.push(("If-Match".to_string(), w.l_etag.clone()));
        }
        let answer: Value = wire::client(self.wire, headers)
            .call(wire::patch(&w.name, query, w.body.clone()))
            .await?;
        Ok(answer)
    }

    /// Waits for a write to settle. Still unsettled at the deadline is an
    /// outcome, not a failure; `Stalled` is a failure.
    async fn settle(
        &self,
        w: &Write,
        answer: Value,
        deadline: Instant,
    ) -> Result<Outcome, ApplyError> {
        let stalled = |message: String| ApplyError::Stalled {
            name: w.name.clone(),
            message,
        };
        if is_operation(&answer) {
            let op: sylphx::common::Operation = serde_json::from_value(answer)
                .map_err(|e| ApplyError::Unexpected(format!("operation: {e}")))?;
            let client = wire::client(self.wire, Vec::new());
            let left = deadline.saturating_duration_since(Instant::now());
            let waited = tokio::time::timeout(left, async {
                let result: Result<Value, Error> = sylphx::Operation::new(&client, op).wait().await;
                result
            })
            .await;
            return match waited {
                Err(_) => Ok(Outcome::Unsettled),
                Ok(Ok(_)) => Ok(Outcome::Written),
                Ok(Err(Error::Api { code, detail, .. })) if code.as_str() == "RECONCILE_FAILED" => {
                    Err(stalled(detail))
                }
                Ok(Err(e)) => Err(e.into()),
            };
        }
        let mut current = answer;
        loop {
            match settled(&current, w.reconciled) {
                Settled::Yes => return Ok(Outcome::Written),
                Settled::Stalled(message) => return Err(stalled(message)),
                Settled::Pending => {}
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(Outcome::Unsettled);
            }
            tokio::time::sleep(self.opts.poll_interval.min(left)).await;
            let rt = self.reg.resource(&w.kind);
            current = match rt {
                Some(rt) => match self.get_live(&w.name, rt).await? {
                    Some(l) => l.value,
                    None => {
                        return Err(ApplyError::Unexpected(format!(
                            "{} vanished while waiting for it to settle",
                            w.name
                        )))
                    }
                },
                None => return Ok(Outcome::Written),
            };
        }
    }
}
