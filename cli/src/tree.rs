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
    /// Earlier spellings of `noun` that still resolve (`ai-models` for `models`).
    #[serde(default)]
    pub aliases: Vec<String>,
    pub collection: String,
    pub resource_type: String,
    /// `true` when the collection noun repeats the service noun (`secrets secrets`,
    /// `build builds`) so its verbs mount directly under `sylphx <service> <verb>`.
    #[serde(default)]
    pub collapsed: bool,
    pub commands: Vec<MethodCmd>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MethodCmd {
    pub verb: String,
    pub method: String,
    pub summary: String,
    /// Individual availability within a partially served service.
    #[serde(default = "served_default")]
    pub served: bool,
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
    pub sensitive: bool,
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
    let build_list = m.method == "build.builds.list";
    if build_list {
        about = "Lists standalone Build records in a project, not deployment builds.".into();
    }
    let mut cmd = Command::new(m.verb.clone())
        .about(about)
        .after_help(format!(
            "{}  ·  method {}  ·  effect {}",
            m.http, m.method, m.effect
        ));
    if build_list {
        cmd = cmd.long_about(concat!(
            "Lists standalone Build records in a project (including `sylphx build run --no-tree`).",
            "\n\nFor a GitHub-triggered deployment, open the commit’s `sylphx/deploy` check for ",
            "your environment, including while it is waiting for build artifacts. After a Release ",
            "exists, run `sylphx status` in the linked environment.",
            "\n\nFor a regular `sylphx build run` or `sylphx build image`, use the command’s ",
            "output or `sylphx sandboxes leases list` in the run’s environment.",
        ));
    }
    if m.method == "runners.scale_sets.create" {
        cmd = cmd.long_about(concat!(
            "Creates a hosted CI scale set using a verified GitHub App installation or GitLab project connection. ",
            "Pass its resource name with --connection. The connection and machine class are immutable.",
        ));
    }
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
        if m.method.starts_with("runners.scale_sets.") {
            arg = match f.field.as_str() {
                "scale_set.spec.connection" => arg.visible_alias("connection"),
                "scale_set.spec.max_runners" => arg.visible_alias("max-runners"),
                _ => arg,
            };
        }
        cmd = cmd.arg(arg);
    }
    if m.method == "runners.scale_sets.create" {
        cmd = cmd.arg(
            Arg::new("runner-machine")
                .long("machine")
                .value_parser(["standard", "large", "xlarge"])
                .conflicts_with("spec.runner-class")
                .help("Hosted Linux machine class; an alternative to --spec.runner-class"),
        );
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

/// `sylphx <service> …` for every served service in the tree. `hand_written`
/// names the `(service, word)` pairs the binary mounts itself (`build run`): a
/// short command starting with that word gives way to it.
pub fn service_commands(tree: &Tree, hand_written: &[(&str, &str)]) -> Vec<Command> {
    tree.services
        .iter()
        .filter(|s| s.served)
        .filter_map(|s| {
            let has_hand_written = hand_written.iter().any(|(svc, _)| *svc == s.noun.as_str());
            let mut cmd = Command::new(s.noun.clone())
                .about(s.display_name.clone())
                .subcommand_required(true)
                .arg_required_else_help(true);
            for m in s.commands.iter().filter(|m| m.served) {
                if !hand_written.contains(&(s.noun.as_str(), m.verb.as_str())) {
                    cmd = cmd.subcommand(method_command(m));
                }
            }
            let short: Vec<(Vec<&str>, &MethodCmd)> = s
                .porcelain
                .iter()
                .filter(|p| p.command.served)
                .map(|p| (p.words(), &p.command))
                .filter(|(w, _)| !hand_written.contains(&(s.noun.as_str(), w[0])))
                .collect();
            for c in &s.collections {
                if c.collapsed {
                    for m in c.commands.iter().filter(|m| m.served) {
                        if hand_written.contains(&(s.noun.as_str(), m.verb.as_str()))
                            || short
                                .iter()
                                .any(|(w, _)| w.len() == 1 && w[0] == m.verb.as_str())
                        {
                            continue;
                        }
                        cmd = cmd.subcommand(method_command(m));
                    }
                    continue;
                }
                let served_methods: Vec<&MethodCmd> =
                    c.commands.iter().filter(|m| m.served).collect();
                if served_methods.is_empty() {
                    continue;
                }
                let mut cc = Command::new(c.noun.clone())
                    .aliases(c.aliases.iter().cloned())
                    .about(format!("`{}`: {}", c.collection, c.resource_type))
                    .subcommand_required(true)
                    .arg_required_else_help(true);
                for m in served_methods {
                    cc = cc.subcommand(method_command(m));
                }
                cmd = cmd.subcommand(cc);
            }
            let cmd = mount(cmd, &short);
            (cmd.has_subcommands() || has_hand_written).then_some(cmd)
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

/// Finds the served method a generated command line selects.
pub fn find<'t>(
    tree: &'t Tree,
    service: &str,
    matches: &ArgMatches,
) -> Option<(&'t MethodCmd, ArgMatches)> {
    let s = tree
        .services
        .iter()
        .find(|s| s.served && s.noun == service)?;
    let (sub, sub_m) = matches.subcommand()?;
    if let Some(m) = s.commands.iter().find(|m| m.served && m.verb == sub) {
        return Some((m, sub_m.clone()));
    }
    for p in s.porcelain.iter().filter(|p| p.command.served) {
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
    if let Some(m) = s
        .collections
        .iter()
        .filter(|c| c.collapsed)
        .flat_map(|c| &c.commands)
        .find(|m| m.served && m.verb == sub)
    {
        return Some((m, sub_m.clone()));
    }
    let c = s
        .collections
        .iter()
        .find(|c| !c.collapsed && c.noun == sub)?;
    let (verb, verb_m) = sub_m.subcommand()?;
    let m = c.commands.iter().find(|m| m.served && m.verb == verb)?;
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
    if m.method == "runners.scale_sets.create" {
        if let Some(machine) = matches.get_one::<String>("runner-machine") {
            out.push((
                "scale_set.spec.runner_class".to_string(),
                json!(format!("sylphx-linux-{machine}")),
            ));
        }
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
/// A partial update leaves unselected resource fields unchanged; request-level
/// preconditions remain required, as do all fields in a full replacement.
pub fn missing_required(m: &MethodCmd, args: &Value) -> Vec<String> {
    let mask = m
        .is_update()
        .then(|| args.get("update_mask").and_then(Value::as_str))
        .flatten()
        .filter(|mask| !mask.trim().is_empty());
    let resource_prefix = m.resource_field().map(|field| format!("{field}."));
    m.flags
        .iter()
        .filter(|f| f.required && get_path(args, &f.field).is_none())
        .filter(|f| {
            let Some((mask, field)) = mask.zip(
                resource_prefix
                    .as_deref()
                    .and_then(|prefix| f.field.strip_prefix(prefix)),
            ) else {
                return true;
            };
            mask.split(',').map(str::trim).any(|path| {
                path == "*"
                    || path == field
                    || field
                        .strip_prefix(path)
                        .is_some_and(|rest| rest.starts_with('.'))
                    || path
                        .strip_prefix(field)
                        .is_some_and(|rest| rest.starts_with('.'))
            })
        })
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

    #[test]
    fn build_list_help_points_to_the_owning_read_surface() {
        let help = root(&[])
            .try_get_matches_from(["sylphx", "build", "list", "--help"])
            .expect_err("--help must display help");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = help
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        for needle in [
            "Lists standalone Build records in a project",
            "`sylphx build run --no-tree`",
            "`sylphx/deploy` check for your environment",
            "After a Release exists, run `sylphx status`",
            "`sylphx sandboxes leases list`",
            "method build.builds.list",
        ] {
            assert!(help.contains(needle), "missing {needle:?} in {help}");
        }
    }

    #[test]
    fn masked_update_requires_only_selected_resource_fields() {
        let make_flag = |flag: &str, field: &str, required: bool| FlagSpec {
            flag: flag.into(),
            field: field.into(),
            r#type: "string".into(),
            summary: String::new(),
            required,
            sensitive: false,
            repeated: false,
            values: None,
            pattern: None,
            default_from_context: false,
        };
        let method = MethodCmd {
            verb: "update".into(),
            method: "demo.items.update".into(),
            summary: "Update an item".into(),
            served: true,
            http: "PATCH /v1/{item.name}".into(),
            effect: "write".into(),
            positional: Some(Positional {
                arg: "NAME".into(),
                field: "item.name".into(),
                pattern: Some("orgs/{org}/projects/{project}/envs/{env}/items/{item}".into()),
                optional: false,
                default_from_context: false,
                suffix: None,
            }),
            flags: vec![
                make_flag("--policy", "item.policy", false),
                make_flag("--locales", "item.locales", true),
                make_flag("--intake-bound", "item.intake_bound", true),
            ],
            waits: None,
            update_mask: Some("from-flags".into()),
            sends_etag: true,
            paginates: false,
            resource_field_name: Some("item".into()),
        };
        let matches = method_command(&method)
            .try_get_matches_from([
                "update",
                "orgs/o/projects/p/envs/e/items/i",
                "--policy",
                "policy-v2",
            ])
            .unwrap();
        let (values, mask) = flag_values(&method, &matches).unwrap();
        assert_eq!(mask, ["policy"]);
        let mut fields = Map::new();
        for (field, value) in values {
            set_path(&mut fields, &field, value);
        }
        fields.insert("update_mask".into(), Value::String(mask.join(",")));
        let mut args = Value::Object(fields);
        assert!(missing_required(&method, &args).is_empty());
        assert_eq!(
            get_path(&args, "item.policy"),
            Some(&Value::String("policy-v2".into()))
        );
        assert!(get_path(&args, "item.locales").is_none());

        args["update_mask"] = Value::String("policy,locales".into());
        assert_eq!(missing_required(&method, &args), ["--locales"]);
        args["update_mask"] = Value::String("*".into());
        assert_eq!(
            missing_required(&method, &args),
            ["--locales", "--intake-bound"]
        );
        args.as_object_mut().unwrap().remove("update_mask");
        assert_eq!(
            missing_required(&method, &args),
            ["--locales", "--intake-bound"]
        );

        let mut create = method.clone();
        create.verb = "create".into();
        create.method = "demo.items.create".into();
        create.update_mask = None;
        args["update_mask"] = Value::String("policy".into());
        assert!(missing_required(&create, &args).contains(&"--locales".into()));

        let mut request_required = method.clone();
        request_required
            .flags
            .push(make_flag("--expected-version", "expected_version", true));
        assert_eq!(
            missing_required(&request_required, &args),
            ["--expected-version"]
        );
    }

    #[test]
    fn update_mask_matches_resource_path_boundaries() {
        let tree = load();
        let method = tree
            .services
            .iter()
            .flat_map(|s| &s.collections)
            .flat_map(|c| &c.commands)
            .find(|m| m.method == "access.orgs.update")
            .unwrap();
        for (mask, required) in [
            ("spec", true),
            ("spec.slug", true),
            ("spec.slug.child", true),
            ("spec.slug_extra", false),
            ("labels", false),
            ("", true),
        ] {
            let args = serde_json::json!({"update_mask": mask});
            assert_eq!(
                missing_required(method, &args).contains(&"--spec.slug".into()),
                required,
                "{mask}"
            );
        }
    }

    #[test]
    fn every_generated_cli_command_is_served() {
        let tree = load();
        for service in &tree.services {
            assert!(service.served, "unserved service in CLI: {}", service.noun);
            for method in service
                .commands
                .iter()
                .chain(service.collections.iter().flat_map(|c| &c.commands))
            {
                assert!(method.served, "unserved method in CLI: {}", method.method);
                let help = method_command(method).render_long_help().to_string();
                assert!(!help.contains("Not available yet"), "{}", method.method);
            }
        }
        let all_methods: Vec<&str> = tree
            .services
            .iter()
            .flat_map(|s| {
                s.commands
                    .iter()
                    .chain(s.collections.iter().flat_map(|c| &c.commands))
            })
            .map(|m| m.method.as_str())
            .collect();
        assert!(all_methods.contains(&"agents.agent_sessions.decide_approval"));
        assert!(!all_methods.contains(&"notify.support_cases.create"));
        for method in [
            "analytics.analytics_events.list",
            "billing.plans.list",
            "billing.plans.get",
            "billing.meters.list",
            "billing.meters.get",
        ] {
            assert!(
                all_methods.contains(&method),
                "expected served CLI method: {method}"
            );
        }
        for method in [
            "analytics.analytics_events.batch",
            "billing.billing_accounts.get",
            "billing.invoices.list",
        ] {
            assert!(
                !all_methods.contains(&method),
                "expected unserved CLI method omitted: {method}"
            );
        }
        assert!(!tree
            .services
            .iter()
            .any(|s| matches!(s.noun.as_str(), "entitlement" | "network" | "realtime")));
    }

    #[test]
    fn runner_flags_write_canonical_connection_machine_and_cap_fields() {
        let method: MethodCmd = serde_json::from_value(json!({
            "verb": "create", "method": "runners.scale_sets.create",
            "summary": "Creates a scale set", "http": "POST /v1/{parent=orgs/*}/scale_sets",
            "effect": "write", "resource_field": "scale_set",
            "flags": [
                {"flag": "--spec.connection", "field": "scale_set.spec.connection", "type": "string", "required": true},
                {"flag": "--spec.runner-class", "field": "scale_set.spec.runner_class", "type": "string", "required": true},
                {"flag": "--spec.max-runners", "field": "scale_set.spec.max_runners", "type": "int"}
            ]
        })).unwrap();
        for (connection, machine) in [
            ("github", "standard"),
            ("gitlab", "large"),
            ("gitlab", "xlarge"),
        ] {
            let name = format!("orgs/org_example/connections/{connection}");
            let matches = method_command(&method)
                .try_get_matches_from([
                    "create",
                    "--connection",
                    &name,
                    "--machine",
                    machine,
                    "--max-runners",
                    "20",
                ])
                .unwrap();
            let (values, _) = flag_values(&method, &matches).unwrap();
            assert!(values.contains(&("scale_set.spec.connection".into(), json!(name))));
            assert!(values.contains(&(
                "scale_set.spec.runner_class".into(),
                json!(format!("sylphx-linux-{machine}"))
            )));
            assert!(values.contains(&("scale_set.spec.max_runners".into(), json!(20))));
        }
        assert!(method_command(&method)
            .try_get_matches_from([
                "create",
                "--machine",
                "xlarge",
                "--spec.runner-class",
                "sylphx-linux-large",
            ])
            .is_err());
        assert!(method_command(&method)
            .try_get_matches_from(["create", "--machine", "not-a-class",])
            .is_err());
    }

    /// `MethodPolicy.porcelain` and collapsed stutter collections mount cleanly
    /// and resolve to their backing methods.
    #[test]
    fn short_commands_and_collapsed_stutters_mount_and_resolve() {
        root(&[]).debug_assert();
        root(crate::HAND_WRITTEN).debug_assert();
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
            // Collapsed stutter collections mount directly under the service noun.
            (
                &["sylphx", "build", "read-logs", "orgs/o/projects/p/builds/b"],
                "build.builds.read_logs",
            ),
            (
                &["sylphx", "secrets", "list", "orgs/o/projects/p/envs/e"],
                "secrets.secrets.list",
            ),
            (
                &["sylphx", "agents", "list", "orgs/o/projects/p/envs/e"],
                "agents.agents.list",
            ),
            (
                &["sylphx", "workflows", "list", "orgs/o/projects/p/envs/e"],
                "workflows.workflows.list",
            ),
            (
                &["sylphx", "release", "list", "orgs/o/projects/p/envs/e"],
                "release.releases.list",
            ),
            // Acronym kebab normalization.
            (
                &[
                    "sylphx",
                    "broker",
                    "trust-policies",
                    "open-github-pull-request",
                    "orgs/o/trust_policies/tp",
                ],
                "broker.trust_policies.open_git_hub_pull_request",
            ),
        ] {
            assert_eq!(
                method_of(root(&[]), argv).as_deref(),
                Some(method),
                "{argv:?}"
            );
        }
        let cmd = root(&[]);
        assert!(cmd
            .find_subcommand("secrets")
            .unwrap()
            .find_subcommand("secrets")
            .is_none());
        assert!(cmd
            .find_subcommand("agents")
            .unwrap()
            .find_subcommand("agents")
            .is_none());
        assert!(cmd
            .find_subcommand("build")
            .unwrap()
            .find_subcommand("builds")
            .is_none());
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

    fn stub_method(verb: &str, method: &str, served: bool) -> MethodCmd {
        MethodCmd {
            verb: verb.into(),
            method: method.into(),
            summary: format!("Summary for {method}"),
            served,
            http: "GET /v1/demo".into(),
            effect: "read".into(),
            positional: None,
            flags: vec![],
            waits: None,
            update_mask: None,
            sends_etag: false,
            paginates: false,
            resource_field_name: None,
        }
    }

    #[test]
    fn unserved_services_and_methods_are_omitted_and_rejected() {
        let tree = Tree {
            services: vec![
                ServiceCmd {
                    noun: "vapor".into(),
                    display_name: "Vaporware".into(),
                    served: false,
                    commands: vec![stub_method("ping", "vapor.ping", true)],
                    collections: vec![CollectionCmd {
                        noun: "items".into(),
                        aliases: vec![],
                        collection: "items".into(),
                        resource_type: "vapor.sylphx.com/Item".into(),
                        collapsed: false,
                        commands: vec![stub_method("list", "vapor.items.list", true)],
                    }],
                    porcelain: vec![PorcelainCmd {
                        path: vec!["quick".into()],
                        command: stub_method("run", "vapor.items.run", true),
                    }],
                },
                ServiceCmd {
                    noun: "demo".into(),
                    display_name: "Demo".into(),
                    served: true,
                    commands: vec![
                        stub_method("live", "demo.live", true),
                        stub_method("stub", "demo.stub", false),
                    ],
                    collections: vec![
                        CollectionCmd {
                            noun: "widgets".into(),
                            aliases: vec![],
                            collection: "widgets".into(),
                            resource_type: "demo.sylphx.com/Widget".into(),
                            collapsed: false,
                            commands: vec![
                                stub_method("list", "demo.widgets.list", true),
                                stub_method("create", "demo.widgets.create", false),
                            ],
                        },
                        CollectionCmd {
                            noun: "ghosts".into(),
                            aliases: vec![],
                            collection: "ghosts".into(),
                            resource_type: "demo.sylphx.com/Ghost".into(),
                            collapsed: false,
                            commands: vec![stub_method("list", "demo.ghosts.list", false)],
                        },
                        CollectionCmd {
                            noun: "demo".into(),
                            aliases: vec![],
                            collection: "demos".into(),
                            resource_type: "demo.sylphx.com/Demo".into(),
                            collapsed: true,
                            commands: vec![
                                stub_method("get", "demo.demos.get", true),
                                stub_method("purge", "demo.demos.purge", false),
                            ],
                        },
                    ],
                    porcelain: vec![
                        PorcelainCmd {
                            path: vec!["shortcut".into()],
                            command: stub_method("go", "demo.widgets.go", true),
                        },
                        PorcelainCmd {
                            path: vec!["dead".into()],
                            command: stub_method("end", "demo.widgets.end", false),
                        },
                    ],
                },
                ServiceCmd {
                    noun: "empty".into(),
                    display_name: "Empty".into(),
                    served: true,
                    commands: vec![stub_method("gone", "empty.gone", false)],
                    collections: vec![CollectionCmd {
                        noun: "things".into(),
                        aliases: vec![],
                        collection: "things".into(),
                        resource_type: "empty.sylphx.com/Thing".into(),
                        collapsed: false,
                        commands: vec![stub_method("list", "empty.things.list", false)],
                    }],
                    porcelain: vec![PorcelainCmd {
                        path: vec![],
                        command: stub_method("nope", "empty.nope", false),
                    }],
                },
            ],
        };

        let mut root_cmd = Command::new("sylphx");
        for s in service_commands(&tree, &[]) {
            root_cmd = root_cmd.subcommand(s);
        }
        root_cmd.clone().debug_assert();

        // Unserved and filtered-empty services are omitted.
        assert!(root_cmd.find_subcommand("vapor").is_none());
        assert!(root_cmd.find_subcommand("empty").is_none());

        // A filtered-empty served service stays mounted only if a hand-written
        // subcommand owns it.
        let with_hand_written = service_commands(&tree, &[("empty", "custom")]);
        assert!(with_hand_written.iter().any(|c| c.get_name() == "empty"));
        assert!(!with_hand_written.iter().any(|c| c.get_name() == "vapor"));

        // Served service mounts only served methods, porcelain, and non-empty collections.
        let demo = root_cmd.find_subcommand("demo").expect("demo");
        assert!(demo.find_subcommand("live").is_some());
        assert!(demo.find_subcommand("stub").is_none());
        assert!(demo.find_subcommand("get").is_some());
        assert!(demo.find_subcommand("purge").is_none());
        assert!(demo.find_subcommand("ghosts").is_none());
        assert!(demo.find_subcommand("dead").is_none());
        assert!(demo
            .find_subcommand("shortcut")
            .and_then(|c| c.find_subcommand("go"))
            .is_some());
        let widgets = demo.find_subcommand("widgets").expect("widgets");
        assert!(widgets.find_subcommand("list").is_some());
        assert!(widgets.find_subcommand("create").is_none());

        // `find` rejects every unserved service, method, collection method, and porcelain entry
        // even against a synthetic clap match tree.
        let synthetic_matches = |argv: &[&str]| -> ArgMatches {
            let mut leaf = Command::new(argv[argv.len() - 1].to_string());
            for seg in argv[1..argv.len() - 1].iter().rev() {
                leaf = Command::new((*seg).to_string()).subcommand(leaf);
            }
            Command::new(argv[0].to_string())
                .subcommand(leaf)
                .try_get_matches_from(argv)
                .unwrap()
        };

        for (service, argv) in [
            ("vapor", &["vapor", "ping"][..]),
            ("vapor", &["vapor", "items", "list"][..]),
            ("vapor", &["vapor", "quick", "run"][..]),
            ("demo", &["demo", "stub"][..]),
            ("demo", &["demo", "widgets", "create"][..]),
            ("demo", &["demo", "ghosts", "list"][..]),
            ("demo", &["demo", "purge"][..]),
            ("demo", &["demo", "dead", "end"][..]),
        ] {
            let m = synthetic_matches(argv);
            assert!(
                find(&tree, service, &m).is_none(),
                "find must reject unserved {argv:?}"
            );
        }

        // Served paths resolve cleanly.
        for (argv, expected) in [
            (&["demo", "live"][..], "demo.live"),
            (&["demo", "widgets", "list"][..], "demo.widgets.list"),
            (&["demo", "get"][..], "demo.demos.get"),
            (&["demo", "shortcut", "go"][..], "demo.widgets.go"),
        ] {
            let m = synthetic_matches(argv);
            let (found, _) = find(&tree, "demo", &m).expect("served method");
            assert_eq!(found.method, expected);
        }

        // No dead "Not available yet" copy remains on method commands.
        let help = method_command(&stub_method("stub", "demo.stub", false))
            .render_long_help()
            .to_string();
        assert!(!help.to_ascii_lowercase().contains("not available yet"));
        assert!(!help.contains("501 UNIMPLEMENTED"));
    }
}
