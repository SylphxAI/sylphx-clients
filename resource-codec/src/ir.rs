//! The registry IR (`generated/registry.json`, spec §8.2), read at
//! startup. The front is generic over it: every route, schema check, and
//! storage decision comes from here, never from generated types.

use std::collections::HashMap;

use serde::Deserialize;

/// The IR the front was built with.
pub const EMBEDDED: &str = include_str!("../generated/registry.json");
/// The OpenAPI document served at `/v1/openapi.json`.
pub const OPENAPI: &str = include_str!("../generated/openapi.json");

#[derive(Debug, Clone, Deserialize)]
pub struct Registry {
    pub ir_version: u32,
    pub services: Vec<Service>,
    pub resources: Vec<Resource>,
    pub messages: Vec<Message>,
    pub enums: Vec<Enum>,
    pub error_codes: Vec<ErrorCode>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Service {
    pub name: String,
    pub display_name: String,
    pub stage: String,
    pub package: String,
    pub collections: Vec<Collection>,
    #[serde(default)]
    pub methods: Vec<Method>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Collection {
    pub name: String,
    pub resource_type: String,
    pub methods: Vec<Method>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodKind {
    Get,
    List,
    Create,
    Update,
    Delete,
    Custom,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Method {
    pub id: String,
    pub kind: MethodKind,
    pub verb: String,
    pub http: Http,
    pub request: String,
    pub response: String,
    pub lro: Option<Lro>,
    pub resource_field: Option<String>,
    pub page: Option<Page>,
    pub permission: String,
    pub effect: String,
    pub publishable_ok: bool,
    pub supports_validate_only: bool,
    pub cancellable: bool,
    pub errors: Vec<String>,
    /// Set for a protocol method (`ServiceInfo.origin`): served by its
    /// service's own host, never by the Resource API.
    #[serde(default)]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Http {
    pub method: String,
    pub template: String,
    pub body: Option<String>,
    pub path_params: Vec<PathParam>,
    pub query_params: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PathParam {
    pub field: String,
    pub pattern: String,
    /// Further patterns of the same variable (AIP-127 additional bindings).
    #[serde(default)]
    pub alt_patterns: Vec<AltPattern>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AltPattern {
    pub pattern: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Lro {
    pub response: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Page {
    pub items_field: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Resource {
    pub r#type: String,
    pub kind: String,
    pub service: String,
    pub message: String,
    pub pattern: String,
    pub parent_pattern: Option<String>,
    /// AIP-156 singleton: `pattern` ends in the literal `collection`.
    #[serde(default)]
    pub singleton: bool,
    /// Further patterns under other parents (AIP-123), or for a singleton
    /// also its `collection/{id}` aliases.
    #[serde(default)]
    pub alt_patterns: Vec<String>,
    pub collection: String,
    pub singular: String,
    pub storage: String,
    pub shape: String,
    pub reconciled: bool,
    pub controller: String,
    pub deletion_protection: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub name: String,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Field {
    pub name: String,
    pub r#type: Type,
    pub repeated: bool,
    pub behaviors: Vec<String>,
    pub filterable: bool,
}

impl Field {
    pub fn has(&self, b: &str) -> bool {
        self.behaviors.iter().any(|x| x == b)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Type {
    Scalar { scalar: String },
    Message { name: String },
    Enum { name: String },
    WellKnown { wkt: String },
    Map { value: Box<Type> },
}

#[derive(Debug, Clone, Deserialize)]
pub struct Enum {
    pub name: String,
    pub values: Vec<EnumValue>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnumValue {
    pub wire: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ErrorCode {
    pub code: String,
    pub grpc_status: String,
    pub http_status: u16,
}

/// The IR plus the indices the request path needs.
#[derive(Debug)]
pub struct Index {
    pub reg: Registry,
    messages: HashMap<String, usize>,
    enums: HashMap<String, usize>,
    by_collection: HashMap<String, usize>,
    /// `(method, service name)` for every method.
    pub methods: Vec<(Method, String)>,
}

impl Index {
    pub fn parse(json: &str) -> Result<Self, String> {
        let mut reg: Registry =
            serde_json::from_str(json).map_err(|e| format!("registry.json: {e}"))?;
        // Protocol methods (Data's data plane, the Auth client API) are served
        // at their service's own origin; the front neither routes nor
        // authorizes them, so they are not part of its index.
        for s in &mut reg.services {
            s.methods.retain(|m| m.origin.is_none());
            for c in &mut s.collections {
                c.methods.retain(|m| m.origin.is_none());
            }
            s.collections.retain(|c| !c.methods.is_empty());
        }
        if reg.ir_version != 2 {
            return Err(format!(
                "registry.json: ir_version {} is not 2",
                reg.ir_version
            ));
        }
        let messages = reg
            .messages
            .iter()
            .enumerate()
            .map(|(i, m)| (m.name.clone(), i))
            .collect();
        let enums = reg
            .enums
            .iter()
            .enumerate()
            .map(|(i, e)| (e.name.clone(), i))
            .collect();
        let by_collection = reg
            .resources
            .iter()
            .enumerate()
            .map(|(i, r)| (r.collection.clone(), i))
            .collect();
        let mut methods = Vec::new();
        for s in &reg.services {
            for m in &s.methods {
                methods.push((m.clone(), s.name.clone()));
            }
            for c in &s.collections {
                for m in &c.methods {
                    methods.push((m.clone(), s.name.clone()));
                }
            }
        }
        Ok(Self {
            reg,
            messages,
            enums,
            by_collection,
            methods,
        })
    }

    pub fn embedded() -> Self {
        Self::parse(EMBEDDED).expect("embedded registry.json parses")
    }

    pub fn message(&self, name: &str) -> Option<&Message> {
        self.messages.get(name).map(|i| &self.reg.messages[*i])
    }

    pub fn enum_(&self, name: &str) -> Option<&Enum> {
        self.enums.get(name).map(|i| &self.reg.enums[*i])
    }

    pub fn resource_by_collection(&self, c: &str) -> Option<&Resource> {
        self.by_collection.get(c).map(|i| &self.reg.resources[*i])
    }

    /// The Resource a full name (`orgs/o/projects/p`) addresses: the one whose
    /// pattern matches segment for segment. A singleton's name ends in its
    /// literal (`orgs/o/entitlement`), and an alias of it in an id
    /// (`orgs/o/catalogs/main`, which names the same Resource).
    pub fn resource_of_name(&self, name: &str) -> Option<&Resource> {
        let segs: Vec<&str> = name.split('/').collect();
        if segs.len() < 2 {
            return None;
        }
        let collection = if segs.len().is_multiple_of(2) {
            segs[segs.len() - 2]
        } else {
            segs[segs.len() - 1]
        };
        let r = self.resource_by_collection(collection).or_else(|| {
            // An alias of a singleton is filed under the singleton's literal.
            self.reg
                .resources
                .iter()
                .find(|r| r.singleton && r.alt_patterns.iter().any(|p| pattern_matches(p, &segs)))
        })?;
        std::iter::once(&r.pattern)
            .chain(&r.alt_patterns)
            .any(|p| pattern_matches(p, &segs))
            .then_some(r)
    }

    pub fn error_code(&self, code: &str) -> Option<&ErrorCode> {
        self.reg.error_codes.iter().find(|c| c.code == code)
    }

    pub fn service(&self, name: &str) -> Option<&Service> {
        self.reg.services.iter().find(|s| s.name == name)
    }
}

/// Whether `segs` fills a `{var}` pattern segment for segment.
pub fn pattern_matches(pattern: &str, segs: &[&str]) -> bool {
    let p: Vec<&str> = pattern.split('/').collect();
    p.len() == segs.len()
        && p.iter().zip(segs).all(|(p, s)| {
            if p.starts_with('{') {
                !s.is_empty()
            } else {
                p == s
            }
        })
}

/// The parent name of a Resource name, `None` at the top level. A
/// singleton's name (an odd number of segments, `orgs/o/entitlement`) drops
/// its one literal segment.
pub fn parent_of(name: &str) -> Option<&str> {
    let mut it = name.rmatch_indices('/');
    let (last, _) = it.next()?;
    if name.split('/').count() % 2 == 1 {
        return Some(&name[..last]);
    }
    it.next().map(|(i, _)| &name[..i])
}

/// The org id segment of a name under `orgs/{org}`.
pub fn org_of(name: &str) -> Option<&str> {
    let mut s = name.split('/');
    (s.next() == Some("orgs")).then(|| s.next()).flatten()
}

/// `(org, project, env)` segments of a name, where present.
pub fn scope_of(name: &str) -> (Option<&str>, Option<&str>, Option<&str>) {
    let s: Vec<&str> = name.split('/').collect();
    let at = |i: usize, lit: &str| {
        (s.get(i) == Some(&lit))
            .then(|| s.get(i + 1).copied())
            .flatten()
    };
    (at(0, "orgs"), at(2, "projects"), at(4, "envs"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_free_ir_v2_parses() {
        let json = r#"{"ir_version":2,"services":[],"resources":[],"messages":[],"enums":[],"error_codes":[]}"#;
        let ix = Index::parse(json).unwrap();
        assert_eq!(ix.reg.ir_version, 2);
        assert!(ix.methods.is_empty());
    }

    #[test]
    fn only_ir_v2_is_accepted() {
        for version in [0, 1, 3, u32::MAX] {
            let json = serde_json::json!({
                "ir_version": version,
                "services": [], "resources": [], "messages": [],
                "enums": [], "error_codes": []
            });
            assert_eq!(
                Index::parse(&json.to_string()).unwrap_err(),
                format!("registry.json: ir_version {version} is not 2")
            );
        }
    }

    #[test]
    fn embedded_registry_has_no_global_provenance() {
        let reg: serde_json::Value = serde_json::from_str(EMBEDDED).unwrap();
        assert_eq!(reg["ir_version"], 2);
        assert!(reg.get("generator").is_none());
        assert!(reg.get("image_sha256").is_none());
        assert_eq!(reg["generated"], "@generated by sylphx-gen. Do not edit.");
    }

    #[test]
    fn the_embedded_registry_indexes() {
        let ix = Index::embedded();
        assert!(ix.methods.iter().any(|(m, _)| m.id == "access.whoami"));
        // Protocol methods are served at their own origin, not indexed here.
        assert!(!ix.methods.iter().any(|(m, _)| m.id == "data.objects.put"));
        assert!(ix
            .service("data")
            .is_some_and(|s| s.collections.iter().all(|c| c.name != "objects")));
        let r = ix
            .resource_of_name("orgs/o/projects/p/envs/e/databases/main")
            .unwrap();
        assert_eq!(r.kind, "Database");
        assert!(ix.resource_of_name("orgs/o/databases/main").is_none());
        assert_eq!(parent_of("orgs/o/projects/p"), Some("orgs/o"));
        assert_eq!(parent_of("orgs/o"), None);
        assert_eq!(parent_of("orgs/o/entitlement"), Some("orgs/o"));
        assert_eq!(
            scope_of("orgs/o/projects/p/envs/e/x/y"),
            (Some("o"), Some("p"), Some("e"))
        );
    }
}
