//! GitLab registration and environment operations through the public CLI.

use super::{run, serve, Sandbox, ENV};
use serde_json::{json, Value};

const CONNECTION_ID: &str = "gitlab-example";
const PROJECT: &str = "prj_fs016wck3akrzep3zkwbv3dt0k9hgr";
const ENVIRONMENT: &str = "env_fs01y448d2kqbb8ntphzf61k85wthw";
const ORG: &str = "org_fs01afbdccgn8xdk1ghj5y47w0vtph";
const BINDING: &str = "00000000-0000-4000-8000-000000000001";

#[tokio::test]
async fn gitlab_connection_create_sends_only_pinned_secret_version_names() {
    let sb = Sandbox::new("gitlab-create");
    let api = format!("{ENV}/secrets/gitlab-api/secret_versions/3");
    let webhook = format!("{ENV}/secrets/gitlab-webhook/secret_versions/5");
    let response = json!({
        "name": format!("orgs/org_a/connections/{CONNECTION_ID}"),
        "status": {"state": "pending"}
    });
    let (url, log) = serve(vec![(200, json!({"name": "orgs/org_a/operations/create-gitlab-example", "done": true, "response": response}))]).await;
    let out = run(
        &sb,
        &url,
        &[
            "connections",
            "create",
            CONNECTION_ID,
            "--parent",
            "orgs/org_a",
            "--spec.gitlab.instance-url",
            "https://gitlab.example.com/scm",
            "--spec.gitlab.project-id",
            "4321",
            "--spec.gitlab.api-token-version",
            &api,
            "--spec.gitlab.webhook-token-version",
            &webhook,
            "-o",
            "json",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&out.stdout).unwrap(),
        response
    );
    let calls = log.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].line,
        "POST /v1/orgs/org_a/connections?connection_id=gitlab-example HTTP/1.1"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&calls[0].body).unwrap(),
        json!({"spec": {"gitlab": {
            "instance_url": "https://gitlab.example.com/scm", "project_id": "4321",
            "api_token_version": api, "webhook_token_version": webhook
        }}})
    );
}

#[tokio::test]
async fn gitlab_binding_uses_the_existing_environment_api_and_prints_readback() {
    let sb = Sandbox::new("gitlab-binding");
    let target = format!("orgs/{ORG}/projects/{PROJECT}/envs/{ENVIRONMENT}");
    let patch = json!({"gitlabConnectionId": CONNECTION_ID, "githubBranch": "release/main"});
    let pending = json!({"githubBranch": "release/main", "gitlabConnectionId": CONNECTION_ID, "gitlabBindingId": null});
    let bound = json!({"githubBranch": "release/main", "gitlabConnectionId": CONNECTION_ID, "gitlabBindingId": BINDING});
    let detached = json!({"githubBranch": "release/main", "gitlabConnectionId": null, "gitlabBindingId": null});
    let (url, log) = serve(vec![(200, pending), (200, bound), (200, detached)]).await;
    let before = std::fs::read(sb.dir.join(".sylphx/project.json")).unwrap();
    for (verb, args, connection, binding, branch, method, body) in [
        (
            "bind",
            vec![CONNECTION_ID, "--branch", "release/main"],
            json!(CONNECTION_ID),
            Value::Null,
            json!("release/main"),
            "PATCH",
            Some(patch),
        ),
        (
            "status",
            vec![],
            json!(CONNECTION_ID),
            json!(BINDING),
            json!("release/main"),
            "GET",
            None,
        ),
        (
            "unbind",
            vec!["--yes"],
            Value::Null,
            Value::Null,
            Value::Null,
            "PATCH",
            Some(json!({"gitlabConnectionId":null})),
        ),
    ] {
        let mut command = vec!["hosting", "gitlab", verb, "--env", &target, "-o", "json"];
        command.extend(args);
        let out = run(&sb, &url, &command).await;
        assert!(
            out.status.success(),
            "{verb}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&out.stdout).unwrap(),
            json!({
                "name": target, "gitlabConnectionId": connection, "gitlabBindingId": binding, "branch": branch,
            })
        );
        let calls = log.lock();
        let last = calls.last().unwrap();
        assert_eq!(
            last.line,
            format!("{method} /v1/projects/{PROJECT}/environments/{ENVIRONMENT} HTTP/1.1")
        );
        if let Some(expected) = body {
            assert_eq!(serde_json::from_str::<Value>(&last.body).unwrap(), expected);
        } else {
            assert!(last.body.is_empty());
        }
    }
    assert_eq!(log.lock().len(), 3);
    assert_eq!(
        std::fs::read(sb.dir.join(".sylphx/project.json")).unwrap(),
        before
    );
}

#[tokio::test]
async fn gitlab_binding_refuses_invalid_targets_and_unconfirmed_detach_without_writing() {
    let sb = Sandbox::new("gitlab-refusals");
    let (url, log) = serve(vec![(200, json!({}))]).await;
    for (args, error) in [
        (
            vec!["bind", "https://gitlab.example.com/scm", "--branch", "main"],
            "expected a GitLab Connection ID",
        ),
        (
            vec![
                "status",
                "--env",
                "orgs/org_a/projects/prj_a/envs/env_a?other",
            ],
            "expected an environment name",
        ),
        (vec!["bind", CONNECTION_ID], "--branch"),
        (vec!["unbind"], "pass --yes to confirm"),
        (
            vec!["status", "--org", "org_b", "--env", ENV],
            "name different organizations",
        ),
    ] {
        let mut command = vec!["hosting", "gitlab"];
        command.extend(args);
        let out = run(&sb, &url, &command).await;
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(2), "{stderr}");
        assert!(stderr.contains(error), "{stderr}");
    }
    let calls = log.lock();
    assert!(calls.is_empty(), "{calls:?}");
}

#[tokio::test]
async fn gitlab_binding_does_not_claim_success_from_an_old_backend() {
    let sb = Sandbox::new("gitlab-old-backend");
    let (url, log) = serve(vec![(200, json!({"githubBranch":"main"}))]).await;
    let out = run(
        &sb,
        &url,
        &[
            "hosting",
            "gitlab",
            "bind",
            CONNECTION_ID,
            "--branch",
            "main",
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("did not report a GitLab binding"));
    let calls = log.lock();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].line,
        "PATCH /v1/projects/prj_a/environments/env_a HTTP/1.1"
    );
}
