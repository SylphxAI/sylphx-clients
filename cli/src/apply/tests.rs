use std::sync::Mutex;

use serde_json::{json, Value};
use sylphx::{Error, HttpRequest, HttpResponse};
use sylphx_apply::{Action, Wire};

use super::*;

// No production kind has completed its writer cutover yet. Exercise the CLI
// with an eligible fixture registry, without admitting a production kind.
fn registry() -> Registry {
    Registry::from_json(r#"{
      "resources": [{"type":"demo.sylphx.com/Settings","service":"demo","collection":"settings",
        "message":"demo.Settings","pattern":"orgs/{org}/projects/{project}/envs/{env}/settings",
        "parent_pattern":"orgs/{org}/projects/{project}/envs/{env}","shape":"singleton","reconciled":false}],
      "messages": [
        {"name":"demo.Settings","fields":[
          {"name":"name","behaviors":["IDENTIFIER"],"type":{"kind":"scalar","scalar":"string"}},
          {"name":"meta","type":{"kind":"message","name":"demo.Meta"}},
          {"name":"spec","type":{"kind":"message","name":"demo.Spec"}}]},
        {"name":"demo.Spec","fields":[
          {"name":"currency","behaviors":["REQUIRED"],"type":{"kind":"scalar","scalar":"string"}}]},
        {"name":"demo.Meta","fields":[]}],
      "services":[{"collections":[{"resource_type":"demo.sylphx.com/Settings","methods":[
        {"id":"demo.settings.get","kind":"get","http":{"query_params":[]}},
        {"id":"demo.settings.update","kind":"update","http":{"query_params":["update_mask","allow_missing","validate_only"]}}
      ]}]}]
    }"#).unwrap()
}

#[derive(Default)]
struct Fake {
    live: Mutex<Option<Value>>,
    writes: Mutex<usize>,
}

impl Wire for Fake {
    async fn send(
        &self,
        request: HttpRequest,
        _: Vec<(String, String)>,
    ) -> Result<HttpResponse, Error> {
        let value = match request.method {
            "GET" => self
                .live
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| json!({"spec":{}})),
            "PATCH" => {
                let mut value = request.body.unwrap();
                value["meta"]["etag"] = json!("etag-1");
                if !request
                    .query
                    .iter()
                    .any(|(k, v)| k == "validate_only" && v == "true")
                {
                    *self.live.lock().unwrap() = Some(value.clone());
                    *self.writes.lock().unwrap() += 1;
                }
                value
            }
            other => panic!("unexpected method {other}"),
        };
        Ok(HttpResponse {
            status: 200,
            request_id: None,
            body: serde_json::to_vec(&value).unwrap(),
        })
    }
}

fn matches(flags: &[&str]) -> ArgMatches {
    let args: Vec<&str> = ["sylphx", "apply", "-o", "json"]
        .into_iter()
        .chain(flags.iter().copied())
        .collect();
    crate::cli(&crate::tree::load())
        .try_get_matches_from(args)
        .unwrap()
        .remove_subcommand()
        .unwrap()
        .1
}

#[tokio::test]
async fn json_create_then_no_op_and_detailed_exit_codes() {
    let reg = registry();
    let declarable = Declarable::from_toml("kinds = [\"demo.sylphx.com/Settings\"]").unwrap();
    let fake = Fake::default();
    let applier = Applier::new(
        &reg,
        &declarable,
        &fake,
        Options {
            env: Some("orgs/o/projects/p/envs/e".into()),
            ..Options::default()
        },
    );
    let decls = Declarations::from_toml(
        "[[resource]]\nkind = 'demo.sylphx.com/Settings'\n[resource.spec]\ncurrency = 'usd'\n",
    )
    .unwrap();
    let m = matches(&["--detailed-exitcode"]);
    let mut stdout = Vec::new();
    let result = execute(&m, &applier, &decls, Format::Json, &mut stdout).await;
    let (_, code) = result.unwrap_or_else(|_| panic!("create failed"));
    assert_eq!(code, 2);
    let printed: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(printed["resources"][0]["action"], "create");
    stdout.clear();
    let (_, code) = execute(&m, &applier, &decls, Format::Json, &mut stdout)
        .await
        .unwrap_or_else(|_| panic!("no-op failed"));
    assert_eq!(code, 0);
    let printed: Value = serde_json::from_slice(&stdout).unwrap();
    assert_eq!(printed["resources"][0]["action"], "no-op");
    assert_eq!(*fake.writes.lock().unwrap(), 1);

    // Release ownership is a refusal, not a changes exit.
    fake.live.lock().unwrap().as_mut().unwrap()["meta"]["annotations"]["sylphx-apply-release"] =
        json!("01900000-0000-7000-8000-000000000001");
    let (report, code) = execute(&m, &applier, &decls, Format::Json, &mut Vec::new())
        .await
        .unwrap_or_else(|_| panic!("refusal failed"));
    assert_eq!(code, 1);
    assert_eq!(report.resources[0].action, Action::Refused);
}

#[tokio::test]
async fn dry_run_prints_changes_without_writing_and_default_exit_is_zero() {
    let reg = registry();
    let declarable = Declarable::from_toml("kinds = [\"demo.sylphx.com/Settings\"]").unwrap();
    let fake = Fake::default();
    let applier = Applier::new(
        &reg,
        &declarable,
        &fake,
        Options {
            env: Some("orgs/o/projects/p/envs/e".into()),
            ..Options::default()
        },
    );
    let decls = Declarations::from_toml(
        "[[resource]]\nkind = 'demo.sylphx.com/Settings'\n[resource.spec]\ncurrency = 'usd'\n",
    )
    .unwrap();
    for (flags, expected) in [
        (vec!["--dry-run", "--detailed-exitcode"], 2),
        (vec!["--dry-run"], 0),
    ] {
        let (report, code) = execute(
            &matches(&flags),
            &applier,
            &decls,
            Format::Json,
            &mut Vec::new(),
        )
        .await
        .unwrap_or_else(|_| panic!("dry-run failed"));
        assert_eq!(report.resources[0].action, Action::Create);
        assert_eq!(code, expected);
    }
    assert_eq!(*fake.writes.lock().unwrap(), 0);
}

fn result_item(action: Action, outcome: sylphx_apply::Outcome) -> sylphx_apply::Applied {
    sylphx_apply::Applied {
        action,
        outcome,
        kind: "demo.sylphx.com/Settings".into(),
        name: "orgs/o/projects/p/envs/e/settings".into(),
        code: None,
        reason: "reason".repeat(1000),
        diff: Vec::new(),
    }
}

#[test]
fn termination_result_matches_release_consumer_contract() {
    use sylphx_apply::{Outcome, Report};
    let mut failed = result_item(Action::Update, Outcome::Failed);
    failed.code = Some("permission_denied".into());
    let report = Report {
        resources: vec![
            result_item(Action::Create, Outcome::Written),
            result_item(Action::Update, Outcome::Written),
            result_item(Action::Drift, Outcome::Unsettled),
            result_item(Action::Orphan, Outcome::Unchanged),
            result_item(Action::NoOp, Outcome::Unchanged),
            failed,
        ],
        unsettled: vec!["orgs/o/projects/p/envs/e/settings".into()],
        error: None,
    };
    let output: Value = serde_json::from_str(&termination_result(&report)).unwrap();
    // apply_step::summary reads these pointers, not the stdout resources array.
    for key in ["create", "update", "drift", "orphan"] {
        assert_eq!(
            output
                .pointer(&format!("/summary/{key}"))
                .and_then(Value::as_u64),
            Some(1)
        );
    }
    // apply_step::failure_reason consumes this object; unsettled reads strings.
    assert_eq!(
        output["failed"],
        json!({
            "kind": "demo.sylphx.com/Settings",
            "name": "orgs/o/projects/p/envs/e/settings",
            "code": "permission_denied"
        })
    );
    assert_eq!(output["unsettled"], json!(report.unsettled));
    assert_eq!(output["truncated"], false);
    assert!(output.get("resources").is_none());

    let refused = Report {
        resources: vec![result_item(Action::Refused, Outcome::Unchanged)],
        unsettled: Vec::new(),
        error: None,
    };
    let output: Value = serde_json::from_str(&termination_result(&refused)).unwrap();
    assert_eq!(output["failed"]["code"], "refused");
    let empty = Report {
        resources: Vec::new(),
        unsettled: Vec::new(),
        error: None,
    };
    let output: Value = serde_json::from_str(&termination_result(&empty)).unwrap();
    assert!(output["failed"].is_null());
    assert_eq!(output["summary"]["create"], 0);
}

#[test]
fn termination_result_stays_valid_with_oversized_unicode_and_escaped_names() {
    use sylphx_apply::{Outcome, Report};
    let name = "雪\\\"\n".repeat(3000);
    let mut failed = result_item(Action::Create, Outcome::Failed);
    failed.kind = name.clone();
    failed.name = name.clone();
    failed.code = Some(name.clone());
    let report = Report {
        resources: vec![result_item(Action::Create, Outcome::Written), failed],
        unsettled: vec!["first".into(), name, "last".into()],
        error: None,
    };
    let text = termination_result(&report);
    assert!(text.len() <= 4096, "{} bytes", text.len());
    let output: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(output["summary"]["create"], 1);
    assert_eq!(output["truncated"], true);
    assert_eq!(output["unsettled"], json!(["first"]));
    assert_eq!(
        output["failed"]["name"].as_str().unwrap().chars().count(),
        120
    );
}

#[test]
fn termination_result_bounds_many_unsettled_resources() {
    let report = sylphx_apply::Report {
        resources: Vec::new(),
        unsettled: (0..2000).map(|i| format!("resource-{i}")).collect(),
        error: None,
    };
    let text = termination_result(&report);
    assert!(text.len() <= 4096);
    let output: Value = serde_json::from_str(&text).unwrap();
    let names = output["unsettled"].as_array().unwrap();
    assert!(!names.is_empty());
    assert!(names.len() < report.unsettled.len());
    assert_eq!(output["truncated"], true);
}

#[test]
fn release_apply_step_argv_parses() {
    let m = crate::cli(&crate::tree::load())
        .try_get_matches_from([
            "sylphx",
            "apply",
            "--release",
            "01900000-0000-7000-8000-000000000001",
            "--manifest",
            "/apply/declarations.json",
            "--source",
            "repo@sha:sylphx.toml",
            "-o",
            "json",
        ])
        .unwrap();
    assert_eq!(m.get_one::<String>("output").unwrap(), "json");
    let sub = m.subcommand_matches("apply").unwrap();
    assert_eq!(
        sub.get_one::<String>("manifest").unwrap(),
        "/apply/declarations.json"
    );
    assert_eq!(
        sub.get_one::<String>("source").unwrap(),
        "repo@sha:sylphx.toml"
    );
}
