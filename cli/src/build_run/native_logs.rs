//! Hosting attempts use a different journal and name than standalone Builds.
use std::time::Duration;

use sylphx::release::{
    BuildAttemptState, ReadBuildAttemptLogsRequest, ReadBuildAttemptLogsResponse,
};
use sylphx::Client;

use super::{interrupted, Out, EXIT_INTERRUPTED};
use crate::{Defaults, Failure};

const NAME: &str = "orgs/{org}/projects/{project}/envs/{env}/build_attempts/{build_attempt}";

pub(super) fn is_attempt(arg: &str) -> bool {
    arg.starts_with("bda_") || (arg.starts_with("orgs/") && arg.contains("/build_attempts/"))
}

pub(super) async fn run(client: &Client, arg: &str, m: &clap::ArgMatches) -> Result<(), Failure> {
    let mut defaults = Defaults {
        client,
        link: None,
        org: m.try_get_one::<String>("org").ok().flatten().cloned(),
    };
    let name = defaults.name(arg, NAME).await?;
    let cursor = m.get_one::<String>("cursor").cloned().unwrap_or_default();
    let out = Out {
        json: false,
        quiet: false,
    };
    tokio::select! {
        result = follow(client, &name, cursor, m.get_flag("follow"), Duration::from_secs(1), |page| {
            for line in &page.lines {
                out.stdout(line.as_bytes());
                out.stdout(b"\n");
            }
            if m.get_flag("show-cursor") && !page.lines.is_empty() {
                out.progress(&format!("cursor {}", page.next_page_token));
            }
        }) => result,
        _ = interrupted() => Err(Failure::Exit(EXIT_INTERRUPTED)),
    }
}

async fn follow<T: sylphx::Transport>(
    client: &Client<T>,
    name: &str,
    mut cursor: String,
    wait: bool,
    pause: Duration,
    mut emit: impl FnMut(&ReadBuildAttemptLogsResponse),
) -> Result<(), Failure> {
    loop {
        let mut request = ReadBuildAttemptLogsRequest::default();
        request.name = name.into();
        request.page_token = cursor.clone();
        // The SDK retries a transient failure with the same logical request
        // and cursor. Authorization errors are never handled as an empty page.
        let page = client.release().build_attempts().read_logs(request).await?;
        if (!page.lines.is_empty() || page.has_more)
            && (page.next_page_token.is_empty() || page.next_page_token == cursor)
        {
            return Err(Failure::Refused(
                "Hosting log cursor did not advance".into(),
            ));
        }
        emit(&page);
        if wait && page.done && !page.has_more {
            return match page.state {
                Some(BuildAttemptState::Succeeded) => Ok(()),
                Some(BuildAttemptState::Failed) => {
                    Err(Failure::Refused("Hosting build attempt failed".into()))
                }
                Some(BuildAttemptState::Cancelled) => Err(Failure::Refused(
                    "Hosting build attempt was cancelled".into(),
                )),
                _ => Err(Failure::Refused(
                    "Hosting build attempt has no terminal outcome".into(),
                )),
            };
        }
        cursor = page.next_page_token;
        if !page.has_more {
            if !wait {
                return Ok(());
            }
            tokio::time::sleep(pause).await;
        }
    }
}

#[cfg(test)]
mod tests;
