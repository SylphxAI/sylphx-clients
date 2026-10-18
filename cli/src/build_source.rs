//! Repository-scoped pull-through source access, derived from GitHub origins.
//! GitHub remains the write/review origin; credentials live only in the environment.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command as Process;

use clap::{Arg, ArgMatches, Command};
use serde_json::json;

use crate::build_cache::{self, MintError, Minted, Network};
use crate::context::{self, Resolved};
use crate::output::{self, Format};
use crate::Failure;

/// Accept repository names and GitHub's supported Git origin spellings, never
/// credential-bearing URLs or arbitrary hosts.
pub fn repository(origin: &str) -> Option<String> {
    let path = origin
        .strip_prefix("https://github.com/")
        .or_else(|| origin.strip_prefix("ssh://git@github.com/"))
        .or_else(|| origin.strip_prefix("git@github.com:"))
        .unwrap_or(origin);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    let valid = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
    };
    (valid(owner) && valid(repo)).then(|| format!("{owner}/{repo}"))
}

pub fn origin(root: &Path) -> Option<String> {
    let out = Process::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "--get", "remote.origin.url"])
        .output()
        .ok()?;
    out.status.success().then_some(())?;
    repository(String::from_utf8(out.stdout).ok()?.trim())
}

pub async fn mint(
    base: &str,
    key: &str,
    project: &str,
    repository: &str,
    network: Network,
    trust_policy: Option<&str>,
) -> Result<Minted, MintError> {
    let mut body = json!({
        "project": project,
        "repository": repository,
        "network": match network { Network::Cluster => "cluster", Network::Public => "public" },
    });
    if let Some(policy) = trust_policy {
        body["trust_policy"] = json!(policy);
    }
    let mut minted = build_cache::mint_at(base, key, "v1/source-sessions", &body).await?;
    if minted.token.is_none() || minted.expires_at.is_none() || config_count(&minted.env).is_none()
    {
        return Err(MintError::BadReply);
    }
    github_aliases(&mut minted.env);
    Ok(minted)
}

fn config_count(env: &BTreeMap<String, String>) -> Option<usize> {
    let count: usize = env.get("GIT_CONFIG_COUNT")?.parse().ok()?;
    if count > env.len() / 2 {
        return None;
    }
    (0..count)
        .all(|i| {
            env.contains_key(&format!("GIT_CONFIG_KEY_{i}"))
                && env.contains_key(&format!("GIT_CONFIG_VALUE_{i}"))
        })
        .then_some(count)
}

/// The same repository may be locked or cloned using GitHub SSH spelling.
/// Redirect reads only; the corresponding push rewrite remains GitHub.
pub fn github_aliases(env: &mut BTreeMap<String, String>) {
    let Some(count) = config_count(env) else {
        return;
    };
    let Some(cache) = (0..count).find_map(|i| {
        env.get(&format!("GIT_CONFIG_KEY_{i}"))
            .and_then(|k| k.strip_prefix("url."))
            .and_then(|k| k.strip_suffix(".insteadOf"))
            .map(str::to_owned)
    }) else {
        return;
    };
    let mut aliases = BTreeMap::new();
    let mut i = 0;
    for prefix in ["ssh://git@github.com/", "git@github.com:"] {
        for (key, value) in [
            (format!("url.{cache}.insteadOf"), prefix.to_string()),
            (
                "url.https://github.com/.pushInsteadOf".to_string(),
                prefix.to_string(),
            ),
        ] {
            aliases.insert(format!("GIT_CONFIG_KEY_{i}"), key);
            aliases.insert(format!("GIT_CONFIG_VALUE_{i}"), value);
            i += 1;
        }
    }
    aliases.insert("GIT_CONFIG_COUNT".into(), i.to_string());
    merge_env(env, aliases);
}

/// Append an indexed Git environment without overwriting existing mirror
/// mappings. Apply explicit user variables afterwards: their advanced override wins.
pub fn merge_env(target: &mut BTreeMap<String, String>, source: BTreeMap<String, String>) {
    let Some(count) = config_count(&source) else {
        target.extend(source);
        return;
    };
    let offset = config_count(target).unwrap_or(0);
    for (key, value) in source {
        if key == "GIT_CONFIG_COUNT" {
            continue;
        }
        let indexed = key
            .strip_prefix("GIT_CONFIG_KEY_")
            .map(|i| ("GIT_CONFIG_KEY_", i))
            .or_else(|| {
                key.strip_prefix("GIT_CONFIG_VALUE_")
                    .map(|i| ("GIT_CONFIG_VALUE_", i))
            });
        match indexed.and_then(|(prefix, i)| i.parse::<usize>().ok().map(|i| (prefix, i))) {
            Some((prefix, i)) => {
                target.insert(format!("{prefix}{}", offset + i), value);
            }
            None => {
                target.insert(key, value);
            }
        }
    }
    target.insert("GIT_CONFIG_COUNT".into(), (offset + count).to_string());
}

pub fn overridden(env: &BTreeMap<String, String>) -> bool {
    env.keys().any(|k| k.starts_with("GIT_CONFIG_"))
}

/// Move only source bearer headers into a private, reread-on-each-Git-call
/// include. Routing remains indexed and composes with the synced mirrors.
pub fn take_headers(env: &mut BTreeMap<String, String>, path: &str) -> Result<Vec<u8>, MintError> {
    let count = config_count(env).ok_or(MintError::BadReply)?;
    let mut entries = Vec::new();
    let mut headers = String::new();
    let quote = |value: &str| {
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    };
    for i in 0..count {
        let key = env
            .remove(&format!("GIT_CONFIG_KEY_{i}"))
            .ok_or(MintError::BadReply)?;
        let value = env
            .remove(&format!("GIT_CONFIG_VALUE_{i}"))
            .ok_or(MintError::BadReply)?;
        if let Some(url) = key
            .strip_prefix("http.")
            .and_then(|key| key.strip_suffix(".extraheader"))
        {
            headers.push_str(&format!(
                "[http \"{}\"]\n\textraheader = \"{}\"\n",
                quote(url),
                quote(&value)
            ));
        } else {
            entries.push((key, value));
        }
    }
    if !headers.is_empty() {
        entries.push(("include.path".to_string(), path.to_string()));
    }
    env.insert("GIT_CONFIG_COUNT".into(), entries.len().to_string());
    for (i, (key, value)) in entries.into_iter().enumerate() {
        env.insert(format!("GIT_CONFIG_KEY_{i}"), key);
        env.insert(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    Ok(headers.into_bytes())
}

pub fn command() -> Command {
    Command::new("source")
        .about("Repository-scoped shared source cache")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("env")
                .about("Print Git settings before cloning a GitHub repository")
                .long_about("Derives a repository-scoped source session from your public Access/Broker grants. No clone or infrastructure configuration is required. Git reads use the shared cache; pushes stay on GitHub. Evaluate the shell exports before git clone, or use -o json for an environment object. The output contains a credential: do not log or save it in a repository. --trust-policy is an optional advanced override. Exit 125 when not signed in or access is denied.")
                .arg(Arg::new("repository").required(true).value_name("OWNER/REPOSITORY"))
                .arg(Arg::new("project").long("project").value_name("ID|SLUG|NAME").help("Default: linked project, else the key's project"))
                .arg(Arg::new("trust-policy").long("trust-policy").value_name("RESOURCE_NAME")),
        )
}

pub async fn run(creds: Option<Resolved>, m: &ArgMatches, format: Format) -> Result<(), Failure> {
    let (_, m) = m.subcommand().expect("subcommand_required");
    let Some(creds) = creds else {
        eprintln!("error: not signed in: run `sylphx login`, or set SYLPHX_API_KEY");
        return Err(Failure::Exit(125));
    };
    let repo =
        repository(m.get_one::<String>("repository").expect("required")).ok_or_else(|| {
            Failure::Usage("repository must be a GitHub owner/name or Git origin".into())
        })?;
    let client = context::client_for(&creds).map_err(Failure::Usage)?;
    let project = build_cache::project_of(
        &client,
        m.get_one::<String>("project"),
        m.try_get_one::<String>("org").ok().flatten().cloned(),
    )
    .await?;
    let minted = mint(
        &build_cache::base_url(),
        &creds.key,
        &project,
        &repo,
        Network::Public,
        m.get_one::<String>("trust-policy").map(String::as_str),
    )
    .await
    .map_err(|e| {
        eprintln!("error: source session unavailable ({})", e.short());
        Failure::Exit(125)
    })?;
    match format {
        Format::Json | Format::Yaml => println!("{}", output::render(&json!(minted.env), format)),
        _ => {
            for (key, value) in minted.env {
                println!("export {key}={}", build_cache::shell_quote(&value));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_include_rereads_renewed_headers_without_clobbering_mirrors_or_other_repositories() {
        let directory =
            std::env::temp_dir().join(format!("sylphx-source-include-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("source.gitconfig");
        let mut env = config(&[
            (
                "url.file:///mirror.insteadOf",
                "https://github.com/acme/private",
            ),
            (
                "http.https://cache.test/git/org_a/acme/repo.git.extraheader",
                "Authorization: Bearer first-fixture-token",
            ),
            (
                "http.https://cache.test/git/org_a/acme/repo.extraheader",
                "Authorization: Bearer first-fixture-token",
            ),
        ]);
        let bytes = take_headers(&mut env, path.to_str().unwrap()).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(env["GIT_CONFIG_COUNT"], "2");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "url.file:///mirror.insteadOf");
        assert_eq!(env["GIT_CONFIG_KEY_1"], "include.path");
        assert!(env
            .values()
            .all(|value| !value.contains("first-fixture-token")));
        let read = |url: &str| {
            Process::new("git")
                .args(["config", "--get-urlmatch", "http.extraheader", url])
                .envs(&env)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
        };
        let current = read("https://cache.test/git/org_a/acme/repo.git/info/refs");
        assert!(current.status.success());
        assert_eq!(
            String::from_utf8(current.stdout).unwrap().trim(),
            "Authorization: Bearer first-fixture-token"
        );
        assert!(
            !read("https://cache.test/git/org_a/acme/repo-other.git/info/refs")
                .status
                .success()
        );
        std::fs::write(
            &path,
            String::from_utf8(bytes)
                .unwrap()
                .replace("first-fixture-token", "renewed-fixture-token"),
        )
        .unwrap();
        let renewed = read("https://cache.test/git/org_a/acme/repo.git/info/refs");
        assert_eq!(
            String::from_utf8(renewed.stdout).unwrap().trim(),
            "Authorization: Bearer renewed-fixture-token"
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    fn config(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([("GIT_CONFIG_COUNT".into(), entries.len().to_string())]);
        for (i, (key, value)) in entries.iter().enumerate() {
            env.insert(format!("GIT_CONFIG_KEY_{i}"), (*key).into());
            env.insert(format!("GIT_CONFIG_VALUE_{i}"), (*value).into());
        }
        env
    }

    #[test]
    fn repository_derivation_rejects_credentials_and_other_hosts() {
        for url in [
            "acme/repo",
            "https://github.com/acme/repo.git",
            "ssh://git@github.com/acme/repo",
            "git@github.com:acme/repo.git",
        ] {
            assert_eq!(repository(url).as_deref(), Some("acme/repo"));
        }
        for url in [
            "https://secret@github.com/acme/repo",
            "https://example.com/acme/repo",
            "acme/repo/other",
            "../repo",
        ] {
            assert_eq!(repository(url), None);
        }
    }

    #[test]
    fn git_environment_composes_mirrors_source_and_advanced_override() {
        let mut env = config(&[(
            "url.file:///mirror.insteadOf",
            "https://github.com/acme/private",
        )]);
        merge_env(
            &mut env,
            config(&[(
                "url.https://cache/git/acme/repo.git.insteadOf",
                "https://github.com/acme/repo",
            )]),
        );
        assert_eq!(env["GIT_CONFIG_COUNT"], "2");
        assert_eq!(env["GIT_CONFIG_KEY_0"], "url.file:///mirror.insteadOf");
        assert_eq!(env["GIT_CONFIG_VALUE_1"], "https://github.com/acme/repo");
        let user = config(&[(
            "url.https://external/repo.insteadOf",
            "https://github.com/acme/repo",
        )]);
        assert!(overridden(&user));
        env.extend(user);
        assert_eq!(env["GIT_CONFIG_COUNT"], "1");
        assert_eq!(
            env["GIT_CONFIG_KEY_0"],
            "url.https://external/repo.insteadOf"
        );
    }
}
