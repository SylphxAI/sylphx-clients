//! The schema registry the offline check and the planner read: the IR that
//! `sylphx-gen` emits (`contracts/generated/registry.json`), which the
//! generated clients are built from. Only the parts `apply` needs are decoded.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

/// The IR this build of the crate was generated with.
const EMBEDDED: &str = include_str!("../generated/registry.json");

/// A `null` in the IR (for example the `parent_pattern` of a top-level
/// Resource) reads as the type's default, like a missing field.
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// One registered Resource type.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ResourceType {
    /// `money.sylphx.com/Catalog`.
    #[serde(default, rename = "type", deserialize_with = "null_default")]
    pub type_name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub service: String,
    #[serde(default, deserialize_with = "null_default")]
    pub collection: String,
    /// The full message name (`sylphx.money.v1.Catalog`).
    #[serde(default, deserialize_with = "null_default")]
    pub message: String,
    /// `orgs/{org}/projects/{project}/envs/{env}/databases/{database}`.
    #[serde(default, deserialize_with = "null_default")]
    pub pattern: String,
    #[serde(default, deserialize_with = "null_default")]
    pub parent_pattern: String,
    /// `spec_status`, `record`, or `singleton`.
    #[serde(default, deserialize_with = "null_default")]
    pub shape: String,
    /// The type has a controller that reconciles a generation (`status`).
    #[serde(default)]
    pub reconciled: bool,
}

/// The environment parent every declarable kind lives under.
pub const ENV_PARENT: &str = "orgs/{org}/projects/{project}/envs/{env}";

impl ResourceType {
    /// An AIP-156 singleton has no id segment: its pattern ends in a literal
    /// (`…/envs/{env}/catalog`).
    pub fn is_singleton(&self) -> bool {
        self.shape == "singleton" && !self.last_segment().starts_with('{')
    }

    /// The last segment of the pattern: `{database}`, or `catalog`.
    pub fn last_segment(&self) -> &str {
        self.pattern.rsplit('/').next().unwrap_or_default()
    }

    /// The Resource's name below the environment `env` (a full environment
    /// name): `env/collection/slug`, or `env/catalog` for a singleton.
    pub fn name_in(&self, env: &str, slug: Option<&str>) -> Result<String, String> {
        if self.parent_pattern != ENV_PARENT {
            return Err(format!(
                "`{}` is not below an environment (its parent is `{}`)",
                self.type_name, self.parent_pattern
            ));
        }
        match (self.is_singleton(), slug) {
            (true, None) => Ok(format!("{env}/{}", self.last_segment())),
            (true, Some(_)) => Err(format!(
                "`{}` is a singleton: it has no `name`",
                self.type_name
            )),
            (false, Some(s)) => Ok(format!("{env}/{}/{s}", self.collection)),
            (false, None) => Err(format!("`{}` needs a `name`", self.type_name)),
        }
    }
}

/// A scalar, message, enum, well-known, or map type.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FieldType {
    Scalar {
        scalar: String,
    },
    Message {
        name: String,
    },
    Enum {
        name: String,
    },
    WellKnown {
        #[serde(default)]
        wkt: String,
    },
    Map {
        value: Box<FieldType>,
    },
}

/// One message field.
#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub name: String,
    #[serde(default)]
    pub behaviors: Vec<String>,
    #[serde(default)]
    pub sensitive: bool,
    #[serde(default)]
    pub repeated: bool,
    /// Explicit presence (`optional`).
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub oneof: Option<String>,
    #[serde(rename = "type")]
    pub ty: FieldType,
}

impl Field {
    pub fn has(&self, behavior: &str) -> bool {
        self.behaviors.iter().any(|b| b == behavior)
    }

    /// Written by the server or never returned: not something a declaration
    /// can carry.
    pub fn is_server_side(&self) -> bool {
        self.has("OUTPUT_ONLY") || self.has("IDENTIFIER")
    }

    /// Never part of the managed state: secrets and input that is not read
    /// back (spec §3).
    pub fn is_unmanaged(&self) -> bool {
        self.sensitive || self.has("INPUT_ONLY") || self.is_server_side()
    }
}

/// A message.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    pub name: String,
    #[serde(default)]
    pub fields: Vec<Field>,
}

impl Message {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }
}

/// One enum value.
#[derive(Debug, Clone, Deserialize)]
pub struct EnumValue {
    pub number: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub wire: String,
}

/// An enum.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Enum {
    pub name: String,
    #[serde(default)]
    pub values: Vec<EnumValue>,
}

/// The HTTP binding of a method, as far as eligibility reads it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Http {
    #[serde(default)]
    pub query_params: Vec<String>,
}

/// One method.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Method {
    #[serde(default, deserialize_with = "null_default")]
    pub id: String,
    /// `get`, `list`, `create`, `update`, `delete`, or `custom`.
    #[serde(default, deserialize_with = "null_default")]
    pub kind: String,
    #[serde(default)]
    pub http: Http,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Collection {
    #[serde(default)]
    resource_type: String,
    #[serde(default)]
    methods: Vec<Method>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct Service {
    #[serde(default)]
    collections: Vec<Collection>,
}

#[derive(Debug, Deserialize)]
struct Raw {
    #[serde(default)]
    resources: Vec<ResourceType>,
    #[serde(default)]
    messages: Vec<Message>,
    #[serde(default)]
    enums: Vec<Enum>,
    #[serde(default)]
    services: Vec<Service>,
}

/// The registry: Resource types, their messages and enums, and their methods.
#[derive(Debug, Default)]
pub struct Registry {
    resources: Vec<ResourceType>,
    messages: HashMap<String, Message>,
    enums: HashMap<String, Enum>,
    methods: HashMap<String, Vec<Method>>,
}

impl Registry {
    /// The registry this crate was built with (parsed once).
    pub fn embedded() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| {
            Registry::from_json(EMBEDDED).expect("contracts/generated/registry.json is valid")
        })
    }

    /// A registry from IR JSON (the embedded one, or a test fixture).
    pub fn from_json(text: &str) -> Result<Registry, String> {
        let raw: Raw = serde_json::from_str(text).map_err(|e| format!("registry: {e}"))?;
        let mut methods: HashMap<String, Vec<Method>> = HashMap::new();
        for s in raw.services {
            for c in s.collections {
                if !c.resource_type.is_empty() {
                    methods
                        .entry(c.resource_type)
                        .or_default()
                        .extend(c.methods);
                }
            }
        }
        Ok(Registry {
            resources: raw.resources,
            messages: raw
                .messages
                .into_iter()
                .map(|m| (m.name.clone(), m))
                .collect(),
            enums: raw.enums.into_iter().map(|e| (e.name.clone(), e)).collect(),
            methods,
        })
    }

    /// A Resource type by `service.sylphx.com/Kind`.
    pub fn resource(&self, type_name: &str) -> Option<&ResourceType> {
        self.resources.iter().find(|r| r.type_name == type_name)
    }

    pub fn resources(&self) -> &[ResourceType] {
        &self.resources
    }

    pub fn message(&self, name: &str) -> Option<&Message> {
        self.messages.get(name)
    }

    pub fn enum_(&self, name: &str) -> Option<&Enum> {
        self.enums.get(name)
    }

    /// The method of `kind` (`get`, `update`, …) of a Resource type.
    pub fn method(&self, type_name: &str, kind: &str) -> Option<&Method> {
        self.methods.get(type_name)?.iter().find(|m| m.kind == kind)
    }

    /// The Spec message of a Resource type: the message of its `spec` field.
    pub fn spec_message(&self, rt: &ResourceType) -> Option<&Message> {
        let resource = self.message(&rt.message)?;
        match &resource.field("spec")?.ty {
            FieldType::Message { name } => self.message(name),
            _ => None,
        }
    }

    /// The top-level `spec` fields a declaration manages, in declaration
    /// order: neither sensitive, INPUT_ONLY, OUTPUT_ONLY, nor an identifier.
    pub fn managed_fields(&self, rt: &ResourceType) -> Vec<&Field> {
        self.spec_message(rt)
            .map(|m| m.fields.iter().filter(|f| !f.is_unmanaged()).collect())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_registry_decodes() {
        let reg = Registry::embedded();
        let service = reg.resource("hosting.sylphx.com/Service").expect("Service");
        assert_eq!(service.parent_pattern, ENV_PARENT);
        assert!(!service.is_singleton());
        assert!(!reg.managed_fields(service).is_empty());
        assert!(reg.method("hosting.sylphx.com/Service", "update").is_some());
        assert_eq!(
            service
                .name_in("orgs/o/projects/p/envs/e", Some("web"))
                .unwrap(),
            "orgs/o/projects/p/envs/e/services/web"
        );
    }

    #[test]
    fn a_singleton_is_named_by_its_literal_segment() {
        let rt = ResourceType {
            type_name: "demo.sylphx.com/Catalog".into(),
            collection: "catalog".into(),
            pattern: "orgs/{org}/projects/{project}/envs/{env}/catalog".into(),
            parent_pattern: ENV_PARENT.into(),
            shape: "singleton".into(),
            ..ResourceType::default()
        };
        assert!(rt.is_singleton());
        assert_eq!(
            rt.name_in("orgs/o/projects/p/envs/e", None).unwrap(),
            "orgs/o/projects/p/envs/e/catalog"
        );
        assert!(rt.name_in("orgs/o/projects/p/envs/e", Some("x")).is_err());
    }
}
