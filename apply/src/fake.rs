//! A small in-memory Resource API for the tests: the standard methods over
//! the fixture registry's two kinds (`things`, reconciled and named by slug,
//! and the `settings` singleton), with `update_mask`, `allow_missing`,
//! `validate_only`, `If-Match`, `Idempotency-Key`, server defaults, status
//! that lags a write until the next read, and switches to provoke races.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde_json::{json, Map, Value};
use sylphx::{Error, HttpRequest, HttpResponse};

use crate::wire::Wire;

#[derive(Clone, Debug, Default)]
pub struct Res {
    pub spec: Map<String, Value>,
    pub labels: Map<String, Value>,
    pub annotations: Map<String, Value>,
    pub display_name: String,
    pub generation: i64,
    pub revision: i64,
    pub observed: i64,
}

#[derive(Clone, Debug)]
pub struct Logged {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Option<Value>,
}

impl Logged {
    pub fn q(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn h(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    /// A PATCH that is not a validate-only call.
    pub fn is_write(&self) -> bool {
        self.method == "PATCH" && self.q("validate_only") != Some("true")
    }
}

#[derive(Default)]
pub struct State {
    pub res: BTreeMap<String, Res>,
    pub log: Vec<Logged>,
    idem: HashMap<String, (u16, Vec<u8>)>,
    operations: HashMap<String, String>,
    /// A read lets the controller catch up with the generation.
    pub auto_settle: bool,
    /// Every read reports `Stalled=TRUE`.
    pub stall: bool,
    /// Writes return an Operation, settled by `:wait`.
    pub operations_mode: bool,
    /// Before the next write: a status write bumps the etag.
    pub bump_status: bool,
    /// Before the next write: somebody edits `spec.<field>` directly.
    pub edit_before_patch: Option<(String, String, Value)>,
    /// Before the next create: somebody else creates the name first.
    pub race_create: bool,
}

pub struct Fake {
    pub state: Mutex<State>,
}

fn is_singleton(name: &str) -> bool {
    name.ends_with("/settings")
}

fn is_reconciled(name: &str) -> bool {
    name.contains("/things/")
}

fn etag(r: &Res) -> String {
    format!("\"r{}\"", r.revision)
}

fn ok(v: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        request_id: None,
        body: serde_json::to_vec(&v).unwrap(),
    }
}

fn problem(status: u16, code: &str, detail: &str) -> HttpResponse {
    let grpc = match status {
        404 => "NOT_FOUND",
        409 if code == "ETAG_MISMATCH" => "ABORTED",
        409 => "ALREADY_EXISTS",
        _ => "INVALID_ARGUMENT",
    };
    let body = json!({
        "type": "about:blank", "title": detail, "status": status, "detail": detail,
        "code": code, "grpc_status": grpc, "retryable": false,
    });
    HttpResponse {
        status,
        request_id: None,
        body: serde_json::to_vec(&body).unwrap(),
    }
}

impl Fake {
    pub fn new() -> Fake {
        Fake {
            state: Mutex::new(State {
                auto_settle: true,
                ..State::default()
            }),
        }
    }

    /// Puts a Resource in the store directly (someone else wrote it).
    pub fn seed(&self, name: &str, spec: Value, labels: Value, annotations: Value) {
        let mut st = self.state.lock().unwrap();
        st.res.insert(
            name.to_string(),
            Res {
                spec: spec.as_object().cloned().unwrap_or_default(),
                labels: labels.as_object().cloned().unwrap_or_default(),
                annotations: annotations.as_object().cloned().unwrap_or_default(),
                generation: 1,
                revision: 1,
                observed: 1,
                ..Res::default()
            },
        );
    }

    /// The stored Resource as a client would read it, without the
    /// controller catching up.
    pub fn read(&self, name: &str) -> Option<Value> {
        let st = self.state.lock().unwrap();
        st.res.get(name).map(|r| render(name, r))
    }

    pub fn writes(&self) -> Vec<Logged> {
        let st = self.state.lock().unwrap();
        st.log.iter().filter(|l| l.is_write()).cloned().collect()
    }

    fn handle(&self, request: &HttpRequest, headers: &[(String, String)]) -> HttpResponse {
        let mut st = self.state.lock().unwrap();
        st.log.push(Logged {
            method: request.method.to_string(),
            path: request.path.clone(),
            query: request.query.clone(),
            headers: headers.to_vec(),
            body: request.body.clone(),
        });
        let rest = request.path.trim_start_matches("/v1/").to_string();
        match request.method {
            "GET" if rest == "whoami" => ok(json!({
                "org": "orgs/o", "project": "orgs/o/projects/p", "env": "orgs/o/projects/p/envs/e"
            })),
            "GET" if rest.ends_with("/things") => {
                let prefix = format!("{rest}/");
                let items: Vec<Value> = st
                    .res
                    .iter()
                    .filter(|(n, r)| {
                        n.starts_with(&prefix)
                            && r.labels.get("sylphx-managed-by") == Some(&json!("apply"))
                    })
                    .map(|(n, r)| render(n, r))
                    .collect();
                ok(json!({"things": items}))
            }
            "GET" => {
                let settle = st.auto_settle;
                let stall = st.stall;
                if let Some(r) = st.res.get_mut(&rest) {
                    if settle {
                        r.observed = r.generation;
                    }
                }
                match st.res.get(&rest) {
                    Some(r) => {
                        let mut v = render(&rest, r);
                        if stall {
                            stall_status(&mut v);
                        }
                        ok(v)
                    }
                    None if is_singleton(&rest) => ok(json!({"name": rest, "spec": {}})),
                    None => problem(404, "RESOURCE_NOT_FOUND", "no such resource"),
                }
            }
            "POST" if rest.ends_with(":wait") => {
                let op = rest.trim_end_matches(":wait").to_string();
                let target = st.operations.get(&op).cloned().unwrap_or_default();
                if let Some(r) = st.res.get_mut(&target) {
                    r.observed = r.generation;
                }
                let resource = st
                    .res
                    .get(&target)
                    .map(|r| render(&target, r))
                    .unwrap_or_else(|| json!({}));
                let mut response = resource;
                response["@type"] = json!("type.sylphx.com/Resource");
                ok(json!({"name": op, "target": target, "done": true, "response": response}))
            }
            "PATCH" => self.patch(&mut st, &rest, request, headers),
            _ => problem(404, "NOT_FOUND", "no such method"),
        }
    }

    fn patch(
        &self,
        st: &mut State,
        name: &str,
        request: &HttpRequest,
        headers: &[(String, String)],
    ) -> HttpResponse {
        let q = |k: &str| {
            request
                .query
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        let header = |k: &str| {
            headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.clone())
        };
        let validate_only = q("validate_only") == Some("true");
        let allow_missing = q("allow_missing") == Some("true");
        let mask: Vec<String> = q("update_mask")
            .unwrap_or_default()
            .split(',')
            .filter(|p| !p.is_empty())
            .map(str::to_string)
            .collect();
        let body = request.body.clone().unwrap_or_else(|| json!({}));

        if !validate_only {
            if let Some(key) = header("idempotency-key") {
                if let Some((status, bytes)) = st.idem.get(&key) {
                    return HttpResponse {
                        status: *status,
                        request_id: None,
                        body: bytes.clone(),
                    };
                }
            }
            if st.bump_status {
                st.bump_status = false;
                if let Some(r) = st.res.get_mut(name) {
                    r.revision += 1;
                }
            }
            if let Some((n, field, value)) = st.edit_before_patch.take() {
                if let Some(r) = st.res.get_mut(&n) {
                    r.spec.insert(field, value);
                    r.generation += 1;
                    r.revision += 1;
                }
            }
        }

        let exists = st.res.contains_key(name);
        if !exists && st.race_create && !validate_only && allow_missing {
            st.race_create = false;
            st.res.insert(
                name.to_string(),
                Res {
                    generation: 1,
                    revision: 1,
                    observed: 1,
                    ..Res::default()
                },
            );
            return problem(409, "RESOURCE_ALREADY_EXISTS", "created by someone else");
        }
        if !exists && !allow_missing {
            return problem(404, "RESOURCE_NOT_FOUND", "no such resource");
        }
        let mut res = st.res.get(name).cloned().unwrap_or_default();
        if exists && !validate_only {
            if let Some(sent) = header("if-match") {
                if sent != etag(&res) {
                    return problem(409, "ETAG_MISMATCH", "the resource changed");
                }
            }
        }

        let before = res.spec.clone();
        for path in &mask {
            if let Some(field) = path.strip_prefix("spec.") {
                match body.pointer("/spec").and_then(|s| s.get(field)) {
                    Some(v) => {
                        res.spec.insert(field.to_string(), v.clone());
                    }
                    None => {
                        res.spec.remove(field);
                    }
                }
            } else if path == "meta.labels" {
                res.labels = body
                    .pointer("/meta/labels")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
            } else if path == "meta.display_name" {
                res.display_name = body
                    .pointer("/meta/display_name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            } else if let Some(key) = path.strip_prefix("meta.annotations.") {
                match body.pointer("/meta/annotations").and_then(|a| a.get(key)) {
                    Some(v) => {
                        res.annotations.insert(key.to_string(), v.clone());
                    }
                    None => {
                        res.annotations.remove(key);
                    }
                }
            }
        }
        // A server default: `mode` is `safe` unless set.
        if is_reconciled(name) && !res.spec.contains_key("mode") {
            res.spec.insert("mode".into(), json!("safe"));
        }
        if !exists {
            res.generation = 1;
        } else if res.spec != before {
            res.generation += 1;
        }
        res.revision += 1;

        if validate_only {
            return ok(render(name, &res));
        }
        st.res.insert(name.to_string(), res.clone());
        let response = if st.operations_mode && is_reconciled(name) {
            let op = format!(
                "orgs/o/projects/p/envs/e/operations/op{}",
                st.operations.len() + 1
            );
            st.operations.insert(op.clone(), name.to_string());
            json!({"name": op, "target": name, "target_generation": res.generation.to_string(), "done": false})
        } else {
            render(name, &res)
        };
        let bytes = serde_json::to_vec(&response).unwrap();
        if let Some(key) = header("idempotency-key") {
            st.idem.insert(key, (200, bytes.clone()));
        }
        HttpResponse {
            status: 200,
            request_id: None,
            body: bytes,
        }
    }
}

fn stall_status(v: &mut Value) {
    v["status"]["conditions"] =
        json!([{"type": "Stalled", "status": "true", "message": "the controller gave up"}]);
}

/// A Resource as the API returns it. A singleton that was never written has
/// no etag.
fn render(name: &str, r: &Res) -> Value {
    let mut meta = json!({
        "generation": r.generation.to_string(),
        "etag": etag(r),
        "labels": r.labels,
        "annotations": r.annotations,
    });
    if !r.display_name.is_empty() {
        meta["display_name"] = json!(r.display_name);
    }
    let mut v = json!({"name": name, "meta": meta, "spec": r.spec});
    if is_reconciled(name) {
        let mut conditions = Vec::new();
        if r.observed != r.generation {
            conditions.push(json!({"type": "Reconciling", "status": "true"}));
        }
        v["status"] = json!({
            "observed_generation": r.observed.to_string(),
            "conditions": conditions,
        });
    }
    v
}

impl Wire for Fake {
    async fn send(
        &self,
        request: HttpRequest,
        headers: Vec<(String, String)>,
    ) -> Result<HttpResponse, Error> {
        Ok(self.handle(&request, &headers))
    }
}
