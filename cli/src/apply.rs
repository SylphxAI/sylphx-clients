//! Declarative Resource apply, backed by the shared planner and writer.
use std::time::Duration;

use clap::{Arg, ArgAction, ArgMatches, Command};
use sylphx_apply::{check, Applier, Declarable, Declarations, Options, Registry};

use crate::{context, output::Format, Failure};

pub fn command() -> Command {
    let mut cmd = Command::new("apply").about("Apply the Resources declared in a manifest");
    for flag in ["dry-run", "offline", "detailed-exitcode", "force"] {
        cmd = cmd.arg(Arg::new(flag).long(flag).action(ArgAction::SetTrue));
    }
    cmd.arg(
        Arg::new("manifest")
            .long("manifest")
            .default_value("sylphx.toml"),
    )
    .arg(Arg::new("release").long("release"))
    .arg(Arg::new("source").long("source").default_value(""))
    .arg(
        Arg::new("timeout")
            .long("timeout")
            .default_value("600")
            .help("Per-Resource timeout in seconds")
            .value_parser(clap::value_parser!(u64)),
    )
}

pub async fn run(
    m: &ArgMatches,
    api_key: Option<String>,
    base_url: Option<String>,
    format: Format,
) -> Result<(), Failure> {
    let path = m.get_one::<String>("manifest").expect("default");
    let text =
        std::fs::read_to_string(path).map_err(|e| Failure::Refused(format!("{path}: {e}")))?;
    let decls = if path.ends_with(".json") {
        Declarations::from_json(&text)
    } else {
        Declarations::from_toml(&text)
    }
    .map_err(|e| Failure::Refused(e.to_string()))?;
    let reg = Registry::embedded();
    let declarable = Declarable::embedded();
    let issues = check(reg, &declarable, &decls);
    if !issues.is_empty() {
        return Err(Failure::Refused(
            issues
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        ));
    }
    if m.get_flag("offline") {
        if format == Format::Json {
            println!("{{\"resources\":[],\"unsettled\":[]}}");
        }
        return Ok(());
    }
    let client = context::client(api_key, base_url)
        .await
        .map_err(Failure::Refused)?
        .ok_or_else(|| Failure::Refused("not signed in: run `sylphx login`".into()))?;
    let opts = Options {
        release_id: m.get_one::<String>("release").cloned(),
        source: m.get_one::<String>("source").expect("default").clone(),
        force: m.get_flag("force"),
        timeout: Duration::from_secs(*m.get_one::<u64>("timeout").expect("default")),
        env: context::default_link(
            &std::env::current_dir().map_err(|e| Failure::Refused(e.to_string()))?,
        )
        .map_err(Failure::Refused)?
        .map(|link| link.env),
        ..Options::default()
    };
    let applier = Applier::new(reg, &declarable, client.transport(), opts);
    let (report, code) = execute(m, &applier, &decls, format, &mut std::io::stdout()).await?;
    if m.get_one::<String>("release").is_some() {
        std::fs::write("/dev/termination-log", termination_result(&report))
            .map_err(|e| Failure::Refused(format!("termination log: {e}")))?;
    }
    if code == 0 {
        Ok(())
    } else {
        Err(Failure::Exit(code))
    }
}

/// Release Job result, kept below Kubernetes' 4096-byte termination-message
/// limit without cutting JSON. Stdout retains the full plan and diffs.
fn termination_result(report: &sylphx_apply::Report) -> String {
    use serde_json::json;
    use sylphx_apply::{Action, Outcome};

    let count = |action| {
        report
            .resources
            .iter()
            .filter(|r| r.action == action && r.outcome != Outcome::Failed)
            .count()
    };
    let mut truncated = false;
    let failed = report
        .resources
        .iter()
        .find(|r| r.outcome == Outcome::Failed || r.action == Action::Refused)
        .map(|r| {
            // The consumer keeps at most 120 characters per failure field.
            let mut bounded = |text: &str| {
                truncated |= text.chars().count() > 120;
                text.chars().take(120).collect::<String>()
            };
            json!({
                "kind": bounded(&r.kind),
                "name": bounded(&r.name),
                "code": bounded(r.code.as_deref().unwrap_or(if r.action == Action::Refused {
                    "refused"
                } else {
                    "failed"
                })),
            })
        });
    let mut result = json!({
        "summary": {
            "create": count(Action::Create),
            "update": count(Action::Update),
            "drift": count(Action::Drift),
            "orphan": count(Action::Orphan),
        },
        "failed": failed,
        "unsettled": [],
        "truncated": truncated,
    });
    // Reserve the longer `false` spelling while adding whole names. Stop at
    // the first name that does not fit, preserving the report's order.
    for name in &report.unsettled {
        result["unsettled"]
            .as_array_mut()
            .expect("array")
            .push(json!(name));
        if result.to_string().len() > 4096 {
            result["unsettled"].as_array_mut().expect("array").pop();
            result["truncated"] = json!(true);
            break;
        }
    }
    result.to_string()
}

async fn execute<W: sylphx_apply::Wire>(
    m: &ArgMatches,
    applier: &Applier<'_, W>,
    decls: &Declarations,
    format: Format,
    out: &mut impl std::io::Write,
) -> Result<(sylphx_apply::Report, u8), Failure> {
    let report = if m.get_flag("dry-run") {
        applier.plan(decls, &[]).await.map(|plan| plan.report())
    } else {
        applier.apply(decls, &[]).await
    }
    .map_err(|e| Failure::Refused(e.to_string()))?;
    let text = if format == Format::Json {
        report.to_json()
    } else {
        report.summary()
    };
    writeln!(out, "{text}").map_err(|e| Failure::Refused(e.to_string()))?;
    let code = if report.failed() {
        1
    } else if m.get_flag("detailed-exitcode") && report.resources.iter().any(|r| r.action.writes())
    {
        2
    } else {
        0
    };
    Ok((report, code))
}

#[cfg(test)]
mod tests;
