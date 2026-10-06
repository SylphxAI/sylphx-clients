//! The generated command tree (`generated/commands.json`, emitted by
//! `sylphx-gen` from the one schema) as clap commands, and a parsed command
//! line back into a method call with wire-JSON arguments.

use clap::{Arg, ArgAction, ArgMatches, Command};
use serde::Deserialize;
use serde_json::{json, Map, Value};

/// The generated command tree.
pub const TREE: &str = include_str!("../generated/commands.json");

#[derive(Debug, Deserialize)]
pub struct Tree {
    pub services: Vec<ServiceCmd>,
}

#[derive(Debug, Deserialize)]
pub struct ServiceCmd {
    pub noun: String,
    pub display_name: String,
    pub stage: String,
    /// `false` when no backend serves the service yet (`contracts/services.toml`).
    #[serde(default = "served_default")]
    pub served: bool,
    #[serde(default)]
    pub commands: Vec<MethodCmd>,
    pub collections: Vec<CollectionCmd>,
    /// The short commands of `MethodPolicy.porcelain`
    /// (`sylphx build cache env`): the same words as the SDK method and the
    /// MCP tool.
    #[serde(default)]
    pub porcelain: Vec<PorcelainCmd>,
}

/// A short command: the handle's segments below the service, then the
/// method's command under its short verb.
#[derive(Debug, Clone, Deserialize)]
pub struct PorcelainCmd {
    pub path: Vec<String>,
    #[serde(flatten)]
    pub command: MethodCmd,
}

impl PorcelainCmd {
    /// The words after the service noun: `["cache", "env"]`.
    pub fn words(&self) -> Vec<&str> {
        self.path
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.command.verb.as_str()))
            .collect()
    }
}

fn served_default() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct CollectionCmd {
    pub noun: String,
    pub collection: String,
    pub resource_type: String,
    pub commands: Vec<MethodCmd>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MethodCmd {
    pub verb: String,
    pub method: String,
    pub summary: String,
    pub http: String,
    pub effect: String,
    pub positional: Option<Positional>,
    pub flags: Vec<FlagSpec>,
    #[serde(default)]
    pub waits: Option<Value>,
    #[serde(default)]
    pub update_mask: Option<String>,
    #[serde(default)]
    pub sends_etag: bool,
    #[serde(default)]
    pub paginates: bool,
    #[serde(default, rename = "resource_field")]
    pub resource_field_name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Positional {
    /// `ID`, `PARENT`, or `NAME`.
    pub arg: String,
    pub field: String,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub default_from_context: bool,
    /// A singleton's literal last segment: the name is the parent and it.
    #[serde(default)]
    pub suffix: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FlagSpec {
    /// `--spec.size`.
    pub flag: String,
    /// The request field path, e.g. `database.spec.size`.
    pub field: String,
    /// `bool`, `int`, `number`, `string`, `enum`, `timestamp`, `duration`,
    /// `json`, `key=value`.
    pub r#type: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub repeated: bool,
    #[serde(default)]
    pub values: Option<Vec<String>>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub default_from_context: bool,
}

impl MethodCmd {
    pub fn long_running(&self) -> bool {
        self.waits.is_some()
    }
    pub fn is_update(&self) -> bool {
        self.update_mask.is_some()
    }
    /// The request field that carries the Resource (`database`), for Create
    /// and Update.
    pub fn resource_field(&self) -> Option<String> {
        self.resource_field_name.clone()
    }
}

pub fn load() -> Tree {
    serde_json::from_str(TREE).expect("generated/commands.json is valid")
}

fn arg_id(flag: &str) -> String {
    flag.trim_start_matches("--").to_string()
}

/// The clap command of one generated method.
pub fn method_command(m: &MethodCmd) -> Command {
    let mut about = m.summary.clone();
    if about.is_empty() {
        about = m.method.clone();
    }
    let mut cmd = Command::new(m.verb.clone())
        .about(about)
        .after_help(format!(
            "{}  ·  method {}  ·  effect {}",
            m.http, m.method, m.effect
        ));
    if let Some(p) = &m.positional {
        let mut arg = Arg::new("__positional").value_name(p.arg.clone());
        arg = arg.required(!(p.optional || p.default_from_context));
        let help = match p.arg.as_str() {
            "ID" => "The new Resource's id; the server assigns one when omitted".to_string(),
            "PARENT" => format!(
                "The parent ({}); defaults to the linked project or the key's scope",
                p.pattern.clone().unwrap_or_default()
            ),
            _ => format!(
                "The Resource name ({}), or its bare id below the linked env",
                p.pattern.clone().unwrap_or_default()
            ),
        };
        cmd = cmd.arg(arg.help(help));
    }
    for f in &m.flags {
        let id = arg_id(&f.flag);
        let mut arg = Arg::new(id.clone()).long(id).help(f.summary.clone());
        arg = match f.r#type.as_str() {
            "bool" => arg.action(ArgAction::SetTrue),
            _ if f.repeated => arg.action(ArgAction::Append).value_name(value_name(f)),
            _ => arg.action(ArgAction::Set).value_name(value_name(f)),
        };
        if let Some(values) = &f.values {
            if f.r#type == "enum" {
                arg = arg.value_parser(clap::builder::PossibleValuesParser::new(values.clone()));
            }
        }
        cmd = cmd.arg(arg);
    }
    cmd
}

fn value_name(f: &FlagSpec) -> String {
    match f.r#type.as_str() {
        "key=value" => "KEY=VALUE".into(),
        "int" | "number" => "N".into(),
        "json" => "JSON".into(),
        "timestamp" => "RFC3339".into(),
        "duration" => "DURATION".into(),
        "enum" => "VALUE".into(),
        _ => "STRING".into(),
    }
}

/// `sylphx <service> …` for every service in the tree. `hand_written` names
/// the `(service, word)` pairs the binary mounts itself (`build run`): a
/// short command starting with that word gives way to it.
pub fn service_commands(tree: &Tree, hand_written: &[(&str, &str)]) -> Vec<Command> {
    tree.services
        .iter()
        .map(|s| {
            let mut about = s.display_name.clone();
            if s.stage == "preview" {
                about.push_str(" (preview)");
            }
            if !s.served {
                about.push_str(" - not available yet");
            }
            let mut cmd = Command::new(s.noun.clone())
                .about(about)
                .subcommand_required(true)
                .arg_required_else_help(true);
            for m in &s.commands {
                cmd = cmd.subcommand(method_command(m));
            }
            for c in &s.collections {
                let mut cc = Command::new(c.noun.clone())
                    .about(format!("`{}`: {}", c.collection, c.resource_type))
                    .subcommand_required(true)
                    .arg_required_else_help(true);
                for m in &c.commands {
                    cc = cc.subcommand(method_command(m));
                }
                cmd = cmd.subcommand(cc);
            }
            let short: Vec<(Vec<&str>, &MethodCmd)> = s
                .porcelain
                .iter()
                .map(|p| (p.words(), &p.command))
                .filter(|(w, _)| !hand_written.contains(&(s.noun.as_str(), w[0])))
                .collect();
            mount(cmd, &short)
        })
        .collect()
}

/// Mounts short commands below `cmd`: a command whose words are only its
/// verb directly, the others under one subcommand per handle segment.
fn mount(mut cmd: Command, items: &[(Vec<&str>, &MethodCmd)]) -> Command {
    let mut handles: Vec<&str> = Vec::new();
    for (words, m) in items {
        match words.as_slice() {
            [_verb] => cmd = cmd.subcommand(method_command(m)),
            [seg, ..] if !handles.contains(seg) => handles.push(seg),
            _ => {}
        }
    }
    for seg in handles {
        let below: Vec<(Vec<&str>, &MethodCmd)> = items
            .iter()
            .filter(|(w, _)| w.len() > 1 && w[0] == seg)
            .map(|(w, m)| (w[1..].to_vec(), *m))
            .collect();
        let handle = Command::new(seg.to_string())
            .about(format!("`{seg}` commands"))
            .subcommand_required(true)
            .arg_required_else_help(true);
        cmd = cmd.subcommand(mount(handle, &below));
    }
    cmd
}

/// Finds the method a generated command line selects.
pub fn find<'t>(
    tree: &'t Tree,
    service: &str,
    matches: &ArgMatches,
) -> Option<(&'t MethodCmd, ArgMatches)> {
    let s = tree.services.iter().find(|s| s.noun == service)?;
    let (sub, sub_m) = matches.subcommand()?;
    if let Some(m) = s.commands.iter().find(|m| m.verb == sub) {
        return Some((m, sub_m.clone()));
    }
    for p in &s.porcelain {
        let words = p.words();
        if words[0] != sub {
            continue;
        }
        let mut cur = sub_m;
        let mut hit = true;
        for w in &words[1..] {
            match cur.subcommand() {
                Some((name, next)) if name == *w => cur = next,
                _ => {
                    hit = false;
                    break;
                }
            }
        }
        if hit {
            return Some((&p.command, cur.clone()));
        }
    }
    let c = s.collections.iter().find(|c| c.noun == sub)?;
    let (verb, verb_m) = sub_m.subcommand()?;
    let m = c.commands.iter().find(|m| m.verb == verb)?;
    Some((m, verb_m.clone()))
}

/// Sets `value` at a dotted path, creating objects on the way.
pub fn set_path(root: &mut Map<String, Value>, path: &str, value: Value) {
    let mut parts: Vec<&str> = path.split('.').collect();
    let last = parts.pop().unwrap_or_default();
    let mut cur = root;
    for p in parts {
        let entry = cur.entry(p.to_string()).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        cur = entry.as_object_mut().expect("object");
    }
    cur.insert(last.to_string(), value);
}

/// Reads a dotted path.
pub fn get_path<'v>(root: &'v Value, path: &str) -> Option<&'v Value> {
    path.split('.').try_fold(root, |cur, p| cur.get(p))
}

/// `(field path, wire value)` of each flag a command line set.
pub type FlagValues = Vec<(String, Value)>;

/// The flags a command line set, as wire JSON at their field paths, and the
/// resource-relative paths they touched (the Update mask).
pub fn flag_values(
    m: &MethodCmd,
    matches: &ArgMatches,
) -> Result<(FlagValues, Vec<String>), String> {
    let resource = m.resource_field();
    let mut out = Vec::new();
    let mut mask = Vec::new();
    for f in &m.flags {
        let id = arg_id(&f.flag);
        let value = match f.r#type.as_str() {
            "bool" => {
                if !matches.get_flag(&id) {
                    continue;
                }
                Value::Bool(true)
            }
            _ => {
                let Some(raw) = matches.get_many::<String>(&id) else {
                    continue;
                };
                let raw: Vec<String> = raw.cloned().collect();
                parse_value(f, &raw).map_err(|e| format!("{}: {e}", f.flag))?
            }
        };
        if let Some(r) = &resource {
            if let Some(rel) = f.field.strip_prefix(&format!("{r}.")) {
                mask.push(rel.to_string());
            }
        }
        out.push((f.field.clone(), value));
    }
    Ok((out, mask))
}

fn parse_value(f: &FlagSpec, raw: &[String]) -> Result<Value, String> {
    let one = |s: &str| -> Result<Value, String> {
        match f.r#type.as_str() {
            "int" => s
                .parse::<i64>()
                .map(|n| json!(n))
                .map_err(|e| format!("`{s}` is not an integer: {e}")),
            "number" => match s.parse::<i64>() {
                Ok(n) => Ok(json!(n)),
                Err(_) => s
                    .parse::<f64>()
                    .map(|n| json!(n))
                    .map_err(|e| format!("`{s}` is not a number: {e}")),
            },
            "json" => {
                let text = match s.strip_prefix('@') {
                    Some(path) => {
                        std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?
                    }
                    None => s.to_string(),
                };
                serde_json::from_str(&text).map_err(|e| format!("not JSON: {e}"))
            }
            _ => Ok(Value::String(s.to_string())),
        }
    };
    if f.r#type == "key=value" {
        let mut map = Map::new();
        for kv in raw {
            let (k, v) = kv
                .split_once('=')
                .ok_or_else(|| format!("`{kv}` is not KEY=VALUE"))?;
            map.insert(k.to_string(), Value::String(v.to_string()));
        }
        return Ok(Value::Object(map));
    }
    if f.repeated {
        let mut items = Vec::new();
        for r in raw {
            // `--spec.scopes a,b` and `--spec.scopes a --spec.scopes b` both work.
            let parts: Vec<&str> = if f.r#type == "json" {
                vec![r.as_str()]
            } else {
                r.split(',').collect()
            };
            for p in parts.into_iter().filter(|p| !p.is_empty()) {
                items.push(one(p)?);
            }
        }
        return Ok(Value::Array(items));
    }
    one(raw.last().map(String::as_str).unwrap_or_default())
}

/// Required flags a request still lacks after `--from-file` and the flags.
pub fn missing_required(m: &MethodCmd, args: &Value) -> Vec<String> {
    m.flags
        .iter()
        .filter(|f| f.required && get_path(args, &f.field).is_none())
        .map(|f| f.flag.clone())
        .collect()
}

/// Parses `.sylphx/project.json`-style patterns: how many `{collection}/{id}`
/// pairs a named pattern has below the root, and whether it is the standard
/// org/project/env chain.
pub fn context_level(pattern: &str) -> Option<usize> {
    match pattern {
        "orgs/{org}" => Some(1),
        "orgs/{org}/projects/{project}" => Some(2),
        "orgs/{org}/projects/{project}/envs/{env}" => Some(3),
        _ => None,
    }
}

/// `(parent pattern, collection)` of a Resource pattern:
/// `orgs/{org}/…/databases/{database}` → (`orgs/{org}/…`, `databases`).
pub fn split_pattern(pattern: &str) -> Option<(String, String)> {
    let segs: Vec<&str> = pattern.split('/').collect();
    if segs.len() < 4 {
        return None;
    }
    let collection = segs[segs.len() - 2].to_string();
    Some((segs[..segs.len() - 2].join("/"), collection))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(hand_written: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new("sylphx");
        for s in service_commands(&load(), hand_written) {
            cmd = cmd.subcommand(s);
        }
        cmd
    }

    fn method_of(cmd: Command, argv: &[&str]) -> Option<String> {
        let m = cmd.try_get_matches_from(argv).expect("parses");
        let (service, sub) = m.subcommand().expect("service");
        find(&load(), service, sub).map(|(c, _)| c.method.clone())
    }

    /// `MethodPolicy.porcelain` mounts each short command under the same
    /// words as the SDK method and the MCP tool, and it resolves to the
    /// method its full-tree command calls.
    #[test]
    fn short_commands_mount_and_resolve_to_their_method() {
        root(&[]).debug_assert();
        for (argv, method) in [
            (
                &[
                    "sylphx",
                    "build",
                    "run",
                    "orgs/o/projects/p",
                    "--tree",
                    "{}",
                    "--command",
                    "make",
                ][..],
                "build.builds.run",
            ),
            (
                &["sylphx", "build", "image", "orgs/o/projects/p"],
                "build.builds.build_image",
            ),
            (
                &[
                    "sylphx",
                    "build",
                    "logs",
                    "read",
                    "orgs/o/projects/p/builds/b",
                ],
                "build.builds.read_logs",
            ),
            (
                &[
                    "sylphx",
                    "build",
                    "cache",
                    "env",
                    "orgs/o/projects/p/build_caches/c",
                ],
                "build.build_caches.mint_env",
            ),
            (
                &[
                    "sylphx",
                    "build",
                    "caches",
                    "purge",
                    "orgs/o/projects/p/build_caches/c",
                ],
                "build.build_caches.purge",
            ),
            // The full-tree command stays beside the short one.
            (
                &[
                    "sylphx",
                    "build",
                    "builds",
                    "read-logs",
                    "orgs/o/projects/p/builds/b",
                ],
                "build.builds.read_logs",
            ),
        ] {
            assert_eq!(
                method_of(root(&[]), argv).as_deref(),
                Some(method),
                "{argv:?}"
            );
        }
    }

    /// A word the binary writes by hand (`build run`) is not mounted from
    /// the tree, so clap sees one subcommand per name.
    #[test]
    fn a_hand_written_word_takes_the_place_of_the_short_command() {
        let cmd = root(&[("build", "run"), ("build", "cache")]);
        let build = cmd.find_subcommand("build").expect("build");
        assert!(build.find_subcommand("run").is_none());
        assert!(build.find_subcommand("cache").is_none());
        assert!(build.find_subcommand("image").is_some());
        assert!(build.find_subcommand("caches").is_some());
    }
}
