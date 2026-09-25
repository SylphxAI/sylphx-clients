//! Slugs in names. Resource names carry Access ids
//! (`orgs/org_fs01…/projects/prj_…/envs/env_…`); people type slugs
//! (`--project web --env production`). A project or environment segment that
//! is not an id is resolved against Access's lists; an org is the key's own
//! (or given as an id).

use serde_json::json;
use sylphx::Client;

use crate::Failure;

fn is_id(segment: &str, kind: &str) -> bool {
    segment.starts_with(&format!("{kind}_"))
}

async fn pick(
    client: &Client,
    method: &str,
    parent: &str,
    items: &str,
    want: &str,
    what: &str,
) -> Result<String, Failure> {
    let listed = client.invoke(method, json!({ "parent": parent })).await?;
    let rows = listed[items].as_array().cloned().unwrap_or_default();
    let found = rows
        .iter()
        .find(|r| r["spec"]["slug"].as_str() == Some(want) || r["uid"].as_str() == Some(want));
    match found.and_then(|r| r["name"].as_str()) {
        Some(n) => Ok(n.to_string()),
        None => {
            let known: Vec<&str> = rows
                .iter()
                .filter_map(|r| r["spec"]["slug"].as_str())
                .collect();
            Err(Failure::Usage(format!(
                "no {what} `{want}` under {parent} (known: {})",
                if known.is_empty() {
                    "none".to_string()
                } else {
                    known.join(", ")
                }
            )))
        }
    }
}

/// The org's full name from `orgs/x`, `x`, or the key's own org.
pub fn org_name(value: &str) -> String {
    let v = value.trim_start_matches("orgs/");
    format!("orgs/{v}")
}

/// Resolves slug segments of an `orgs/…/projects/…/envs/…` prefix of `name`
/// to ids; everything after the environment is kept as given.
pub async fn resolve(client: &Client, name: &str) -> Result<String, Failure> {
    let segs: Vec<&str> = name.split('/').collect();
    if segs.len() < 2 || segs[0] != "orgs" {
        return Ok(name.to_string());
    }
    let mut out = format!("orgs/{}", segs[1]);
    if segs.len() >= 4 && segs[2] == "projects" {
        out = if is_id(segs[3], "prj") {
            format!("{out}/projects/{}", segs[3])
        } else {
            pick(
                client,
                "access.projects.list",
                &out,
                "projects",
                segs[3],
                "project",
            )
            .await?
        };
        if segs.len() >= 6 && segs[4] == "envs" {
            out = if is_id(segs[5], "env") {
                format!("{out}/envs/{}", segs[5])
            } else {
                pick(
                    client,
                    "access.envs.list",
                    &out,
                    "envs",
                    segs[5],
                    "environment",
                )
                .await?
            };
            if segs.len() > 6 {
                out = format!("{out}/{}", segs[6..].join("/"));
            }
        } else if segs.len() > 4 {
            out = format!("{out}/{}", segs[4..].join("/"));
        }
    } else if segs.len() > 2 {
        out = format!("{out}/{}", segs[2..].join("/"));
    }
    Ok(out)
}

/// `link`'s three flags into full names (each may be a slug, an id, or a
/// full name), resolved in order.
pub async fn link_names(
    client: &Client,
    org: &str,
    project: Option<&str>,
    env: Option<&str>,
) -> Result<(String, Option<String>, Option<String>), Failure> {
    let org = org_name(org);
    let Some(project) = project else {
        return Ok((org, None, None));
    };
    let project_seg = project.rsplit('/').next().unwrap_or(project);
    let project = resolve(client, &format!("{org}/projects/{project_seg}")).await?;
    let env = match env {
        None => None,
        Some(e) => {
            let e = e.rsplit('/').next().unwrap_or(e);
            Some(resolve(client, &format!("{project}/envs/{e}")).await?)
        }
    };
    Ok((org, Some(project), env))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn org_names_normalize() {
        assert_eq!(org_name("orgs/org_a"), "orgs/org_a");
        assert_eq!(org_name("org_a"), "orgs/org_a");
    }

    #[test]
    fn ids_are_recognized_by_prefix() {
        assert!(is_id("prj_fs01abc", "prj"));
        assert!(!is_id("web", "prj"));
    }
}
