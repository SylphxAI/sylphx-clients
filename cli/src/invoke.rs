//! Generic invocation of schema-generated CLI methods: flag/file merging,
//! context defaults, etag pre-reads, destructive confirmation, pagination, and
//! long-running operation waiting.

use std::io::{IsTerminal, Read, Write};

use clap::ArgMatches;
use serde_json::{json, Map, Value};
use sylphx::{Client, Error, HttpRequest};

use crate::output::Format;
use crate::tree::MethodCmd;
use crate::{names, output, transport, tree, Defaults, Failure};

/// A singleton's name: its parent and its literal segment; any other
/// PARENT is the name as is.
fn singleton_name(parent: String, suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("{parent}/{s}"),
        None => parent,
    }
}

/// Compare numeric flags using their protobuf value, not their JSON spelling.
fn same_field_value(existing: &Value, value: &Value, ty: &str) -> bool {
    if existing == value {
        return true;
    }
    match ty {
        "number" => existing
            .as_f64()
            .zip(value.as_f64())
            .is_some_and(|(a, b)| a == b),
        "int" => existing.as_str().is_some_and(|s| {
            s.parse::<i64>()
                .ok()
                .zip(value.as_i64())
                .is_some_and(|(a, b)| a == b)
                || s.parse::<u64>()
                    .ok()
                    .zip(value.as_u64())
                    .is_some_and(|(a, b)| a == b)
        }),
        _ => false,
    }
}

/// Keep OAuth secrets out of the command line and bound bytes read from a pipe or file.
fn social_provider_secret(source: &str) -> Result<String, Failure> {
    let input: Box<dyn Read> = if source == "-" && !std::io::stdin().is_terminal() {
        Box::new(std::io::stdin())
    } else if let Some(path) = source.strip_prefix('@').filter(|p| !p.is_empty()) {
        Box::new(
            std::fs::File::open(path).map_err(|_| {
                Failure::Usage("--client-secret: could not open secret file".into())
            })?,
        )
    } else {
        return Err(Failure::Usage(
            "--client-secret needs @FILE or piped stdin (-); do not pass a secret on the command line".into(),
        ));
    };
    let mut secret = String::new();
    input
        .take(4098)
        .read_to_string(&mut secret)
        .map_err(|_| Failure::Usage("--client-secret: could not read UTF-8 secret input".into()))?;
    let secret = secret.trim_end_matches(['\r', '\n']);
    if secret.trim().is_empty() || secret.len() > 4096 {
        return Err(Failure::Usage("--client-secret needs 1–4096 bytes".into()));
    }
    Ok(secret.to_owned())
}

pub(crate) async fn generated(
    client: &Client,
    method: &MethodCmd,
    mm: &ArgMatches,
    root: &ArgMatches,
    format: Format,
) -> Result<(), Failure> {
    let configures_social = method.method == "auth.auth_configs.configure_social_provider";
    let mut args = match root.get_one::<String>("from-file") {
        Some(path) => {
            let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
            let parse_error = |error: &dyn std::fmt::Display| {
                if configures_social {
                    format!("{path}: invalid provider configuration request")
                } else {
                    format!("{path}: {error}")
                }
            };
            let v: Value = if path.ends_with(".json") {
                serde_json::from_str(&text).map_err(|e| parse_error(&e))?
            } else {
                serde_yaml_ng::from_str(&text).map_err(|e| parse_error(&e))?
            };
            match v {
                Value::Object(o) => o,
                _ => {
                    return Err(Failure::Usage(format!(
                        "{path}: the request must be an object"
                    )))
                }
            }
        }
        None => Map::new(),
    };
    // Files contain the whole method request, not its HTTP resource body.
    // Validate before resolving defaults (which can contact the API), so a
    // misplaced field can never disappear while the SDK shapes that body.
    if method.method == "sandboxes.leases.create" && root.get_one::<String>("from-file").is_some() {
        let ix = sylphx_resource_codec::ir::Index::embedded();
        if let Some((request, _)) = ix.methods.iter().find(|(m, _)| m.id == method.method) {
            sylphx_resource_codec::schema::validate_field_names(
                &ix,
                &request.request,
                &Value::Object(args.clone()),
                "",
                sylphx_resource_codec::schema::FieldNames::Canonical,
            )
            .map_err(Failure::Input)?;
        }
    }
    let file_args = Value::Object(args.clone());
    let (mut values, mask) = tree::flag_values(method, mm)?;
    if configures_social {
        for (field, value) in &mut values {
            if field == "client_secret" {
                *value = Value::String(social_provider_secret(value.as_str().unwrap_or_default())?);
            }
        }
    }
    for (field, value) in &values {
        if let Some(existing) = tree::get_path(&file_args, field) {
            let flag = method
                .flags
                .iter()
                .find(|f| &f.field == field)
                .expect("flag field");
            if !same_field_value(existing, value, &flag.r#type) {
                let detail = if flag.sensitive {
                    ": values redacted".to_string()
                } else {
                    format!(": file value {existing}, flag value {value}")
                };
                return Err(Failure::Usage(format!(
                    "{} conflicts with --from-file at {field}{detail}",
                    flag.flag
                )));
            }
        }
    }
    let mut defaults = Defaults {
        client,
        link: None,
        org: root.get_one::<String>("org").cloned(),
    };

    if let Some(p) = &method.positional {
        let given = mm.get_one::<String>("__positional").cloned();
        let value = match (p.arg.as_str(), given) {
            ("ID", v) => v,
            ("PARENT", Some(v)) => Some(singleton_name(
                names::resolve(client, &v).await?,
                p.suffix.as_deref(),
            )),
            ("PARENT", None)
                if tree::get_path(&Value::Object(args.clone()), &p.field).is_none() =>
            {
                Some(singleton_name(
                    defaults
                        .at(p.pattern.as_deref().unwrap_or_default())
                        .await?,
                    p.suffix.as_deref(),
                ))
            }
            (_, Some(v)) => Some(
                defaults
                    .name(&v, p.pattern.as_deref().unwrap_or_default())
                    .await?,
            ),
            _ => None,
        };
        if let Some(v) = value {
            let value = Value::String(v);
            if let Some(existing) = tree::get_path(&file_args, &p.field) {
                if existing != &value {
                    return Err(Failure::Usage(format!(
                        "{} conflicts with --from-file at {}: file value {existing}, positional value {value}",
                        p.arg, p.field
                    )));
                }
            }
            tree::set_path(&mut args, &p.field, value);
        }
    }
    for (field, value) in values {
        tree::set_path(&mut args, &field, value);
    }
    // `--parent` defaults from context.
    for f in method.flags.iter().filter(|f| f.default_from_context) {
        if tree::get_path(&Value::Object(args.clone()), &f.field).is_none() {
            let v = defaults
                .at(f.pattern.as_deref().unwrap_or_default())
                .await?;
            tree::set_path(&mut args, &f.field, Value::String(v));
        }
    }
    // A file is a full request: preserve its explicit mask, or its unmasked
    // update semantics. Only flags-only updates derive a mask from flags.
    if method.is_update()
        && root.get_one::<String>("from-file").is_none()
        && !mask.is_empty()
        && !args.contains_key("update_mask")
    {
        args.insert("update_mask".into(), Value::String(mask.join(",")));
    }
    let missing = tree::missing_required(method, &Value::Object(args.clone()));
    if !missing.is_empty() {
        return Err(Failure::Usage(format!(
            "missing required flags: {}",
            missing.join(", ")
        )));
    }

    // Updates and deletes send the etag they read (§3.5), unless one is given.
    if method.sends_etag || method.flags.iter().any(|f| f.field == "etag") {
        etag(client, method, &mut args).await?;
    }

    if method.effect == "destructive" && !root.get_flag("yes") {
        let snapshot = Value::Object(args.clone());
        let target = method
            .positional
            .as_ref()
            .and_then(|p| tree::get_path(&snapshot, &p.field))
            .and_then(Value::as_str)
            .unwrap_or(&method.method)
            .to_string();
        confirm(&format!("{} {target}?", method.verb))?;
    }

    if sylphx::methods::method(&method.method).is_some_and(|spec| spec.server_streaming) {
        use futures_util::StreamExt;
        let mut stream = client
            .invoke_stream(&method.method, Value::Object(args))
            .await?;
        while let Some(event) = stream.next().await {
            println!("{}", output::render(&event?, format));
        }
        return Ok(());
    }
    // Manual grants use their caller-chosen ID as the replay key. Scope this
    // contract to the grant create call; other mutations keep SDK-generated keys.
    let grant_id = (method.method == "money.entitlement_grants.create")
        .then(|| args.get("entitlement_grant_id").and_then(Value::as_str))
        .flatten();
    let mut result = if let Some(id) = grant_id {
        Client::new(transport::IdempotencyTransport::new(client.transport(), id))
            .invoke(&method.method, Value::Object(args.clone()))
            .await?
    } else {
        client
            .invoke(&method.method, Value::Object(args.clone()))
            .await?
    };
    // Use the authority's resolved spec, not client guesses about defaults.
    // Keep stdout machine-readable for --output json/yaml consumers.
    if method.method == "sandboxes.leases.create" {
        if let Some(spec) = result.get("spec") {
            eprintln!(
                "Resolved lease spec: kind={} image={} size={} ttl={}",
                spec.get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                spec.get("image")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                spec.get("shape")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                spec.get("ttl").and_then(Value::as_str).unwrap_or("unknown"),
            );
        }
    }
    if method.paginates && root.get_flag("all") {
        result = follow_pages(client, method, args, result).await?;
    }
    if method.long_running() && !root.get_flag("no-wait") && result.get("validate_only").is_none() {
        result = wait(client, result).await?;
    }
    let text = output::render(&result, format);
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(())
}

/// Reads the Resource first and sends its etag with the write.
async fn etag(
    client: &Client,
    method: &MethodCmd,
    args: &mut Map<String, Value>,
) -> Result<(), Failure> {
    let Some(get) = method
        .method
        .rsplit_once('.')
        .map(|(c, _)| format!("{c}.get"))
    else {
        return Ok(());
    };
    let (name_field, etag_path) = match method.resource_field() {
        Some(r) if method.is_update() => (format!("{r}.name"), format!("{r}.meta.etag")),
        _ => ("name".to_string(), "etag".to_string()),
    };
    let current = Value::Object(args.clone());
    if tree::get_path(&current, &etag_path).is_some() || sylphx::methods::method(&get).is_none() {
        return Ok(());
    }
    let Some(name) = tree::get_path(&current, &name_field)
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(());
    };
    match client.invoke(&get, json!({ "name": name })).await {
        Ok(r) => {
            if let Some(e) = r
                .get("meta")
                .and_then(|m| m.get("etag"))
                .and_then(Value::as_str)
            {
                tree::set_path(args, &etag_path, Value::String(e.to_string()));
            }
            Ok(())
        }
        // An upsert of a missing Resource has no etag to send.
        Err(Error::Api { status: 404, .. }) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn confirm(prompt: &str) -> Result<(), Failure> {
    if !std::io::stdin().is_terminal() {
        return Err(Failure::Usage(format!(
            "{prompt} This is destructive: pass --yes to confirm"
        )));
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if matches!(line.trim(), "y" | "Y" | "yes") {
        Ok(())
    } else {
        Err(Failure::Usage("aborted".into()))
    }
}

async fn follow_pages(
    client: &Client,
    method: &MethodCmd,
    mut args: Map<String, Value>,
    first: Value,
) -> Result<Value, Failure> {
    let key = first
        .as_object()
        .and_then(|o| {
            o.iter()
                .find(|(k, v)| k.as_str() != "next_page_token" && v.is_array())
        })
        .map(|(k, _)| k.clone());
    let Some(key) = key else {
        return Ok(first);
    };
    let mut items = first[&key].as_array().cloned().unwrap_or_default();
    let mut token = first["next_page_token"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    while !token.is_empty() {
        args.insert("page_token".into(), Value::String(token));
        let page = client
            .invoke(&method.method, Value::Object(args.clone()))
            .await?;
        items.extend(page[&key].as_array().cloned().unwrap_or_default());
        token = page["next_page_token"]
            .as_str()
            .unwrap_or_default()
            .to_string();
    }
    Ok(json!({ key: items }))
}

/// Waits for an Operation (spec §3.9) and returns its settled Resource.
async fn wait(client: &Client, mut op: Value) -> Result<Value, Failure> {
    let Some(name) = op.get("name").and_then(Value::as_str).map(str::to_string) else {
        return Ok(op);
    };
    if !name.contains("/operations/") {
        return Ok(op);
    }
    let tty = std::io::stderr().is_terminal();
    while op.get("done") != Some(&Value::Bool(true)) {
        if tty {
            let msg = op
                .get("progress")
                .and_then(|p| p.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("waiting");
            eprint!("\r{msg}…");
        }
        op = client
            .call(HttpRequest {
                method: "POST",
                path: format!("/v1/{name}:wait"),
                query: vec![("timeout".into(), "60s".into())],
                body: Some(json!({})),
                mutation: false,
                origin: None,
                effect_ids: false,
            })
            .await?;
    }
    if tty {
        eprint!("\r");
    }
    if let Some(err) = op.get("error").filter(|e| !e.is_null()) {
        let problem: sylphx::common::ProblemDetails =
            serde_json::from_value(err.clone()).map_err(|e| Failure::Usage(e.to_string()))?;
        return Err(Failure::Api(Error::Api {
            code: sylphx::common::ErrorCode::from_wire(&problem.code),
            status: problem.status.max(0) as u16,
            retryable: problem.retryable,
            effect: problem.effect.clone(),
            detail: problem.detail.clone(),
            request_id: None,
            problem: Box::new(problem),
        }));
    }
    Ok(match op.get("response") {
        Some(Value::Object(r)) => {
            let mut r = r.clone();
            r.remove("@type");
            Value::Object(r)
        }
        _ => op,
    })
}
