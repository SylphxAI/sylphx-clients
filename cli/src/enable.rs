//! `sylphx auth enable` and `sylphx auth status`: Enable Auth on an
//! environment, and read it back. Both call the composition route that
//! already does the work (`PUT` / `GET /v1/projects/{project}/composition/
//! bindings`, docs/services/auth/capabilities.md "Enable Auth"), which
//! provisions or reuses the environment's Auth instance, so enabling twice
//! answers the same instance. A key holding `auth:admin` on the environment
//! is enough. This module holds the request and answer shapes; `main.rs` does
//! the calls.

use serde_json::{json, Value};

/// The last segment of a resource name (`orgs/o/projects/p/envs/e` is `e`).
pub fn bare_id(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// `/v1/projects/{project}/composition/bindings` for a project name or id.
pub fn bindings_path(project: &str) -> String {
    format!("/v1/projects/{}/composition/bindings", bare_id(project))
}

/// The body that binds Sylphx Auth into the environment's identity slot.
pub fn enable_body(env: &str) -> Value {
    json!({
        "capabilityId": "identity",
        "slot": "identity",
        "providerId": "sylphx-auth",
        "environmentId": bare_id(env),
    })
}

/// The Auth instance an enable answer names.
pub fn organization_id(answer: &Value) -> Option<&str> {
    answer.pointer("/config/organizationId")?.as_str()
}

/// What `status` prints: the identity binding of each environment the key
/// can see (a key bound to one environment sees only its own; the answer
/// names environments by their raw ids, so they are not matched against a
/// resource name here). An empty list means Auth is not enabled.
pub fn status(bindings: &Value) -> Value {
    let identity: Vec<Value> = bindings
        .get("bindings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|b| b.get("slot").and_then(Value::as_str) == Some("identity"))
        .map(|b| {
            json!({
                "environmentId": b.get("environmentId"),
                "provider": b.get("providerId"),
                "status": b.get("status"),
                "organizationId": b.pointer("/config/organizationId"),
            })
        })
        .collect();
    json!({ "enabled": !identity.is_empty(), "bindings": identity })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_is_the_documented_binding() {
        assert_eq!(
            bindings_path("orgs/o/projects/p1"),
            "/v1/projects/p1/composition/bindings"
        );
        assert_eq!(bindings_path("p1"), "/v1/projects/p1/composition/bindings");
        assert_eq!(
            enable_body("orgs/o/projects/p1/envs/e9"),
            json!({
                "capabilityId": "identity",
                "slot": "identity",
                "providerId": "sylphx-auth",
                "environmentId": "e9"
            })
        );
    }

    #[test]
    fn the_answer_names_the_instance() {
        let answer = json!({"status": "active", "config": {"organizationId": "org-1"}});
        assert_eq!(organization_id(&answer), Some("org-1"));
        assert_eq!(organization_id(&json!({"config": {}})), None);
    }

    #[test]
    fn status_lists_only_identity_bindings() {
        let list = json!({"bindings": [
            {"slot": "identity", "environmentId": "e1", "providerId": "sylphx-auth",
             "status": "active", "config": {"organizationId": "org-1"}},
            {"slot": "email", "environmentId": "e2", "providerId": "x"},
        ]});
        let on = status(&list);
        assert_eq!(on["enabled"], true);
        assert_eq!(on["bindings"].as_array().unwrap().len(), 1);
        assert_eq!(on["bindings"][0]["organizationId"], "org-1");
        assert_eq!(status(&json!({"bindings": []}))["enabled"], false);
        assert_eq!(status(&json!({}))["enabled"], false);
    }
}
