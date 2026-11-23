//! Per-service runtime env isolation: `[[services]] env_isolation`.
//!
//! A service's `env` list is informational by default (`env_isolation`
//! omitted or `"inherit"`): every environment-scoped variable reaches the
//! service exactly as before, whatever the list says.
//!
//! `env_isolation = "strict"` makes the service's `env` list the exclusive
//! allowlist of its runtime configuration. Secrets, `[env]` literals, resource
//! bindings and platform-composed connection URLs (`SYLPHX_*`) that the list
//! does not name never reach the service. `env = []` admits nothing, and
//! strict without an `env` list is refused (fail closed) rather than read as
//! "everything".
//!
//! An `[[environments.<type>.services]]` entry of the same name replaces each
//! field it declares (`env_isolation`, `env`) for that environment type: the
//! same resolution the sealed `env` list already uses.
//!
//! Pure: callers apply the policy at every writer of a service's runtime env.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value};

/// The manifest field on a `[[services]]` entry.
pub const ENV_ISOLATION_FIELD: &str = "env_isolation";
/// Default: the `env` list is informational; every env-scoped key is delivered.
pub const ENV_ISOLATION_INHERIT: &str = "inherit";
/// Opt-in: the `env` list is the exclusive allowlist.
pub const ENV_ISOLATION_STRICT: &str = "strict";

/// The runtime env a service may receive.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ServiceEnvPolicy {
    /// Every environment-scoped key (today's behavior).
    #[default]
    Inherit,
    /// Only these keys (plus caller-preserved platform keys).
    Strict(BTreeSet<String>),
}

impl ServiceEnvPolicy {
    #[must_use]
    pub fn is_strict(&self) -> bool {
        matches!(self, Self::Strict(_))
    }

    /// Whether `key` may reach the service. `preserved` names keys the
    /// platform derives from the service's own declarations (its `connect`
    /// URLs); they are admitted under strict as well.
    #[must_use]
    pub fn admits(&self, key: &str, preserved: &BTreeSet<String>) -> bool {
        match self {
            Self::Inherit => true,
            Self::Strict(allowed) => allowed.contains(key) || preserved.contains(key),
        }
    }

    /// Drop every key the policy does not admit; returns the dropped key
    /// names, sorted (names only, never values).
    pub fn narrow_json(
        &self,
        values: &mut Map<String, Value>,
        preserved: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut removed: Vec<String> = values
            .keys()
            .filter(|key| !self.admits(key, preserved))
            .cloned()
            .collect();
        for key in &removed {
            values.remove(key);
        }
        removed.sort();
        removed
    }

    /// [`Self::narrow_json`] over a plaintext map.
    pub fn narrow_plain(
        &self,
        values: &mut BTreeMap<String, String>,
        preserved: &BTreeSet<String>,
    ) -> Vec<String> {
        let mut removed = Vec::new();
        values.retain(|key, _| {
            let keep = self.admits(key, preserved);
            if !keep {
                removed.push(key.clone());
            }
            keep
        });
        removed
    }
}

fn service_entry<'a>(section: &'a Value, name: &str) -> Option<&'a Value> {
    section
        .get("services")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("name").and_then(Value::as_str).map(str::trim) == Some(name))
}

fn resolve_policy(
    manifest: &Value,
    env_type: Option<&str>,
    name: &str,
) -> Result<ServiceEnvPolicy, String> {
    let base = service_entry(manifest, name);
    let over = env_type
        .and_then(|env_type| manifest.get("environments")?.get(env_type))
        .and_then(|section| service_entry(section, name));
    let field = |key: &str| {
        over.and_then(|entry| entry.get(key))
            .or_else(|| base.and_then(|entry| entry.get(key)))
    };
    match field(ENV_ISOLATION_FIELD) {
        None => return Ok(ServiceEnvPolicy::Inherit),
        Some(Value::String(mode)) if mode == ENV_ISOLATION_INHERIT => {
            return Ok(ServiceEnvPolicy::Inherit)
        }
        Some(Value::String(mode)) if mode == ENV_ISOLATION_STRICT => {}
        Some(_) => {
            return Err(format!(
                "service {name}: env_isolation must be \"{ENV_ISOLATION_INHERIT}\" or \
                 \"{ENV_ISOLATION_STRICT}\""
            ))
        }
    }
    let list = field("env")
        .ok_or_else(|| {
            format!(
                "service {name} declares env_isolation = \"strict\" without an env list: list \
                 every variable it may receive (env = [] for none)"
            )
        })?
        .as_array()
        .ok_or_else(|| {
            format!("service {name}: env must be a list of variable names under strict isolation")
        })?;
    let mut allowed = BTreeSet::new();
    for key in list {
        let key = key
            .as_str()
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .ok_or_else(|| {
                format!("service {name}: env entries must be variable names under strict isolation")
            })?;
        allowed.insert(key.to_string());
    }
    Ok(ServiceEnvPolicy::Strict(allowed))
}

/// The policy of one service in one environment type. A manifest that does
/// not declare the service declares no isolation: [`ServiceEnvPolicy::Inherit`].
pub fn service_env_policy(
    manifest: &Value,
    env_type: &str,
    service_name: &str,
) -> Result<ServiceEnvPolicy, String> {
    resolve_policy(manifest, Some(env_type), service_name.trim())
}

/// Refuse an invalid `env_isolation` at manifest admission, for every service
/// in every environment type the manifest names (not only the one released).
pub fn validate_manifest(manifest: &Value) -> Result<(), String> {
    let names_in = |section: &Value| -> Vec<String> {
        section
            .get("services")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.get("name").and_then(Value::as_str))
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect()
    };
    let mut names: BTreeSet<String> = names_in(manifest).into_iter().collect();
    let mut env_types = vec![None];
    if let Some(environments) = manifest.get("environments").and_then(Value::as_object) {
        for (env_type, section) in environments {
            names.extend(names_in(section));
            env_types.push(Some(env_type.as_str()));
        }
    }
    for name in &names {
        for env_type in &env_types {
            resolve_policy(manifest, *env_type, name)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn values() -> Map<String, Value> {
        let mut values = Map::new();
        for key in [
            "PUBLIC_API_URL",
            "DATABASE_URL",
            "STRIPE_SECRET_KEY",
            "SYLPHX_SECRET_URL",
            "API_INTERNAL_URL",
        ] {
            values.insert(key.into(), json!(format!("value-of-{key}")));
        }
        values
    }

    fn manifest() -> Value {
        json!({
            "env": {
                "PUBLIC_API_URL": {"value": "https://api.example.test"},
                "DATABASE_URL": {"from_resource": "database", "attr": "database_url"},
                "STRIPE_SECRET_KEY": {"from_secret": "STRIPE_SECRET_KEY"}
            },
            "services": [
                {"name": "web", "env_isolation": "strict", "env": ["PUBLIC_API_URL"]},
                {"name": "edge", "env_isolation": "strict", "env": []},
                {"name": "api"},
                {"name": "worker", "env": ["DATABASE_URL"]},
                {"name": "explicit", "env_isolation": "inherit", "env": []}
            ]
        })
    }

    fn connect() -> BTreeSet<String> {
        BTreeSet::from(["API_INTERNAL_URL".to_string()])
    }

    #[test]
    fn strict_web_receives_only_its_declared_keys() {
        let policy = service_env_policy(&manifest(), "production", "web").unwrap();
        assert_eq!(
            policy,
            ServiceEnvPolicy::Strict(BTreeSet::from(["PUBLIC_API_URL".to_string()]))
        );
        let mut sealed = values();
        let removed = policy.narrow_json(&mut sealed, &BTreeSet::new());
        assert_eq!(sealed.keys().collect::<Vec<_>>(), vec!["PUBLIC_API_URL"]);
        assert!(!sealed.contains_key("DATABASE_URL"));
        assert!(!sealed.contains_key("STRIPE_SECRET_KEY"));
        assert!(!sealed.contains_key("SYLPHX_SECRET_URL"));
        assert_eq!(
            removed,
            vec![
                "API_INTERNAL_URL",
                "DATABASE_URL",
                "STRIPE_SECRET_KEY",
                "SYLPHX_SECRET_URL"
            ]
        );
    }

    #[test]
    fn strict_empty_list_seals_zero_keys() {
        let policy = service_env_policy(&manifest(), "production", "edge").unwrap();
        assert_eq!(policy, ServiceEnvPolicy::Strict(BTreeSet::new()));
        let mut sealed = values();
        policy.narrow_json(&mut sealed, &BTreeSet::new());
        assert!(sealed.is_empty(), "env = [] under strict admits nothing");
        let mut plain: BTreeMap<String, String> =
            BTreeMap::from([("DATABASE_URL".into(), "postgres://x".into())]);
        assert_eq!(
            policy.narrow_plain(&mut plain, &BTreeSet::new()),
            vec!["DATABASE_URL"]
        );
        assert!(plain.is_empty());
    }

    #[test]
    fn omitted_or_inherit_isolation_keeps_every_key() {
        for service in ["api", "worker", "explicit", "undeclared"] {
            let policy = service_env_policy(&manifest(), "production", service).unwrap();
            assert_eq!(policy, ServiceEnvPolicy::Inherit, "{service}");
            let mut sealed = values();
            assert!(policy.narrow_json(&mut sealed, &BTreeSet::new()).is_empty());
            assert_eq!(sealed, values(), "{service}: byte-identical to today");
            let mut plain: BTreeMap<String, String> = values()
                .into_iter()
                .map(|(key, value)| (key, value.as_str().unwrap().to_string()))
                .collect();
            assert!(policy.narrow_plain(&mut plain, &BTreeSet::new()).is_empty());
            assert_eq!(plain.len(), values().len());
        }
    }

    #[test]
    fn strict_keeps_the_services_own_connect_urls() {
        let policy = service_env_policy(&manifest(), "production", "web").unwrap();
        let mut sealed = values();
        policy.narrow_json(&mut sealed, &connect());
        assert_eq!(
            sealed.keys().collect::<Vec<_>>(),
            vec!["API_INTERNAL_URL", "PUBLIC_API_URL"]
        );
    }

    #[test]
    fn json_and_plain_narrowing_agree() {
        // The Release v2 seal narrows a JSON map and the legacy app-env writers
        // a plaintext map: one policy, one result.
        for service in ["web", "edge", "api"] {
            let policy = service_env_policy(&manifest(), "production", service).unwrap();
            let mut sealed = values();
            let json_removed = policy.narrow_json(&mut sealed, &connect());
            let mut plain: BTreeMap<String, String> = values()
                .into_iter()
                .map(|(key, value)| (key, value.as_str().unwrap().to_string()))
                .collect();
            let plain_removed = policy.narrow_plain(&mut plain, &connect());
            assert_eq!(json_removed, plain_removed, "{service}");
            assert_eq!(
                sealed.keys().cloned().collect::<Vec<_>>(),
                plain.keys().cloned().collect::<Vec<_>>(),
                "{service}"
            );
        }
    }

    #[test]
    fn aliased_secret_ships_only_under_its_declared_name() {
        let manifest = json!({
            "env": {"PAYMENTS_KEY": {"from_secret": "STRIPE_SECRET_KEY"}},
            "services": [{"name": "web", "env_isolation": "strict", "env": ["PAYMENTS_KEY"]}]
        });
        let policy = service_env_policy(&manifest, "production", "web").unwrap();
        let mut sealed = Map::new();
        sealed.insert("STRIPE_SECRET_KEY".into(), json!("sk"));
        sealed.insert("PAYMENTS_KEY".into(), json!("sk"));
        policy.narrow_json(&mut sealed, &BTreeSet::new());
        assert_eq!(sealed.keys().collect::<Vec<_>>(), vec!["PAYMENTS_KEY"]);
    }

    #[test]
    fn strict_without_env_list_fails_closed() {
        let manifest = json!({"services": [{"name": "web", "env_isolation": "strict"}]});
        let err = service_env_policy(&manifest, "production", "web").unwrap_err();
        assert!(err.contains("without an env list"), "{err}");
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn invalid_isolation_values_and_lists_are_refused() {
        for service in [
            json!({"name": "web", "env_isolation": "STRICT", "env": []}),
            json!({"name": "web", "env_isolation": true, "env": []}),
            json!({"name": "web", "env_isolation": "strict", "env": "DATABASE_URL"}),
            json!({"name": "web", "env_isolation": "strict", "env": ["OK", 1]}),
            json!({"name": "web", "env_isolation": "strict", "env": [" "]}),
        ] {
            let manifest = json!({"services": [service.clone()]});
            assert!(
                service_env_policy(&manifest, "production", "web").is_err(),
                "{service}"
            );
            assert!(validate_manifest(&manifest).is_err(), "{service}");
        }
    }

    #[test]
    fn environment_type_override_replaces_declared_fields() {
        let manifest = json!({
            "services": [{"name": "web", "env": ["PUBLIC_API_URL"]}],
            "environments": {
                "production": {"services": [{"name": "web", "env_isolation": "strict"}]},
                "preview": {"services": [{"name": "web", "env_isolation": "strict", "env": []}]},
                "staging": {"services": [{"name": "api"}]}
            }
        });
        // Production opts in and inherits the base list.
        assert_eq!(
            service_env_policy(&manifest, "production", "web").unwrap(),
            ServiceEnvPolicy::Strict(BTreeSet::from(["PUBLIC_API_URL".to_string()]))
        );
        // Preview replaces the list, even with an empty one.
        assert_eq!(
            service_env_policy(&manifest, "preview", "web").unwrap(),
            ServiceEnvPolicy::Strict(BTreeSet::new())
        );
        // Other environment types keep the informational base.
        for env_type in ["staging", "development"] {
            assert_eq!(
                service_env_policy(&manifest, env_type, "web").unwrap(),
                ServiceEnvPolicy::Inherit
            );
        }
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn base_strict_without_list_is_refused_even_if_one_override_lists() {
        let manifest = json!({
            "services": [{"name": "web", "env_isolation": "strict"}],
            "environments": {"production": {"services": [{"name": "web", "env": []}]}}
        });
        assert!(service_env_policy(&manifest, "production", "web").is_ok());
        assert!(service_env_policy(&manifest, "preview", "web").is_err());
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn override_can_relax_back_to_inherit() {
        let manifest = json!({
            "services": [{"name": "web", "env_isolation": "strict", "env": []}],
            "environments": {"development": {"services": [{"name": "web", "env_isolation": "inherit"}]}}
        });
        assert_eq!(
            service_env_policy(&manifest, "development", "web").unwrap(),
            ServiceEnvPolicy::Inherit
        );
        assert!(service_env_policy(&manifest, "production", "web")
            .unwrap()
            .is_strict());
    }

    #[test]
    fn every_manifest_without_the_field_validates_unchanged() {
        // Informational lists, including `env = []` and lists naming keys no
        // `[env]` declares, are never validated or enforced without opt-in.
        for manifest in [
            json!({}),
            json!({"services": []}),
            json!({"services": [{"name": "runner", "env": []}]}),
            json!({"services": [{"name": "api", "env": ["DATABASE_URL", "PORT"]}]}),
            json!({"services": [{"name": "api", "env": "not-a-list"}]}),
            manifest(),
        ] {
            assert!(validate_manifest(&manifest).is_ok(), "{manifest}");
        }
    }
}
