//! Compiles every method's `google.api.http` template into a matcher.

use std::collections::HashMap;

use crate::ir::Index;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Seg {
    /// A literal segment, inside variable `.1` when it is part of one
    /// (`orgs` in `{name=orgs/*}`).
    Lit(String, Option<usize>),
    /// One `*` segment of the variable `var` (index into `vars`).
    Star(usize),
}

#[derive(Debug, Clone)]
struct Route {
    segs: Vec<Seg>,
    verb: Option<String>,
    vars: Vec<String>,
    method: usize,
}

/// A matched request: the method (index into `Index::methods`) and the
/// path variables as `(request field path, value)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub method: usize,
    pub vars: Vec<(String, String)>,
}

/// Every route, bucketed by `(HTTP method, segment count, custom verb)`.
#[derive(Debug, Default)]
pub struct Router {
    buckets: HashMap<(String, usize, Option<String>), Vec<Route>>,
}

impl Router {
    pub fn new(ix: &Index) -> Result<Self, String> {
        let mut r = Router::default();
        for (i, (m, _)) in ix.methods.iter().enumerate() {
            for template in bindings(&m.http) {
                let route = compile(&template, i).map_err(|e| format!("{}: {e}", m.id))?;
                r.buckets
                    .entry((m.http.method.clone(), route.segs.len(), route.verb.clone()))
                    .or_default()
                    .push(route);
            }
        }
        // Literal segments beat stars: the most specific route first.
        for routes in r.buckets.values_mut() {
            routes.sort_by_key(|r| {
                std::cmp::Reverse(r.segs.iter().filter(|s| matches!(s, Seg::Lit(..))).count())
            });
        }
        Ok(r)
    }

    /// Matches `path` (beginning `/v1/`) for `method`.
    pub fn find(&self, method: &str, path: &str) -> Option<Matched> {
        let (segs, verb) = split_path(path)?;
        let routes = self.buckets.get(&(method.to_string(), segs.len(), verb))?;
        'routes: for r in routes {
            let mut vals: Vec<Vec<&str>> = vec![vec![]; r.vars.len()];
            for (s, got) in r.segs.iter().zip(&segs) {
                match s {
                    Seg::Lit(l, Some(v)) if l == got => vals[*v].push(got),
                    Seg::Lit(l, None) if l == got => {}
                    Seg::Lit(..) => continue 'routes,
                    Seg::Star(v) if valid_id(got) => vals[*v].push(got),
                    Seg::Star(_) => continue 'routes,
                }
            }
            return Some(Matched {
                method: r.method,
                vars: r
                    .vars
                    .iter()
                    .cloned()
                    .zip(vals.into_iter().map(|v| v.join("/")))
                    .collect(),
            });
        }
        None
    }
}

/// The template and one more per additional pattern (AIP-127): the n-th
/// binding puts each variable's n-th alternative in place of its pattern.
fn bindings(http: &crate::ir::Http) -> Vec<String> {
    let n = http
        .path_params
        .iter()
        .map(|p| p.alt_patterns.len())
        .max()
        .unwrap_or(0);
    (0..=n)
        .map(|v| {
            let mut t = http.template.clone();
            if v > 0 {
                for p in &http.path_params {
                    if let Some(alt) = p.alt_patterns.get(v - 1) {
                        t = t.replace(
                            &format!("{{{}={}}}", p.field, p.pattern),
                            &format!("{{{}={}}}", p.field, alt.pattern),
                        );
                    }
                }
            }
            t
        })
        .collect()
}

/// `/v1/a/b:verb` -> (`["a", "b"]`, `Some("verb")`).
pub fn split_path(path: &str) -> Option<(Vec<&str>, Option<String>)> {
    let rest = path.strip_prefix("/v1/")?;
    let (body, verb) = match rest.rsplit_once(':') {
        Some((b, v)) if !v.contains('/') => (b, Some(v.to_string())),
        _ => (rest, None),
    };
    let segs: Vec<&str> = body.split('/').collect();
    if segs.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some((segs, verb))
}

/// An id or collection segment: printable, no reserved characters. The
/// exact id grammar is checked by the schema layer; `-` is the List wildcard.
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

fn compile(template: &str, method: usize) -> Result<Route, String> {
    let rest = template
        .strip_prefix("/v1/")
        .ok_or("template does not start with /v1/")?;
    // A `:verb` suffix follows the last `}` or the last literal.
    let (body, verb) = match rest.rfind(':') {
        Some(i) if !rest[i..].contains('}') && !rest[i..].contains('/') => {
            (&rest[..i], Some(rest[i + 1..].to_string()))
        }
        _ => (rest, None),
    };
    let mut segs = Vec::new();
    let mut vars = Vec::new();
    let mut i = 0;
    let bytes = body.as_bytes();
    while i < body.len() {
        if bytes[i] == b'/' {
            i += 1;
            continue;
        }
        if bytes[i] == b'{' {
            let end = body[i..].find('}').ok_or("unclosed {")? + i;
            let inner = &body[i + 1..end];
            let (field, pattern) = inner.split_once('=').unwrap_or((inner, "*"));
            let v = vars.len();
            vars.push(field.to_string());
            for p in pattern.split('/') {
                segs.push(if p == "*" {
                    Seg::Star(v)
                } else {
                    Seg::Lit(p.to_string(), Some(v))
                });
            }
            i = end + 1;
        } else {
            let end = body[i..].find('/').map(|e| e + i).unwrap_or(body.len());
            segs.push(Seg::Lit(body[i..end].to_string(), None));
            i = end;
        }
    }
    Ok(Route {
        segs,
        verb,
        vars,
        method,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(ix: &Index, m: Option<Matched>) -> Option<(String, Vec<(String, String)>)> {
        m.map(|m| (ix.methods[m.method].0.id.clone(), m.vars))
    }

    #[test]
    fn routes_match_their_methods() {
        let ix = Index::embedded();
        let r = Router::new(&ix).unwrap();
        let (id, vars) = id_of(&ix, r.find("GET", "/v1/orgs/org_a/projects/prj_b")).unwrap();
        assert_eq!(id, "access.projects.get");
        assert_eq!(
            vars,
            vec![("name".into(), "orgs/org_a/projects/prj_b".into())]
        );
        let (id, vars) = id_of(&ix, r.find("GET", "/v1/orgs/org_a/projects")).unwrap();
        assert_eq!(id, "access.projects.list");
        assert_eq!(vars, vec![("parent".into(), "orgs/org_a".into())]);
        let (id, _) = id_of(&ix, r.find("GET", "/v1/orgs")).unwrap();
        assert_eq!(id, "access.orgs.list");
        // An additional binding (AIP-127): org-wide keys under the org.
        let (id, vars) = id_of(&ix, r.find("POST", "/v1/orgs/org_a/api_keys")).unwrap();
        assert_eq!(id, "access.api_keys.create");
        assert_eq!(vars, vec![("parent".into(), "orgs/org_a".into())]);
        let (id, vars) = id_of(&ix, r.find("POST", "/v1/orgs/org_a/api_keys/k:roll")).unwrap();
        assert_eq!(id, "access.api_keys.roll");
        assert_eq!(vars, vec![("name".into(), "orgs/org_a/api_keys/k".into())]);
        assert!(ix.resource_of_name("orgs/org_a/api_keys/k").is_some());
        let (id, _) = id_of(&ix, r.find("GET", "/v1/whoami")).unwrap();
        assert_eq!(id, "access.whoami");
        let (id, vars) = id_of(
            &ix,
            r.find("POST", "/v1/orgs/o/projects/p/envs/e/api_keys/k:revoke"),
        )
        .unwrap();
        assert_eq!(id, "access.api_keys.revoke");
        assert_eq!(vars[0].1, "orgs/o/projects/p/envs/e/api_keys/k");
        let (id, vars) = id_of(&ix, r.find("PATCH", "/v1/orgs/o")).unwrap();
        assert_eq!(id, "access.orgs.update");
        assert_eq!(vars, vec![("org.name".into(), "orgs/o".into())]);
        assert!(r.find("GET", "/v1/nothing/here").is_none());
        assert!(r.find("GET", "/v1/orgs/o/projects/p:nope").is_none());
        assert!(r.find("GET", "/v1/orgs//projects").is_none());
    }
}
