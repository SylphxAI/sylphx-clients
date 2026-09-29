//! Which kinds `apply` may declare (spec §4.1): a computed rule over the
//! registry, and membership in `declarable.toml`.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::registry::{FieldType, Registry, ResourceType, ENV_PARENT};

const EMBEDDED_LIST: &str = include_str!("../declarable.toml");

/// Kinds another writer owns until its cutover, and who writes them today
/// (spec §4.1). A trailing `/*` names every kind of a service.
const OWNED: &[(&str, &str)] = &[
    (
        "hosting.sylphx.com/Service",
        "`[[services]]` of sylphx.toml",
    ),
    ("hosting.sylphx.com/Preview", "preview environments"),
    (
        "data.sylphx.com/Database",
        "`[resources.database]` of sylphx.toml",
    ),
    (
        "network.sylphx.com/Route",
        "the `domains` projection of sylphx.toml",
    ),
    (
        "workflows.sylphx.com/Schedule",
        "`[[schedules]]` of sylphx.toml",
    ),
    ("release.sylphx.com/Release", "the Release controller"),
    (
        "secrets.sylphx.com/*",
        "Secrets (values never live in a repository)",
    ),
    (
        "keys.sylphx.com/*",
        "Keys (values never live in a repository)",
    ),
];

/// Who writes `kind` today, when it is not `apply`.
pub fn owner_of(kind: &str) -> Option<&'static str> {
    OWNED.iter().find_map(|(pattern, owner)| {
        let hit = match pattern.strip_suffix('*') {
            Some(prefix) => kind.starts_with(prefix),
            None => *pattern == kind,
        };
        hit.then_some(*owner)
    })
}

#[derive(Deserialize)]
struct List {
    #[serde(default)]
    kinds: Vec<String>,
}

/// The kinds `clients/apply/declarable.toml` lists.
#[derive(Debug, Clone, Default)]
pub struct Declarable {
    kinds: BTreeSet<String>,
}

impl Declarable {
    /// The list this crate was built with.
    pub fn embedded() -> Declarable {
        Declarable::from_toml(EMBEDDED_LIST).expect("declarable.toml is valid")
    }

    pub fn from_toml(text: &str) -> Result<Declarable, String> {
        let list: List = toml::from_str(text).map_err(|e| format!("declarable.toml: {e}"))?;
        Ok(Declarable {
            kinds: list.kinds.into_iter().collect(),
        })
    }

    pub fn contains(&self, kind: &str) -> bool {
        self.kinds.contains(kind)
    }

    pub fn kinds(&self) -> impl Iterator<Item = &str> {
        self.kinds.iter().map(String::as_str)
    }
}

/// The computed rule alone: `Err` names the first condition the type fails.
pub fn computed(reg: &Registry, rt: &ResourceType) -> Result<(), String> {
    let kind = &rt.type_name;
    if rt.parent_pattern != ENV_PARENT {
        return Err(format!(
            "`{kind}` is not below an environment (its parent is `{}`)",
            rt.parent_pattern
        ));
    }
    if rt.service == "access" {
        return Err(format!(
            "`{kind}` belongs to Access, which apply never writes"
        ));
    }
    if reg.method(kind, "get").is_none() {
        return Err(format!("`{kind}` has no Get"));
    }
    match reg.method(kind, "update") {
        None => return Err(format!("`{kind}` has no Update")),
        Some(m) => {
            for q in ["allow_missing", "validate_only"] {
                if !m.http.query_params.iter().any(|p| p == q) {
                    return Err(format!("`{kind}` Update does not accept `{q}`"));
                }
            }
        }
    }
    if !rt.is_singleton() && reg.method(kind, "list").is_none() {
        return Err(format!("`{kind}` has no List and is not a singleton"));
    }
    // The mask is per top-level spec field, so a secret nested under a
    // managed field would be cleared by every write.
    for field in reg.managed_fields(rt) {
        let mut seen = BTreeSet::new();
        if let Some(path) = nested_unmanaged(reg, &field.ty, &mut seen) {
            return Err(format!(
                "`{kind}` spec field `{}` holds a sensitive or INPUT_ONLY field (`{path}`) nested under it, which a per-field write would clear",
                field.name
            ));
        }
    }
    Ok(())
}

/// The path of a sensitive or INPUT_ONLY field anywhere below `ty`.
fn nested_unmanaged(reg: &Registry, ty: &FieldType, seen: &mut BTreeSet<String>) -> Option<String> {
    match ty {
        FieldType::Map { value } => nested_unmanaged(reg, value, seen),
        FieldType::Message { name } => {
            if !seen.insert(name.clone()) {
                return None;
            }
            let message = reg.message(name)?;
            for f in &message.fields {
                if f.sensitive || f.has("INPUT_ONLY") {
                    return Some(format!("{name}.{}", f.name));
                }
                if let Some(p) = nested_unmanaged(reg, &f.ty, seen) {
                    return Some(p);
                }
            }
            None
        }
        _ => None,
    }
}

/// Whether a declaration of `kind` is allowed at all; `Err` is the reason,
/// naming the current owner where one writes the kind today.
pub fn check_kind(
    reg: &Registry,
    declarable: &Declarable,
    kind: &str,
) -> Result<ResourceType, String> {
    if let Some(owner) = owner_of(kind) {
        return Err(format!(
            "`{kind}` is written by {owner} until its cutover; apply does not declare it"
        ));
    }
    let Some(rt) = reg.resource(kind) else {
        return Err(format!("unknown kind `{kind}`"));
    };
    computed(reg, rt).map_err(|e| format!("{e}; apply cannot declare it"))?;
    if !declarable.contains(kind) {
        return Err(format!(
            "`{kind}` is not declarable yet: it is not listed in clients/apply/declarable.toml"
        ));
    }
    Ok(rt.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_the_pilot_kind_only() {
        let d = Declarable::embedded();
        assert_eq!(d.kinds().collect::<Vec<_>>(), ["money.sylphx.com/Catalog"]);
    }

    #[test]
    fn every_listed_kind_in_the_registry_passes_the_computed_rule() {
        let reg = Registry::embedded();
        for kind in Declarable::embedded().kinds() {
            if let Some(rt) = reg.resource(kind) {
                computed(reg, rt).unwrap_or_else(|e| panic!("{kind}: {e}"));
            }
        }
    }

    #[test]
    fn owned_kinds_name_their_owner() {
        let reg = Registry::embedded();
        let d = Declarable::embedded();
        for (kind, needle) in [
            ("hosting.sylphx.com/Service", "[[services]]"),
            ("data.sylphx.com/Database", "[resources.database]"),
            ("network.sylphx.com/Route", "domains"),
            ("workflows.sylphx.com/Schedule", "[[schedules]]"),
            ("release.sylphx.com/Release", "Release controller"),
            ("secrets.sylphx.com/Secret", "Secrets"),
            ("keys.sylphx.com/Anything", "Keys"),
        ] {
            let e = check_kind(reg, &d, kind).unwrap_err();
            assert!(e.contains(needle), "{kind}: {e}");
        }
    }

    #[test]
    fn unknown_and_unlisted_kinds_are_refused() {
        let reg = Registry::embedded();
        let d = Declarable::embedded();
        assert!(check_kind(reg, &d, "nope.sylphx.com/Nothing")
            .unwrap_err()
            .contains("unknown kind"));
        let e = check_kind(reg, &d, "access.sylphx.com/ApiKey").unwrap_err();
        assert!(e.contains("Access"), "{e}");
    }
}
