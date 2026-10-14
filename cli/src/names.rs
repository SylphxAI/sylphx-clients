//! Slugs in names. Resource names carry Access ids
//! (`orgs/org_fs01…/projects/prj_…/envs/env_…`); people type slugs
//! (`--project web --env production`). A project or environment segment that
//! is not an id is resolved against Access's lists; an org is the key's own
//! (or given as an id).

use serde_json::json;
use sylphx::Client;

use crate::Failure;

fn is_id(segment: &str, kind: &str) -> bool {
    segment
        .strip_prefix(kind)
        .is_some_and(|rest| rest.starts_with('_'))
}

async fn pick(
    client: &Client,
    parent: &str,
    items: &str,
    want: &str,
    what: &str,
) -> Result<String, Failure> {
    let rows = list(client, items, parent).await?;
    let found = rows.iter().find(|r| {
        r["spec"]["slug"].as_str() == Some(want)
            || r["name"].as_str().and_then(|n| n.rsplit('/').next()) == Some(want)
    });
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
            pick(client, &out, "projects", segs[3], "project").await?
        };
        if segs.len() >= 6 && segs[4] == "envs" {
            out = if is_id(segs[5], "env") {
                format!("{out}/envs/{}", segs[5])
            } else {
                pick(client, &out, "envs", segs[5], "environment").await?
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

async fn project_name(client: &Client, org: &str, value: &str) -> Result<String, Failure> {
    let name = if value.starts_with("orgs/") {
        let parent = crate::context::Link::project_org(value)
            .ok_or_else(|| Failure::Usage("--project must name a project".into()))?;
        if parent != org {
            return Err(Failure::Usage(
                "--project belongs to a different organization".into(),
            ));
        }
        value.to_string()
    } else {
        if value.is_empty() || value.contains('/') {
            return Err(Failure::Usage(
                "--project must be a slug, id, or full project name".into(),
            ));
        }
        format!("{org}/projects/{value}")
    };
    resolve(client, &name).await
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
    if let Some(full) = env.filter(|e| e.starts_with("orgs/")) {
        if crate::context::Link::environment_project(full).is_none() {
            return Err(Failure::Usage("--env must name an environment".into()));
        }
        let full = resolve(client, full).await?;
        let link = crate::context::Link::from_env_name(&full)
            .ok_or_else(|| Failure::Usage("--env must name an environment".into()))?;
        if link.org != org {
            return Err(Failure::Usage(
                "--env belongs to a different organization".into(),
            ));
        }
        if let Some(p) = project {
            let selected = project_name(client, &org, p).await?;
            if selected != link.project {
                return Err(Failure::Usage(
                    "--project and --env name different projects".into(),
                ));
            }
        }
        return Ok((org, Some(link.project), Some(link.env)));
    }
    if env.is_some_and(|e| e.is_empty() || e.contains('/')) {
        return Err(Failure::Usage(
            "--env must be a slug, id, or full environment name".into(),
        ));
    }
    let project = match project {
        Some(p) => project_name(client, &org, p).await?,
        None if env.is_some() => discover_or_create(client, "projects", &org, "default").await?,
        None => return Ok((org, None, None)),
    };
    let env = match env {
        None => None,
        Some(e) => Some(resolve(client, &format!("{project}/envs/{e}")).await?),
    };
    Ok((org, Some(project), env))
}

/// Vercel/Railway first use: discover a unique target, or create the standard
/// target through Access. Never choose an arbitrary existing project/env.
pub async fn complete(
    client: &Client,
    link: &mut crate::context::Link,
    level: usize,
) -> Result<(), Failure> {
    if link.org.is_empty() {
        return Err(Failure::Usage(
            "the signed-in credential names no organization".into(),
        ));
    }
    if level >= 2 && link.project.is_empty() {
        link.project = discover_or_create(client, "projects", &link.org, "default").await?;
    }
    if level >= 3 && link.env.is_empty() {
        link.env = discover_or_create(client, "envs", &link.project, "production").await?;
    }
    Ok(())
}

async fn list(
    client: &Client,
    collection: &str,
    parent: &str,
) -> Result<Vec<serde_json::Value>, Failure> {
    let mut rows = Vec::new();
    let mut token = String::new();
    let mut tokens = std::collections::HashSet::new();
    loop {
        let mut page = client
            .invoke(
                &format!("access.{collection}.list"),
                json!({"parent": parent, "page_token": token}),
            )
            .await?;
        let items = page
            .get_mut(collection)
            .and_then(serde_json::Value::as_array_mut)
            .ok_or_else(|| {
                Failure::Refused(format!("Access returned no {collection} list for {parent}"))
            })?;
        for item in items.iter() {
            checked_name(item, parent, collection)?;
        }
        rows.append(items);
        token = page["next_page_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if token.is_empty() {
            return Ok(rows);
        }
        if !tokens.insert(token.clone()) {
            return Err(Failure::Refused(
                "Access repeated a pagination token".into(),
            ));
        }
    }
}

fn checked_name<'a>(
    item: &'a serde_json::Value,
    parent: &str,
    collection: &str,
) -> Result<&'a str, Failure> {
    item["name"]
        .as_str()
        .filter(|name| {
            name.strip_prefix(parent)
                .and_then(|s| s.strip_prefix('/'))
                .and_then(|s| s.strip_prefix(collection))
                .and_then(|s| s.strip_prefix('/'))
                .is_some_and(|id| !id.is_empty() && !id.contains('/'))
        })
        .ok_or_else(|| {
            Failure::Refused(format!(
                "Access returned a target outside {parent}/{collection}/"
            ))
        })
}

/// Stable across invocations, so a lost create reply cannot duplicate a
/// first-use target. Access namespaces replay keys by org and principal.
struct BootstrapTransport<'a> {
    inner: &'a sylphx::HttpTransport,
    headers: [(String, String); 1],
}

impl sylphx::runtime::Transport for BootstrapTransport<'_> {
    async fn send(
        &self,
        request: sylphx::runtime::HttpRequest,
    ) -> Result<sylphx::runtime::HttpResponse, sylphx::Error> {
        self.inner.send_with_headers(request, &self.headers).await
    }
}
async fn discover_or_create(
    client: &Client,
    collection: &str,
    parent: &str,
    slug: &str,
) -> Result<String, Failure> {
    let rows = list(client, collection, parent).await?;
    if !rows.is_empty() {
        return choose(&rows, parent, collection, slug);
    }
    let (field, spec) = if collection == "projects" {
        ("project", json!({"slug": slug}))
    } else {
        ("environment", json!({"slug": slug, "mode": "live"}))
    };
    use sha2::Digest;
    let key = format!(
        "cli-bootstrap:{:x}",
        sha2::Sha256::digest(format!("{parent}/{collection}/{slug}"))
    );
    let creator = Client::new(BootstrapTransport {
        inner: client.transport(),
        headers: [("Idempotency-Key".into(), key)],
    });
    let created = creator
        .invoke(
            &format!("access.{collection}.create"),
            json!({"parent": parent, (field): {"spec": spec}}),
        )
        .await;
    match created {
        Ok(item) => Ok(checked_name(&item, parent, collection)?.to_string()),
        Err(sylphx::Error::Api { status: 409, .. }) => {
            // Another first-use client won the slug; rediscover, never create
            // an alternative project or fall back to another tenant.
            choose(
                &list(client, collection, parent).await?,
                parent,
                collection,
                slug,
            )
        }
        Err(e) => Err(e.into()),
    }
}

fn choose(
    rows: &[serde_json::Value],
    parent: &str,
    collection: &str,
    slug: &str,
) -> Result<String, Failure> {
    let mut defaults = rows
        .iter()
        .filter(|r| r["spec"]["slug"].as_str() == Some(slug));
    let first = defaults.next();
    let selected = match (first, defaults.next(), rows) {
        (Some(one), None, _) => Some(one),
        (None, None, [one]) => Some(one),
        _ => None,
    };
    if let Some(item) = selected {
        return Ok(checked_name(item, parent, collection)?.to_string());
    }
    let choices = rows
        .iter()
        .map(|r| checked_name(r, parent, collection))
        .collect::<Result<Vec<_>, _>>()?;
    Err(Failure::Usage(format!(
        "choose a target under {parent}; run sylphx link --project … --env … (choices: {})",
        choices.join(", ")
    )))
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
