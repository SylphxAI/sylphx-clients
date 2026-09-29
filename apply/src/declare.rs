//! Declarations: the `[[resource]]` and `[[removed]]` tables of a
//! `sylphx.toml`, or the equivalent JSON the Release step mounts
//! (spec §2). Every other table of the file is somebody else's and is ignored.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::error::ApplyError;

/// `meta` of a declaration: the caller-writable metadata apply manages.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclMeta {
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
}

/// One `[[resource]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Declaration {
    /// The registry's resource type: `money.sylphx.com/Catalog`.
    pub kind: String,
    /// A slug; absent for a singleton.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub meta: DeclMeta,
    /// The Spec message in wire JSON.
    #[serde(default)]
    pub spec: Map<String, Value>,
    /// Allows taking over an unmanaged Resource whose managed fields differ.
    #[serde(default)]
    pub import: bool,
}

/// One `[[removed]]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Removed {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub destroy: bool,
}

/// Everything a file declares.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Declarations {
    pub resources: Vec<Declaration>,
    pub removed: Vec<Removed>,
}

#[derive(Deserialize)]
struct Root {
    #[serde(default)]
    resource: Vec<Declaration>,
    #[serde(default)]
    removed: Vec<Removed>,
}

impl Declarations {
    /// Parses a `sylphx.toml`.
    pub fn from_toml(text: &str) -> Result<Self, ApplyError> {
        let value: toml::Value =
            toml::from_str(text).map_err(|e| ApplyError::Parse(format!("sylphx.toml: {e}")))?;
        let json = serde_json::to_value(value)
            .map_err(|e| ApplyError::Parse(format!("sylphx.toml: {e}")))?;
        Self::from_value(json)
    }

    /// Parses the JSON form: `{"resource": [...], "removed": [...]}`.
    pub fn from_json(text: &str) -> Result<Self, ApplyError> {
        let json: Value = serde_json::from_str(text)
            .map_err(|e| ApplyError::Parse(format!("declarations: {e}")))?;
        Self::from_value(json)
    }

    /// Parses the decoded document (the tables under their TOML names).
    pub fn from_value(value: Value) -> Result<Self, ApplyError> {
        let root: Root = serde_json::from_value(value)
            .map_err(|e| ApplyError::Parse(format!("declarations: {e}")))?;
        Ok(Declarations {
            resources: root.resource,
            removed: root.removed,
        })
    }

    /// Whether the file declares nothing.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty() && self.removed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOML: &str = r#"
[project]
name = "ignored"

[[services]]
name = "web"

[[resource]]
kind = "money.sylphx.com/Catalog"

[resource.spec]
features = [{ key = "plus", kind = "boolean" }]

[[resource.spec.products]]
key = "plus_monthly"

[[resource]]
kind = "data.sylphx.com/Widget"
name = "main"
import = true

[resource.meta]
display_name = "Main"

[resource.meta.labels]
team = "core"

[[removed]]
kind = "data.sylphx.com/Widget"
name = "old"
destroy = false
"#;

    #[test]
    fn toml_tables_parse_and_other_tables_are_ignored() {
        let d = Declarations::from_toml(TOML).unwrap();
        assert_eq!(d.resources.len(), 2);
        assert_eq!(d.resources[0].kind, "money.sylphx.com/Catalog");
        assert_eq!(d.resources[0].name, None);
        assert_eq!(d.resources[0].spec["products"][0]["key"], "plus_monthly");
        assert_eq!(d.resources[1].name.as_deref(), Some("main"));
        assert!(d.resources[1].import);
        assert_eq!(d.resources[1].meta.labels["team"], "core");
        assert_eq!(d.resources[1].meta.display_name.as_deref(), Some("Main"));
        assert_eq!(d.removed.len(), 1);
        assert!(!d.removed[0].destroy);
    }

    #[test]
    fn json_and_toml_are_the_same_document() {
        let from_toml = Declarations::from_toml(TOML).unwrap();
        let json = serde_json::json!({
            "resource": from_toml.resources,
            "removed": from_toml.removed,
        });
        let from_json = Declarations::from_json(&json.to_string()).unwrap();
        assert_eq!(from_toml, from_json);
    }

    #[test]
    fn unknown_fields_of_a_resource_are_errors() {
        let e = Declarations::from_toml("[[resource]]\nkind = \"a.sylphx.com/B\"\nnaem = \"x\"\n")
            .unwrap_err();
        assert!(e.to_string().contains("naem"), "{e}");
    }

    #[test]
    fn a_resource_needs_a_kind() {
        assert!(Declarations::from_toml("[[resource]]\nname = \"x\"\n").is_err());
    }

    #[test]
    fn an_empty_file_declares_nothing() {
        assert!(Declarations::from_toml("").unwrap().is_empty());
        assert!(Declarations::from_json("{}").unwrap().is_empty());
    }
}
