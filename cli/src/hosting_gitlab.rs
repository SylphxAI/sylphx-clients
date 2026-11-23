//! GitLab environment binding through Hosting's existing environment API.
//! Connection registration and secret custody remain on their generated APIs.

use clap::{builder::NonEmptyStringValueParser, Arg, ArgMatches, Command};
use serde_json::{json, Value};
use sylphx::{Client, HttpRequest};

use crate::context::{self, Resolved};
use crate::output::{self, Format};
use crate::{confirm, release_cmd, Failure};

pub(crate) const COMMAND: &str = "gitlab";

pub(crate) fn command() -> Command {
    let env = || {
        Arg::new("env")
            .long("env")
            .value_name("NAME")
            .help("Full Access environment name; defaults to the linked or key's environment")
    };
    Command::new(COMMAND)
        .about("Manage an environment's self-hosted GitLab source")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .subcommand(
            Command::new("bind")
                .about("Bind a GitLab Connection to an environment and branch")
                .arg(env())
                .arg(
                    Arg::new("connection")
                        .value_name("CONNECTION_ID")
                        .required(true)
                        .help("ID of an org-local GitLab Connection created with `sylphx connections create`"),
                )
                .arg(
                    Arg::new("branch")
                        .long("branch")
                        .value_name("BRANCH")
                        .required(true)
                        .value_parser(NonEmptyStringValueParser::new())
                        .help("GitLab branch to deploy"),
                ),
        )
        .subcommand(
            Command::new("status")
                .about("Show the GitLab Connection and verified webhook binding ID, if any")
                .arg(env()),
        )
        .subcommand(
            Command::new("unbind")
                .about("Detach the GitLab source without deleting the Connection")
                .arg(env()),
        )
}

fn safe_segment(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

fn valid_connection_id(id: &str) -> bool {
    safe_segment(id)
        && id.len() <= 63
        && !id.starts_with("github-")
        && id.bytes().all(|b| !b.is_ascii_uppercase())
        && id.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && id.as_bytes().last().is_some_and(u8::is_ascii_alphanumeric)
}

fn env_ids(name: &str) -> Result<(&str, &str, &str), Failure> {
    let mut segments = name.split('/');
    match (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) {
        (
            Some("orgs"),
            Some(org),
            Some("projects"),
            Some(project),
            Some("envs"),
            Some(env),
            None,
        ) if [org, project, env].into_iter().all(safe_segment) => Ok((org, project, env)),
        _ => Err(Failure::Usage(
            "expected an environment name: orgs/{org}/projects/{project}/envs/{env}".into(),
        )),
    }
}

pub(crate) async fn run(
    credentials: Option<Resolved>,
    matches: &ArgMatches,
    org: Option<&str>,
    format: Format,
) -> Result<(), Failure> {
    let (verb, sub) = matches.subcommand().expect("subcommand required");
    if sub.get_one::<String>("from-file").is_some() {
        return Err(Failure::Usage(
            "GitLab binding does not support --from-file".into(),
        ));
    }
    if let Some(env) = sub.get_one::<String>("env") {
        env_ids(env)?;
    }
    let (method, body) = match verb {
        "bind" => {
            let id = sub.get_one::<String>("connection").expect("required ID");
            if !valid_connection_id(id) {
                return Err(Failure::Usage(
                    "expected a GitLab Connection ID, not a URL or full resource name".into(),
                ));
            }
            let branch = sub.get_one::<String>("branch").expect("required branch");
            (
                "PATCH",
                Some(json!({"gitlabConnectionId": id, "githubBranch": branch})),
            )
        }
        "unbind" => ("PATCH", Some(json!({"gitlabConnectionId": null}))),
        _ => ("GET", None),
    };
    let credentials = credentials.ok_or_else(|| {
        Failure::Usage("not signed in: run `sylphx login` or set SYLPHX_API_KEY".into())
    })?;
    // The environment PATCH does not promise replay of an ambiguous write.
    let client = Client::new(
        context::client_builder(&credentials)
            .max_retries(0)
            .build()?,
    );
    let env = release_cmd::environment(&client, sub, org).await?;
    let (env_org, project_id, env_id) = env_ids(&env)?;
    if let Some(selected) = sub.get_one::<String>("org") {
        let stored = context::load_credentials();
        let selected = context::selected_org(&stored, Some(selected))
            .map(|credential| credential.id.as_str())
            .unwrap_or_else(|| selected.strip_prefix("orgs/").unwrap_or(selected));
        if selected != env_org {
            return Err(Failure::Usage(
                "--org and the environment name different organizations".into(),
            ));
        }
    }
    if verb == "unbind" && !sub.get_flag("yes") {
        confirm(&format!("Detach the GitLab source from {env}?"))?;
    }
    let expected_connection = body
        .as_ref()
        .map(|patch| patch["gitlabConnectionId"].clone());
    let expected_branch = body
        .as_ref()
        .and_then(|patch| patch.get("githubBranch"))
        .cloned();
    let response: Value = client
        .call(HttpRequest {
            method,
            path: format!("/v1/projects/{project_id}/environments/{env_id}"),
            query: vec![],
            mutation: body.is_some(),
            body,
            origin: None,
            effect_ids: false,
        })
        .await?;

    // An older backend may ignore unknown PATCH keys. Never treat its 200 OK
    // with no GitLab fields as confirmation that a binding changed.
    let missing = || {
        Failure::Refused("the environment response did not report a GitLab binding; check `sylphx hosting gitlab status`".into())
    };
    let connection = response.get("gitlabConnectionId").ok_or_else(missing)?;
    let binding = response.get("gitlabBindingId").ok_or_else(missing)?;
    if (!connection.is_null() && !connection.as_str().is_some_and(valid_connection_id))
        || (!binding.is_null()
            && (connection.is_null() || !binding.as_str().is_some_and(safe_segment)))
        || expected_connection
            .as_ref()
            .is_some_and(|expected| expected != connection)
        || expected_branch.is_some_and(|expected| expected != response["githubBranch"])
    {
        return Err(missing());
    }
    let result = json!({
        "name": env,
        "gitlabConnectionId": connection,
        "gitlabBindingId": binding,
        "branch": if connection.is_null() { &Value::Null } else { &response["githubBranch"] },
    });
    println!("{}", output::render(&result, format));
    Ok(())
}
