//! The offline check (`sylphx apply --offline`, spec §2): declarations
//! against the registry, with no network.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::declare::Declarations;
use crate::eligibility::{check_kind, Declarable};
use crate::error::Issue;
use crate::registry::{Field, FieldType, Registry};

/// Whether `name` is a slug: `^[a-z][a-z0-9-]{0,62}$`.
pub fn is_slug(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && name.len() <= 63
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Every problem in `decls`; empty when the file is sound.
pub fn check(reg: &Registry, declarable: &Declarable, decls: &Declarations) -> Vec<Issue> {
    let mut issues = Vec::new();
    let mut declared: BTreeSet<(String, String)> = BTreeSet::new();

    for (i, d) in decls.resources.iter().enumerate() {
        let at = format!("resource[{i}]");
        let slug = d.name.as_deref();
        if let Some(n) = slug {
            if !is_slug(n) {
                issues.push(issue(
                    format!("{at}.name"),
                    format!("`{n}` is not a slug (^[a-z][a-z0-9-]{{0,62}}$)"),
                ));
            }
        }
        if !declared.insert((d.kind.clone(), slug.unwrap_or_default().to_string())) {
            issues.push(issue(
                at.clone(),
                format!("`{}` is declared twice", label(&d.kind, slug)),
            ));
        }
        for (k, v) in &d.meta.labels {
            if k.starts_with(crate::hash::RESERVED_PREFIX) {
                issues.push(issue(
                    format!("{at}.meta.labels.{k}"),
                    "the `sylphx-` prefix is reserved for apply's markers".into(),
                ));
            }
            if !label_ok(k) || !label_ok(v) {
                issues.push(issue(
                    format!("{at}.meta.labels.{k}"),
                    "label keys and values are [a-z0-9_-], at most 63 characters".into(),
                ));
            }
        }
        match check_kind(reg, declarable, &d.kind) {
            Err(e) => issues.push(issue(format!("{at}.kind"), e)),
            Ok(rt) => {
                if let Err(e) = rt.name_in("orgs/o/projects/p/envs/e", slug) {
                    issues.push(issue(format!("{at}.name"), e));
                }
                if let Some(msg) = reg.spec_message(&rt) {
                    check_object(
                        reg,
                        &msg.fields,
                        &d.spec,
                        &format!("{at}.spec"),
                        &mut issues,
                    );
                } else {
                    issues.push(issue(
                        format!("{at}.kind"),
                        format!("`{}` has no spec message in the registry", d.kind),
                    ));
                }
            }
        }
    }

    for (i, r) in decls.removed.iter().enumerate() {
        let at = format!("removed[{i}]");
        let slug = r.name.as_deref();
        if let Some(n) = slug {
            if !is_slug(n) {
                issues.push(issue(
                    format!("{at}.name"),
                    format!("`{n}` is not a slug (^[a-z][a-z0-9-]{{0,62}}$)"),
                ));
            }
        }
        if decls
            .resources
            .iter()
            .any(|d| d.kind == r.kind && d.name.as_deref() == slug)
        {
            issues.push(issue(
                at.clone(),
                format!(
                    "`{}` is in both [[resource]] and [[removed]]",
                    label(&r.kind, slug)
                ),
            ));
        }
        match reg.resource(&r.kind) {
            None => issues.push(issue(
                format!("{at}.kind"),
                format!("unknown kind `{}`", r.kind),
            )),
            Some(rt) => {
                if let Err(e) = rt.name_in("orgs/o/projects/p/envs/e", slug) {
                    issues.push(issue(format!("{at}.name"), e));
                }
            }
        }
    }
    issues
}

fn label(kind: &str, name: Option<&str>) -> String {
    match name {
        Some(n) => format!("{kind}/{n}"),
        None => kind.to_string(),
    }
}

fn issue(location: String, message: String) -> Issue {
    Issue { location, message }
}

fn label_ok(s: &str) -> bool {
    s.len() <= 63
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Checks the members of `obj` against the fields of one message, and its
/// REQUIRED fields.
fn check_object(
    reg: &Registry,
    fields: &[Field],
    obj: &Map<String, Value>,
    at: &str,
    issues: &mut Vec<Issue>,
) {
    let mut oneofs: BTreeSet<&str> = BTreeSet::new();
    for (key, value) in obj {
        let here = format!("{at}.{key}");
        let Some(f) = fields.iter().find(|f| &f.name == key) else {
            issues.push(issue(here, "unknown field".into()));
            continue;
        };
        if f.sensitive || f.has("INPUT_ONLY") {
            issues.push(issue(
                here,
                "is sensitive or input-only and is never declared".into(),
            ));
            continue;
        }
        if f.is_server_side() {
            issues.push(issue(
                here,
                "is set by the server and is never declared".into(),
            ));
            continue;
        }
        if value.is_null() {
            continue;
        }
        if let Some(o) = f.oneof.as_deref() {
            if !oneofs.insert(o) {
                issues.push(issue(
                    here.clone(),
                    format!("more than one member of oneof `{o}`"),
                ));
            }
        }
        check_field(reg, f, value, &here, issues);
    }
    for f in fields {
        if f.has("REQUIRED") && !f.is_server_side() && obj.get(&f.name).is_none_or(Value::is_null) {
            issues.push(issue(format!("{at}.{}", f.name), "is required".into()));
        }
    }
}

fn check_field(reg: &Registry, f: &Field, value: &Value, at: &str, issues: &mut Vec<Issue>) {
    if f.repeated {
        match value.as_array() {
            None => issues.push(issue(at.to_string(), "must be a list".into())),
            Some(items) => {
                for (i, v) in items.iter().enumerate() {
                    check_value(reg, &f.ty, v, &format!("{at}[{i}]"), issues);
                }
            }
        }
        return;
    }
    check_value(reg, &f.ty, value, at, issues);
}

fn check_value(reg: &Registry, ty: &FieldType, value: &Value, at: &str, issues: &mut Vec<Issue>) {
    let bad = |issues: &mut Vec<Issue>, want: &str| {
        issues.push(issue(at.to_string(), format!("must be {want}")));
    };
    match ty {
        FieldType::Scalar { scalar } => {
            let ok = match scalar.as_str() {
                "string" | "bytes" => value.is_string(),
                "bool" => value.is_boolean(),
                "int32" | "uint32" | "int64" | "uint64" | "sint32" | "sint64" | "fixed32"
                | "fixed64" | "sfixed32" | "sfixed64" => is_integer(value),
                "double" | "float" => value.is_number(),
                _ => true,
            };
            if !ok {
                bad(issues, &format!("a {scalar}"));
            }
        }
        FieldType::Enum { name } => match (value.as_str(), reg.enum_(name)) {
            (Some(s), Some(e)) => {
                if !e.values.iter().any(|v| v.wire == s) {
                    let known: Vec<&str> = e.values.iter().map(|v| v.wire.as_str()).collect();
                    issues.push(issue(
                        at.to_string(),
                        format!("`{s}` is not one of: {}", known.join(", ")),
                    ));
                }
            }
            (None, _) => bad(issues, "an enum string"),
            (Some(_), None) => {}
        },
        FieldType::Message { name } => match (value.as_object(), reg.message(name)) {
            (Some(obj), Some(m)) => check_object(reg, &m.fields, obj, at, issues),
            (None, _) => bad(issues, "an object"),
            (Some(_), None) => {}
        },
        FieldType::Map { value: inner } => match value.as_object() {
            Some(entries) => {
                for (k, v) in entries {
                    check_value(reg, inner, v, &format!("{at}.{k}"), issues);
                }
            }
            None => bad(issues, "an object"),
        },
        FieldType::WellKnown { wkt } => {
            let ok = match wkt.as_str() {
                "timestamp" | "duration" | "field_mask" => value.is_string(),
                "struct" => value.is_object(),
                _ => true,
            };
            if !ok {
                bad(issues, &format!("a {wkt} value"));
            }
        }
    }
}

/// protojson accepts an integer as a JSON number or a decimal string.
fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.is_i64() || n.is_u64(),
        Value::String(s) => s.parse::<i128>().is_ok(),
        _ => false,
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A registry with a normal kind (`things`) and a singleton kind
    /// (`settings`), both declarable and below an environment.
    pub const REGISTRY: &str = r#"{
      "resources": [
        {"type": "demo.sylphx.com/Thing", "service": "demo", "collection": "things",
         "message": "demo.Thing", "pattern": "orgs/{org}/projects/{project}/envs/{env}/things/{thing}",
         "parent_pattern": "orgs/{org}/projects/{project}/envs/{env}", "shape": "spec_status", "reconciled": true},
        {"type": "demo.sylphx.com/Settings", "service": "demo", "collection": "settings",
         "message": "demo.Settings", "pattern": "orgs/{org}/projects/{project}/envs/{env}/settings",
         "parent_pattern": "orgs/{org}/projects/{project}/envs/{env}", "shape": "singleton", "reconciled": false}
      ],
      "messages": [
        {"name": "demo.Thing", "fields": [
          {"name": "name", "behaviors": ["IDENTIFIER"], "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "meta", "behaviors": ["OPTIONAL"], "type": {"kind": "message", "name": "demo.Meta"}},
          {"name": "spec", "behaviors": ["OPTIONAL"], "type": {"kind": "message", "name": "demo.ThingSpec"}},
          {"name": "status", "behaviors": ["OUTPUT_ONLY"], "type": {"kind": "message", "name": "demo.Status"}}
        ]},
        {"name": "demo.ThingSpec", "fields": [
          {"name": "size", "behaviors": ["REQUIRED"], "type": {"kind": "scalar", "scalar": "int64"}},
          {"name": "mode", "behaviors": ["OPTIONAL"], "type": {"kind": "enum", "name": "demo.Mode"}},
          {"name": "note", "behaviors": ["OPTIONAL"], "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "tags", "behaviors": ["OPTIONAL"], "repeated": true, "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "password", "behaviors": ["INPUT_ONLY"], "sensitive": true, "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "token", "behaviors": ["OPTIONAL"], "sensitive": true, "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "limits", "behaviors": ["OPTIONAL"], "type": {"kind": "message", "name": "demo.Limits"}}
        ]},
        {"name": "demo.Limits", "fields": [
          {"name": "cpu", "behaviors": ["OPTIONAL"], "type": {"kind": "scalar", "scalar": "int32"}},
          {"name": "ratio", "behaviors": ["OPTIONAL"], "type": {"kind": "scalar", "scalar": "double"}}
        ]},
        {"name": "demo.Settings", "fields": [
          {"name": "name", "behaviors": ["IDENTIFIER"], "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "meta", "behaviors": ["OPTIONAL"], "type": {"kind": "message", "name": "demo.Meta"}},
          {"name": "spec", "behaviors": ["OPTIONAL"], "type": {"kind": "message", "name": "demo.SettingsSpec"}}
        ]},
        {"name": "demo.SettingsSpec", "fields": [
          {"name": "currency", "behaviors": ["REQUIRED"], "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "plans", "behaviors": ["OPTIONAL"], "repeated": true, "type": {"kind": "message", "name": "demo.Plan"}}
        ]},
        {"name": "demo.Plan", "fields": [
          {"name": "key", "behaviors": ["REQUIRED"], "type": {"kind": "scalar", "scalar": "string"}},
          {"name": "amount", "behaviors": ["OPTIONAL"], "type": {"kind": "scalar", "scalar": "int64"}}
        ]},
        {"name": "demo.Meta", "fields": []},
        {"name": "demo.Status", "fields": []}
      ],
      "enums": [
        {"name": "demo.Mode", "values": [
          {"number": 1, "wire": "fast"}, {"number": 2, "wire": "safe"}
        ]}
      ],
      "services": [
        {"collections": [
          {"resource_type": "demo.sylphx.com/Thing", "methods": [
            {"id": "demo.things.get", "kind": "get", "http": {"query_params": []}},
            {"id": "demo.things.list", "kind": "list", "http": {"query_params": ["filter", "page_size", "page_token"]}},
            {"id": "demo.things.update", "kind": "update", "http": {"query_params": ["update_mask", "allow_missing", "validate_only"]}}
          ]},
          {"resource_type": "demo.sylphx.com/Settings", "methods": [
            {"id": "demo.settings.get", "kind": "get", "http": {"query_params": []}},
            {"id": "demo.settings.update", "kind": "update", "http": {"query_params": ["update_mask", "allow_missing", "validate_only"]}}
          ]}
        ]}
      ]
    }"#;

    pub fn registry() -> Registry {
        Registry::from_json(REGISTRY).unwrap()
    }

    pub fn declarable() -> Declarable {
        Declarable::from_toml("kinds = [\"demo.sylphx.com/Thing\", \"demo.sylphx.com/Settings\"]")
            .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn issues_of(toml: &str) -> Vec<String> {
        let d = Declarations::from_toml(toml).unwrap();
        check(&registry(), &declarable(), &d)
            .into_iter()
            .map(|i| i.to_string())
            .collect()
    }

    fn has(issues: &[String], needle: &str) -> bool {
        issues.iter().any(|i| i.contains(needle))
    }

    #[test]
    fn slugs() {
        for ok in ["a", "main", "a-1", &"a".repeat(63)] {
            assert!(is_slug(ok), "{ok}");
        }
        for bad in ["", "1a", "A", "a_b", "a/b", "-a", &"a".repeat(64)] {
            assert!(!is_slug(bad), "{bad}");
        }
    }

    #[test]
    fn a_sound_file_has_no_issues() {
        let issues = issues_of(
            r#"
[[resource]]
kind = "demo.sylphx.com/Thing"
name = "main"
[resource.spec]
size = "10"
mode = "fast"
tags = ["a"]
[resource.spec.limits]
cpu = 2
ratio = 0.5

[[resource]]
kind = "demo.sylphx.com/Settings"
[resource.spec]
currency = "usd"
[[resource.spec.plans]]
key = "plus"
"#,
        );
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn unknown_kind_and_unlisted_kind() {
        let issues = issues_of("[[resource]]\nkind = \"demo.sylphx.com/Nope\"\nname = \"a\"\n");
        assert!(has(&issues, "unknown kind"), "{issues:?}");
        let d = Declarations::from_toml("[[resource]]\nkind = \"demo.sylphx.com/Thing\"\nname = \"a\"\n[resource.spec]\nsize = 1\n")
            .unwrap();
        let none = Declarable::from_toml("kinds = []").unwrap();
        let issues = check(&registry(), &none, &d);
        assert!(issues
            .iter()
            .any(|i| i.message.contains("not declarable yet")));
    }

    #[test]
    fn spec_fields_types_enums_and_required() {
        let issues = issues_of(
            r#"
[[resource]]
kind = "demo.sylphx.com/Thing"
name = "main"
[resource.spec]
mode = "warp"
note = 5
tags = "a"
extra = 1
password = "x"
token = "y"
[resource.spec.limits]
cpu = "many"
"#,
        );
        assert!(has(&issues, "spec.size: is required"), "{issues:?}");
        assert!(
            has(&issues, "spec.mode: `warp` is not one of: fast, safe"),
            "{issues:?}"
        );
        assert!(has(&issues, "spec.note: must be a string"), "{issues:?}");
        assert!(has(&issues, "spec.tags: must be a list"), "{issues:?}");
        assert!(has(&issues, "spec.extra: unknown field"), "{issues:?}");
        assert!(has(&issues, "spec.password: is sensitive"), "{issues:?}");
        assert!(has(&issues, "spec.token: is sensitive"), "{issues:?}");
        assert!(
            has(&issues, "spec.limits.cpu: must be a int32"),
            "{issues:?}"
        );
    }

    #[test]
    fn nested_required_fields_are_checked() {
        let issues = issues_of(
            "[[resource]]\nkind = \"demo.sylphx.com/Settings\"\n[resource.spec]\ncurrency = \"usd\"\n[[resource.spec.plans]]\namount = 5\n",
        );
        assert!(has(&issues, "spec.plans[0].key: is required"), "{issues:?}");
    }

    #[test]
    fn names_follow_the_kind() {
        let issues = issues_of("[[resource]]\nkind = \"demo.sylphx.com/Thing\"\nname = \"Bad_Name\"\n[resource.spec]\nsize = 1\n");
        assert!(has(&issues, "not a slug"), "{issues:?}");
        let issues = issues_of(
            "[[resource]]\nkind = \"demo.sylphx.com/Thing\"\n[resource.spec]\nsize = 1\n",
        );
        assert!(has(&issues, "needs a `name`"), "{issues:?}");
        let issues = issues_of("[[resource]]\nkind = \"demo.sylphx.com/Settings\"\nname = \"x\"\n[resource.spec]\ncurrency = \"usd\"\n");
        assert!(has(&issues, "is a singleton"), "{issues:?}");
    }

    #[test]
    fn duplicates_and_the_removed_table() {
        let issues = issues_of(
            r#"
[[resource]]
kind = "demo.sylphx.com/Thing"
name = "a"
[resource.spec]
size = 1

[[resource]]
kind = "demo.sylphx.com/Thing"
name = "a"
[resource.spec]
size = 1

[[removed]]
kind = "demo.sylphx.com/Thing"
name = "a"

[[removed]]
kind = "demo.sylphx.com/Missing"
name = "b"
"#,
        );
        assert!(has(&issues, "declared twice"), "{issues:?}");
        assert!(
            has(&issues, "in both [[resource]] and [[removed]]"),
            "{issues:?}"
        );
        assert!(
            has(&issues, "unknown kind `demo.sylphx.com/Missing`"),
            "{issues:?}"
        );
    }

    #[test]
    fn reserved_labels_are_refused() {
        let issues = issues_of(
            "[[resource]]\nkind = \"demo.sylphx.com/Thing\"\nname = \"a\"\n[resource.meta.labels]\nsylphx-managed-by = \"me\"\n[resource.spec]\nsize = 1\n",
        );
        assert!(has(&issues, "reserved"), "{issues:?}");
    }

    #[test]
    fn owned_kinds_are_refused_offline_with_their_owner() {
        let issues =
            issues_of("[[resource]]\nkind = \"hosting.sylphx.com/Service\"\nname = \"web\"\n");
        assert!(has(&issues, "[[services]]"), "{issues:?}");
    }
}
