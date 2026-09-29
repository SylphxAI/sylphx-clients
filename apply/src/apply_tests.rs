//! The planner and the writer against the in-memory Resource API in
//! `fake.rs`, over the fixture registry's `things` (slug-named, reconciled)
//! and `settings` (a singleton).

use std::time::Duration;

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::apply::{Action, Applier, Options, Outcome, Plan, PlanItem, Report};
use crate::check::fixtures;
use crate::declare::Declarations;
use crate::error::ApplyError;
use crate::fake::Fake;
use crate::hash::to_hex;

const ENV: &str = "orgs/o/projects/p/envs/e";
const R1: &str = "01900000-0000-7000-8000-000000000001";
const R2: &str = "01900000-0000-7000-8000-000000000002";
const THING: &str = "demo.sylphx.com/Thing";

fn opts(release: Option<&str>) -> Options {
    Options {
        release_id: release.map(str::to_string),
        source: "repo@abc:sylphx.toml".into(),
        force: false,
        timeout: Duration::from_millis(300),
        poll_interval: Duration::from_millis(5),
        env: Some(ENV.into()),
    }
}

fn name() -> String {
    format!("{ENV}/things/main")
}

fn thing(size: &str) -> Declarations {
    thing_with(size, "")
}

fn thing_with(size: &str, extra: &str) -> Declarations {
    Declarations::from_toml(&format!(
        "[[resource]]\nkind = \"{THING}\"\nname = \"main\"\n{extra}\n[resource.spec]\nsize = \"{size}\"\n"
    ))
    .unwrap()
}

fn settings(amount: &str) -> Declarations {
    Declarations::from_toml(&format!(
        "[[resource]]\nkind = \"demo.sylphx.com/Settings\"\n[resource.spec]\ncurrency = \"usd\"\n[[resource.spec.plans]]\nkey = \"plus\"\namount = \"{amount}\"\n"
    ))
    .unwrap()
}

async fn plan(fake: &Fake, d: &Declarations, o: Options) -> Plan {
    let (reg, dec) = (fixtures::registry(), fixtures::declarable());
    Applier::new(&reg, &dec, fake, o)
        .plan(d, &[])
        .await
        .unwrap()
}

async fn run(fake: &Fake, d: &Declarations, o: Options) -> Report {
    let (reg, dec) = (fixtures::registry(), fixtures::declarable());
    Applier::new(&reg, &dec, fake, o)
        .apply(d, &[])
        .await
        .unwrap()
}

fn live(fake: &Fake, name: &str) -> Value {
    fake.read(name).expect("the Resource exists")
}

fn edit_directly(fake: &Fake, name: &str, field: &str, value: Value) {
    let mut st = fake.state.lock().unwrap();
    let r = st.res.get_mut(name).unwrap();
    r.spec.insert(field.to_string(), value);
    r.generation += 1;
    r.observed = r.generation;
    r.revision += 1;
}

#[tokio::test]
async fn create_then_no_op_writes_nothing() {
    let fake = Fake::new();
    let d = thing("10");

    let p = plan(&fake, &d, opts(Some(R1))).await;
    assert_eq!(p.items.len(), 1);
    assert_eq!(p.items[0].action, Action::Create);
    assert_eq!(p.items[0].name, name());
    assert_eq!(p.items[0].diff[0].path, "/spec");
    assert_eq!(p.items[0].diff[0].value.as_ref().unwrap()["size"], "10");
    assert!(fake.writes().is_empty(), "a plan writes nothing");

    let r = run(&fake, &d, opts(Some(R1))).await;
    assert!(!r.failed());
    assert_eq!(r.resources[0].outcome, Outcome::Written);
    assert_eq!(r.summary(), "applied: 1 created");

    let l = live(&fake, &name());
    assert_eq!(l["meta"]["labels"]["sylphx-managed-by"], "apply");
    assert_eq!(l["meta"]["annotations"]["sylphx-apply-release"], R1);
    assert_eq!(
        l["meta"]["annotations"]["sylphx-apply-source"],
        "repo@abc:sylphx.toml"
    );
    assert_eq!(
        l["meta"]["annotations"]["sylphx-applied"]
            .as_str()
            .unwrap()
            .len(),
        32
    );

    // The next Release: the server default `mode` does not show as a change.
    let writes = fake.writes().len();
    let p2 = plan(&fake, &d, opts(Some(R2))).await;
    assert_eq!(p2.items[0].action, Action::NoOp);
    assert!(p2.items[0].diff.is_empty());
    assert!(!p2.has_changes());
    let r2 = run(&fake, &d, opts(Some(R2))).await;
    assert_eq!(r2.summary(), "applied: no changes");
    assert_eq!(fake.writes().len(), writes, "a no-op writes no markers");
    assert_eq!(
        live(&fake, &name())["meta"]["annotations"]["sylphx-apply-release"],
        R1
    );
}

#[tokio::test]
async fn update_sends_the_mask_if_match_and_a_derived_idempotency_key() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;
    let etag = live(&fake, &name())["meta"]["etag"]
        .as_str()
        .unwrap()
        .to_string();

    let p = plan(&fake, &thing("20"), opts(Some(R2))).await;
    assert_eq!(p.items[0].action, Action::Update);
    assert!(p.items[0]
        .diff
        .iter()
        .any(|o| o.op == "replace" && o.path == "/spec/size"));

    let r = run(&fake, &thing("20"), opts(Some(R2))).await;
    assert_eq!(r.summary(), "applied: 1 updated");
    assert_eq!(live(&fake, &name())["spec"]["size"], "20");

    let writes = fake.writes();
    let w = writes.last().unwrap();
    let mask: Vec<&str> = w.q("update_mask").unwrap().split(',').collect();
    for want in [
        "spec.size",
        "spec.mode",
        "spec.note",
        "spec.tags",
        "spec.limits",
        "meta.labels",
        "meta.display_name",
        "meta.annotations.sylphx-applied",
        "meta.annotations.sylphx-apply-source",
        "meta.annotations.sylphx-apply-release",
    ] {
        assert!(mask.contains(&want), "{want} in {mask:?}");
    }
    assert!(!mask
        .iter()
        .any(|m| m.contains("password") || m.contains("token")));
    assert_eq!(w.q("allow_missing"), Some("true"));
    let body = w.body.as_ref().unwrap();
    assert_eq!(body["meta"]["labels"]["sylphx-managed-by"], "apply");
    assert!(body["spec"].get("token").is_none());
    assert_eq!(w.h("if-match"), Some(etag.as_str()));
    let key = to_hex(&Sha256::digest(format!("{R2}{}{etag}", name()).as_bytes()));
    assert_eq!(w.h("idempotency-key"), Some(key.as_str()));
}

#[tokio::test]
async fn a_direct_write_is_drift_and_is_put_back() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;
    edit_directly(&fake, &name(), "size", json!("99"));

    let p = plan(&fake, &thing("10"), opts(Some(R2))).await;
    assert_eq!(p.items[0].action, Action::Drift);
    assert!(p.has_changes());

    let r = run(&fake, &thing("10"), opts(Some(R2))).await;
    assert_eq!(r.summary(), "applied: 1 drift put back");
    assert_eq!(live(&fake, &name())["spec"]["size"], "10");
}

#[tokio::test]
async fn adopting_an_unmanaged_resource() {
    // Matching managed fields: adopt with nothing else needed.
    let fake = Fake::new();
    fake.seed(
        &name(),
        json!({"size": "10", "mode": "safe"}),
        json!({}),
        json!({}),
    );
    let p = plan(&fake, &thing("10"), opts(Some(R1))).await;
    assert_eq!(p.items[0].action, Action::Adopt);
    let r = run(&fake, &thing("10"), opts(Some(R1))).await;
    assert!(!r.failed());
    assert_eq!(
        live(&fake, &name())["meta"]["labels"]["sylphx-managed-by"],
        "apply"
    );

    // Different managed fields: refused, nothing written, unless import.
    let fake = Fake::new();
    fake.seed(
        &name(),
        json!({"size": "5", "mode": "safe"}),
        json!({}),
        json!({}),
    );
    let p = plan(&fake, &thing("10"), opts(Some(R1))).await;
    assert_eq!(p.items[0].action, Action::Refused);
    assert!(p.items[0].reason.contains("import = true"));
    let r = run(&fake, &thing("10"), opts(Some(R1))).await;
    assert!(r.failed());
    assert!(matches!(r.error, Some(ApplyError::Refused(_))));
    assert!(fake.writes().is_empty());

    let d = thing_with("10", "import = true");
    let p = plan(&fake, &d, opts(Some(R1))).await;
    assert_eq!(p.items[0].action, Action::Adopt);
    let r = run(&fake, &d, opts(Some(R1))).await;
    assert!(!r.failed());
    assert_eq!(live(&fake, &name())["spec"]["size"], "10");
}

#[tokio::test]
async fn another_manager_is_never_taken_over() {
    let fake = Fake::new();
    fake.seed(
        &name(),
        json!({"size": "10", "mode": "safe"}),
        json!({"sylphx-managed-by": "terraform"}),
        json!({}),
    );
    let d = thing_with("10", "import = true");
    let p = plan(&fake, &d, opts(Some(R1))).await;
    assert_eq!(p.items[0].action, Action::Refused);
    assert!(p.items[0].reason.contains("terraform"));
}

#[tokio::test]
async fn a_newer_release_supersedes_this_one() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R2))).await;
    let writes = fake.writes().len();

    let p = plan(&fake, &thing("20"), opts(Some(R1))).await;
    assert_eq!(p.items[0].action, Action::Superseded);
    let r = run(&fake, &thing("20"), opts(Some(R1))).await;
    assert!(!r.failed());
    assert_eq!(r.resources[0].outcome, Outcome::Superseded);
    assert_eq!(fake.writes().len(), writes);
    assert_eq!(live(&fake, &name())["spec"]["size"], "10");
}

#[tokio::test]
async fn outside_a_release_a_released_resource_needs_force_and_keeps_its_marker() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;

    let p = plan(&fake, &thing("20"), opts(None)).await;
    assert_eq!(p.items[0].action, Action::Refused);
    assert!(p.items[0].reason.contains("--force"));

    let mut forced = opts(None);
    forced.force = true;
    let r = run(&fake, &thing("20"), forced).await;
    assert!(!r.failed());
    assert_eq!(live(&fake, &name())["spec"]["size"], "20");
    assert_eq!(
        live(&fake, &name())["meta"]["annotations"]["sylphx-apply-release"],
        R1,
        "a run outside a Release leaves the annotation unchanged"
    );
    let w = fake.writes();
    assert!(!w
        .last()
        .unwrap()
        .q("update_mask")
        .unwrap()
        .contains("sylphx-apply-release"));

    // Outside a Release, a Resource no Release wrote is fine, and the run
    // writes no release annotation at all.
    let fake = Fake::new();
    let r = run(&fake, &thing("10"), opts(None)).await;
    assert!(!r.failed());
    assert!(live(&fake, &name())["meta"]["annotations"]
        .get("sylphx-apply-release")
        .is_none());
}

fn seed_managed(fake: &Fake, n: &str) {
    fake.seed(
        n,
        json!({"size": "1", "mode": "safe"}),
        json!({"sylphx-managed-by": "apply", "team": "a"}),
        json!({"sylphx-applied": "x", "sylphx-apply-release": R1, "sylphx-apply-source": "s"}),
    );
}

#[tokio::test]
async fn orphans_are_reported_only_and_forget_removes_the_markers() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;
    let old = format!("{ENV}/things/old");
    seed_managed(&fake, &old);

    let p = plan(&fake, &thing("10"), opts(Some(R2))).await;
    assert_eq!(p.items.len(), 2);
    assert_eq!(p.items[0].action, Action::NoOp);
    assert_eq!(p.items[1].action, Action::Orphan);
    assert_eq!(p.items[1].name, old);
    assert!(!p.has_changes());
    let writes = fake.writes().len();
    let r = run(&fake, &thing("10"), opts(Some(R2))).await;
    assert_eq!(r.summary(), "applied: 1 orphan");
    assert_eq!(fake.writes().len(), writes, "an orphan is not written");

    // [[removed]] destroy = false: forget.
    let removed = format!(
        "{}\n[[removed]]\nkind = \"{THING}\"\nname = \"old\"\n",
        "[[resource]]\nkind = \"demo.sylphx.com/Thing\"\nname = \"main\"\n[resource.spec]\nsize = \"10\"\n"
    );
    let d = Declarations::from_toml(&removed).unwrap();
    let p = plan(&fake, &d, opts(Some(R2))).await;
    assert_eq!(p.items[1].action, Action::Forget);
    let r = run(&fake, &d, opts(Some(R2))).await;
    assert!(!r.failed());
    let l = live(&fake, &old);
    assert!(l["meta"]["labels"].get("sylphx-managed-by").is_none());
    assert_eq!(l["meta"]["labels"]["team"], "a");
    for key in [
        "sylphx-applied",
        "sylphx-apply-source",
        "sylphx-apply-release",
    ] {
        assert!(l["meta"]["annotations"].get(key).is_none(), "{key}");
    }
    assert_eq!(l["spec"]["size"], "1", "forget leaves the spec alone");
    let last = fake.writes().last().cloned().unwrap();
    assert_eq!(last.q("allow_missing"), None);

    // Forgetting again, or forgetting what does not exist: no-op.
    let p = plan(&fake, &d, opts(Some(R2))).await;
    assert_eq!(p.items[1].action, Action::NoOp);
    let gone = Declarations::from_toml(&format!(
        "[[removed]]\nkind = \"{THING}\"\nname = \"never\"\n"
    ))
    .unwrap();
    let p = plan(&fake, &gone, opts(Some(R2))).await;
    assert_eq!(p.items[0].action, Action::NoOp);
}

#[tokio::test]
async fn destroy_is_refused_for_now() {
    let fake = Fake::new();
    let old = format!("{ENV}/things/old");
    seed_managed(&fake, &old);
    let d = Declarations::from_toml(&format!(
        "[[removed]]\nkind = \"{THING}\"\nname = \"old\"\ndestroy = true\n"
    ))
    .unwrap();
    let p = plan(&fake, &d, opts(Some(R2))).await;
    assert_eq!(p.items[0].action, Action::Refused);
    assert_eq!(p.items[0].reason, "destroy is not available yet");
    let r = run(&fake, &d, opts(Some(R2))).await;
    assert!(r.failed());
    assert!(fake.writes().is_empty());
}

#[tokio::test]
async fn the_previous_plan_finds_orphans_of_kinds_no_longer_in_the_file() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;
    let previous = vec![PlanItem {
        action: Action::Create,
        kind: THING.into(),
        name: name(),
        diff: Vec::new(),
        reason: String::new(),
    }];
    let (reg, dec) = (fixtures::registry(), fixtures::declarable());
    let p = Applier::new(&reg, &dec, &fake, opts(Some(R2)))
        .plan(&settings("500"), &previous)
        .await
        .unwrap();
    let orphans: Vec<&str> = p
        .items
        .iter()
        .filter(|i| i.action == Action::Orphan)
        .map(|i| i.name.as_str())
        .collect();
    assert_eq!(orphans, [name().as_str()]);
}

#[tokio::test]
async fn a_singleton_has_no_name_and_no_etag_before_its_first_write() {
    let fake = Fake::new();
    let singleton = format!("{ENV}/settings");
    let p = plan(&fake, &settings("500"), opts(Some(R1))).await;
    assert_eq!(p.items[0].action, Action::Create);
    assert_eq!(p.items[0].name, singleton);

    let r = run(&fake, &settings("500"), opts(Some(R1))).await;
    assert!(!r.failed());
    let first = fake.writes().last().cloned().unwrap();
    assert_eq!(first.path, format!("/v1/{singleton}"));
    assert_eq!(first.h("if-match"), None, "a create sends no If-Match");
    assert_eq!(live(&fake, &singleton)["spec"]["currency"], "usd");

    let p = plan(&fake, &settings("500"), opts(Some(R2))).await;
    assert_eq!(p.items[0].action, Action::NoOp);

    let p = plan(&fake, &settings("900"), opts(Some(R2))).await;
    assert_eq!(p.items[0].action, Action::Update);
    run(&fake, &settings("900"), opts(Some(R2))).await;
    let second = fake.writes().last().cloned().unwrap();
    assert!(second.h("if-match").is_some());
    assert_eq!(live(&fake, &singleton)["spec"]["plans"][0]["amount"], "900");
}

#[tokio::test]
async fn a_status_write_alone_is_retried_with_the_new_etag() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;
    let before = fake.writes().len();
    fake.state.lock().unwrap().bump_status = true;

    let r = run(&fake, &thing("20"), opts(Some(R2))).await;
    assert!(!r.failed(), "{:?}", r.error);
    assert_eq!(live(&fake, &name())["spec"]["size"], "20");
    let writes = fake.writes();
    assert_eq!(writes.len() - before, 2);
    let (first, second) = (&writes[before], &writes[before + 1]);
    assert_ne!(first.h("if-match"), second.h("if-match"));
    assert_ne!(first.h("idempotency-key"), second.h("idempotency-key"));
}

#[tokio::test]
async fn a_changed_managed_field_is_planned_again_once() {
    let fake = Fake::new();
    run(&fake, &thing("10"), opts(Some(R1))).await;
    let before = fake.writes().len();
    fake.state.lock().unwrap().edit_before_patch = Some((name(), "size".into(), json!("99")));

    let r = run(&fake, &thing("20"), opts(Some(R2))).await;
    assert!(!r.failed(), "{:?}", r.error);
    assert_eq!(r.resources[0].action, Action::Drift, "the edit is put back");
    assert_eq!(live(&fake, &name())["spec"]["size"], "20");
    assert_eq!(fake.writes().len() - before, 2);
}

#[tokio::test]
async fn a_create_race_re_reads_and_updates() {
    let fake = Fake::new();
    fake.state.lock().unwrap().race_create = true;
    let d = thing_with("10", "import = true");
    let r = run(&fake, &d, opts(Some(R1))).await;
    assert!(!r.failed(), "{:?}", r.error);
    assert_eq!(r.resources[0].action, Action::Adopt);
    assert_eq!(live(&fake, &name())["spec"]["size"], "10");

    // Without import the other writer's Resource is refused.
    let fake = Fake::new();
    fake.state.lock().unwrap().race_create = true;
    let r = run(&fake, &thing("10"), opts(Some(R1))).await;
    assert!(r.failed());
    assert!(matches!(r.error, Some(ApplyError::Refused(_))));
}

#[tokio::test]
async fn unsettled_at_the_timeout_is_a_result_not_an_error() {
    let fake = Fake::new();
    fake.state.lock().unwrap().auto_settle = false;
    let mut o = opts(Some(R1));
    o.timeout = Duration::from_millis(60);
    let r = run(&fake, &thing("10"), o).await;
    assert!(!r.failed());
    assert_eq!(r.resources[0].outcome, Outcome::Unsettled);
    assert_eq!(r.unsettled, [name()]);
    let json: Value = serde_json::from_str(&r.to_json()).unwrap();
    assert_eq!(json["unsettled"][0], name());
}

#[tokio::test]
async fn stalled_fails_the_run_with_a_code() {
    let fake = Fake::new();
    fake.state.lock().unwrap().stall = true;
    let r = run(&fake, &thing("10"), opts(Some(R1))).await;
    assert!(r.failed());
    assert!(matches!(r.error, Some(ApplyError::Stalled { .. })));
    assert_eq!(r.resources[0].code.as_deref(), Some("RECONCILE_FAILED"));
    assert_eq!(r.resources[0].outcome, Outcome::Failed);
}

#[tokio::test]
async fn an_operation_is_awaited() {
    let fake = Fake::new();
    fake.state.lock().unwrap().operations_mode = true;
    let r = run(&fake, &thing("10"), opts(Some(R1))).await;
    assert!(!r.failed(), "{:?}", r.error);
    assert_eq!(r.resources[0].outcome, Outcome::Written);
}

#[tokio::test]
async fn the_environment_comes_from_whoami() {
    let fake = Fake::new();
    let mut o = opts(Some(R1));
    o.env = None;
    let p = plan(&fake, &thing("10"), o).await;
    assert_eq!(p.items[0].name, name());
}

#[tokio::test]
async fn the_result_document_has_the_release_contract_shape() {
    let fake = Fake::new();
    let p = plan(&fake, &thing("10"), opts(Some(R1))).await;
    let v: Value = serde_json::from_str(&p.report().to_json()).unwrap();
    let item = &v["resources"][0];
    assert_eq!(item["action"], "create");
    assert_eq!(item["kind"], THING);
    assert_eq!(item["name"], name());
    assert!(item["reason"].is_string());
    assert!(item["code"].is_null());
    assert!(item["diff"].is_array());
    assert_eq!(v["unsettled"], json!([]));

    let same = plan(&fake, &thing("10"), opts(Some(R1))).await;
    let noop = same.report();
    assert_eq!(noop.resources.len(), 1);

    fake.seed(
        &name(),
        json!({"size": "10", "mode": "safe"}),
        json!({"sylphx-managed-by": "apply"}),
        json!({}),
    );
    let p = plan(&fake, &thing("10"), opts(Some(R1))).await;
    let v: Value = serde_json::from_str(&p.report().to_json()).unwrap();
    assert_eq!(v["resources"][0]["action"], "no-op");
}

#[tokio::test]
async fn an_invalid_file_is_not_planned() {
    let fake = Fake::new();
    let d = Declarations::from_toml(&format!(
        "[[resource]]\nkind = \"{THING}\"\nname = \"main\"\n[resource.spec]\nsize = \"1\"\nbogus = 1\n"
    ))
    .unwrap();
    let (reg, dec) = (fixtures::registry(), fixtures::declarable());
    let e = Applier::new(&reg, &dec, &fake, opts(Some(R1)))
        .plan(&d, &[])
        .await
        .unwrap_err();
    assert!(matches!(e, ApplyError::Invalid(_)));
    assert!(fake.state.lock().unwrap().log.is_empty());
}
