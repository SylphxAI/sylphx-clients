//! `sylphx events listen`: a topic's deliveries relayed to a local URL while
//! you develop, signed as Sylphx Events signs a webhook (Standard Webhooks).
//!
//! It needs nothing new on the server: the command creates a short-lived
//! Queue and a Subscription of the topic into it, long-polls the Queue
//! (`:lease`, up to 20 s a call), POSTs each event to `--forward` as the
//! structured CloudEvent JSON body a webhook endpoint receives, and acks it.
//! A delivery the receiver answers (any status) is acked and printed; one it
//! cannot reach (not started yet) is nacked and tried again in a few
//! seconds. On Ctrl-C, or after `--count` events, the Subscription and the
//! Queue are deleted.
//!
//! The signature is `webhook-signature: v1,<base64 HMAC-SHA256>` of
//! `"{webhook-id}.{webhook-timestamp}.{body}"` under a local `whsec_…`
//! secret: `--secret`, else `SYLPHX_LISTEN_SECRET`, else one generated once
//! and kept in the config directory (`listen-secret`, 0600), so a receiver
//! configured with it keeps verifying across runs. `webhook-id` is the queue
//! message id, stable across retries, as on a real endpoint.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use clap::{Arg, ArgAction, ArgMatches, Command};
use hmac::{Hmac, Mac};
use serde_json::{json, Map, Value};
use sha2::Sha256;
use sylphx::events::{CloudEvent, LeaseQueueMessagesResponse, LeasedMessage, MessageLease};
use sylphx::Client;

use crate::{context, devices, names, Failure};

/// The env's whole bus, the one topic served today.
const DEFAULT_TOPIC: &str = "default";
/// The longest long-poll the Queue allows.
const LEASE_WAIT: &str = "20s";
const LEASE_BATCH: i32 = 10;
/// Long enough for a slow local handler; a crash re-delivers after it.
const VISIBILITY: &str = "60s";
/// The listen Queue keeps an unacked event this long at most.
const RETENTION: &str = "3600s";
/// How long an unreachable receiver waits before the event is tried again.
const RETRY_DELAY: &str = "3s";
/// A crashed listener's backlog stops retrying after this many tries.
const MAX_DELIVERIES: i32 = 20;
const FORWARD_TIMEOUT: Duration = Duration::from_secs(30);
const SECRET_FILE: &str = "listen-secret";
const SECRET_PREFIX: &str = "whsec_";

pub fn command() -> Command {
    Command::new("listen")
        .about("Relay a topic's events to a local URL while you develop, signed like a webhook (Standard Webhooks)")
        .long_about("Creates a temporary Queue and a Subscription of TOPIC into it, then POSTs every event published to the topic to --forward as the CloudEvent JSON body a webhook endpoint receives, with webhook-id, webhook-timestamp and webhook-signature headers (Standard Webhooks). Verify them with the signing secret printed at start. Ctrl-C (or --count) deletes the Queue and the Subscription.\n\nExample:\n  sylphx events listen --forward http://localhost:3000/hook")
        .arg(
            Arg::new("topic")
                .value_name("TOPIC")
                .default_value(DEFAULT_TOPIC)
                .help("The topic: an id in the environment, or orgs/…/envs/…/topics/…; default: the environment's whole bus"),
        )
        .arg(
            Arg::new("forward")
                .long("forward")
                .visible_alias("forward-to")
                .value_name("URL")
                .required(true)
                .help("Where each event is POSTed, e.g. http://localhost:3000/hook (a bare host:port means http://)"),
        )
        .arg(
            Arg::new("env")
                .long("env")
                .value_name("ENV")
                .help("The environment (a slug, an id, or orgs/…/envs/…); defaults to the linked env, else the key's scope"),
        )
        .arg(
            Arg::new("types")
                .long("types")
                .value_name("TYPE")
                .value_delimiter(',')
                .action(ArgAction::Append)
                .help("Only these CloudEvents types (a trailing * matches a prefix); default: all"),
        )
        .arg(
            Arg::new("sources")
                .long("sources")
                .value_name("SOURCE")
                .value_delimiter(',')
                .action(ArgAction::Append)
                .help("Only these sources; default: all"),
        )
        .arg(
            Arg::new("secret")
                .long("secret")
                .value_name("WHSEC")
                .help("The whsec_… signing secret; default SYLPHX_LISTEN_SECRET, else one kept in the config directory"),
        )
        .arg(
            Arg::new("count")
                .long("count")
                .value_name("N")
                .value_parser(clap::value_parser!(u64).range(1..))
                .help("Exit after forwarding N events (scripts and tests)"),
        )
}

/// The forward target with a scheme: `localhost:3000/hook` is plain HTTP.
pub fn forward_url(given: &str) -> Result<String, String> {
    let url = if given.starts_with("http://") || given.starts_with("https://") {
        given.to_string()
    } else if given.contains("://") {
        return Err(format!(
            "--forward takes an http:// or https:// URL, not `{given}`"
        ));
    } else {
        format!("http://{given}")
    };
    reqwest::Url::parse(&url).map_err(|e| format!("--forward `{given}`: {e}"))?;
    Ok(url)
}

/// The signing key a `whsec_<base64>` secret stands for.
pub fn signing_key(secret: &str) -> Result<Vec<u8>, String> {
    let b64 = secret.strip_prefix(SECRET_PREFIX).unwrap_or(secret);
    let key = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|_| "the signing secret is whsec_ followed by base64".to_string())?;
    if key.len() < 16 {
        return Err("the signing secret holds fewer than 16 bytes".into());
    }
    Ok(key)
}

/// A new `whsec_…` secret of 24 random bytes.
fn new_secret() -> Result<String, String> {
    let mut key = [0u8; 24];
    getrandom::getrandom(&mut key).map_err(|e| format!("no random source: {e}"))?;
    Ok(format!(
        "{SECRET_PREFIX}{}",
        base64::engine::general_purpose::STANDARD.encode(key)
    ))
}

/// The secret kept in `dir` (created there, mode 0600, the first time).
fn kept_secret(dir: &Path) -> Result<String, String> {
    let path = dir.join(SECRET_FILE);
    if let Ok(s) = std::fs::read_to_string(&path) {
        let s = s.trim().to_string();
        if signing_key(&s).is_ok() {
            return Ok(s);
        }
    }
    let secret = new_secret()?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    write_private(&path, &secret).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(secret)
}

#[cfg(unix)]
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(text.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    std::fs::write(path, text)
}

/// `--secret`, else `SYLPHX_LISTEN_SECRET`, else the kept one.
fn secret(given: Option<&str>) -> Result<String, String> {
    if let Some(s) = given
        .map(str::to_string)
        .or_else(|| std::env::var("SYLPHX_LISTEN_SECRET").ok())
        .filter(|s| !s.is_empty())
    {
        signing_key(&s)?;
        return Ok(s);
    }
    let dir = context::config_dir()
        .ok_or("no config directory: pass --secret or set SYLPHX_LISTEN_SECRET")?;
    kept_secret(&dir)
}

/// The Standard Webhooks `v1,<base64 HMAC-SHA256>` of `body`.
pub fn sign(key: &[u8], id: &str, timestamp: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(timestamp.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    )
}

/// The structured-mode CloudEvent JSON a webhook endpoint receives
/// (services/events `cloud_event_body`): the context attributes, the data,
/// and each extension as a top-level attribute.
pub fn webhook_body(event: &CloudEvent) -> Value {
    let mut o = Map::new();
    o.insert("specversion".into(), json!("1.0"));
    o.insert("id".into(), json!(event.id));
    o.insert("source".into(), json!(event.source));
    o.insert("type".into(), json!(event.r#type));
    let mut put = |k: &str, v: &str| {
        if !v.is_empty() {
            o.insert(k.into(), json!(v));
        }
    };
    put("subject", &event.subject);
    put("time", &event.event_time);
    put("datacontenttype", &event.data_content_type);
    put("dataschema", &event.data_schema);
    if !event.data.is_null() {
        o.insert("data".into(), event.data.clone());
    }
    for (k, v) in &event.extensions {
        o.entry(k.clone()).or_insert_with(|| json!(v));
    }
    Value::Object(o)
}

/// `listen-<10 hex>`: a Queue and Subscription id no other run picks.
fn run_id() -> Result<String, String> {
    let mut b = [0u8; 5];
    getrandom::getrandom(&mut b).map_err(|e| format!("no random source: {e}"))?;
    Ok(format!(
        "listen-{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    ))
}

/// The env and the full topic name `TOPIC` and `--env` stand for.
async fn target(
    client: &Client,
    topic: &str,
    env: Option<&str>,
) -> Result<(String, String), Failure> {
    if topic.starts_with("orgs/") {
        let full = names::resolve(client, topic).await?;
        let env = full
            .split_once("/topics/")
            .map(|(e, _)| e.to_string())
            .ok_or_else(|| Failure::Usage(format!("`{topic}` is not orgs/…/envs/…/topics/…")))?;
        return Ok((env, full));
    }
    let env = devices::parent_env(client, env)
        .await
        .map_err(Failure::Usage)?;
    let topic = format!("{env}/topics/{topic}");
    Ok((env, topic))
}

struct Opts {
    forward: String,
    key: Vec<u8>,
    secret: String,
    types: Vec<String>,
    sources: Vec<String>,
    count: Option<u64>,
    json: bool,
}

pub async fn run(client: &Client, m: &ArgMatches, json: bool) -> Result<(), Failure> {
    let forward = forward_url(m.get_one::<String>("forward").expect("required"))?;
    let secret = secret(m.get_one::<String>("secret").map(String::as_str))?;
    let key = signing_key(&secret)?;
    let list = |name: &str| -> Vec<String> {
        m.get_many::<String>(name)
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .cloned()
            .collect()
    };
    let opts = Opts {
        forward,
        key,
        secret,
        types: list("types"),
        sources: list("sources"),
        count: m.get_one::<u64>("count").copied(),
        json,
    };
    let topic = m.get_one::<String>("topic").expect("defaulted");
    let (env, topic) = target(
        client,
        topic,
        m.get_one::<String>("env").map(String::as_str),
    )
    .await?;
    let id = run_id()?;
    let queue = format!("{env}/queues/{id}");
    let subscription = format!("{env}/subscriptions/{id}");

    client
        .invoke(
            "events.queues.create",
            json!({
                "parent": env,
                "queue_id": id,
                "queue": {"spec": {
                    "visibility_timeout": VISIBILITY,
                    "max_deliveries": MAX_DELIVERIES,
                    "retention": RETENTION,
                }},
            }),
        )
        .await?;
    // From here on the Queue exists: every way out deletes what was made.
    let made = async {
        client
            .invoke(
                "events.subscriptions.create",
                json!({
                    "parent": env,
                    "subscription_id": id,
                    "subscription": {"spec": {
                        "topic": topic,
                        "types": opts.types,
                        "sources": opts.sources,
                        "queue": queue,
                    }},
                }),
            )
            .await?;
        announce(&opts, &topic);
        relay(client, &queue, &opts).await
    };
    let outcome = tokio::select! {
        r = made => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    cleanup(client, &subscription, &queue).await;
    outcome
}

/// On stderr, so `-o json` keeps stdout to one JSON line per event.
fn announce(opts: &Opts, topic: &str) {
    eprintln!("Ready: events on {topic} go to {}", opts.forward);
    eprintln!("Signing secret (Standard Webhooks): {}", opts.secret);
    eprintln!("Ctrl-C stops and deletes the listener.");
}

async fn relay(client: &Client, queue: &str, opts: &Opts) -> Result<(), Failure> {
    let http = reqwest::Client::builder()
        .timeout(FORWARD_TIMEOUT)
        .user_agent(concat!(
            "sylphx-cli/",
            env!("CARGO_PKG_VERSION"),
            " (events listen)"
        ))
        .build()
        .map_err(|e| Failure::Refused(format!("HTTP client: {e}")))?;
    let mut forwarded = 0u64;
    loop {
        let leased: LeaseQueueMessagesResponse = serde_json::from_value(
            client
                .invoke(
                    "events.queues.lease",
                    json!({"name": queue, "max_messages": LEASE_BATCH, "wait": LEASE_WAIT}),
                )
                .await?,
        )
        .map_err(|e| Failure::Refused(format!("lease answer: {e}")))?;
        let mut acks = vec![];
        let mut nacks = vec![];
        for message in &leased.messages {
            let Some(lease) = message.lease.clone() else {
                continue;
            };
            if forward(&http, opts, message).await {
                acks.push(lease);
            } else {
                nacks.push(lease);
            }
        }
        settle(client, queue, "events.queues.ack", acks.as_slice(), None).await?;
        settle(
            client,
            queue,
            "events.queues.nack",
            nacks.as_slice(),
            Some(RETRY_DELAY),
        )
        .await?;
        forwarded += (leased.messages.len() - nacks.len()) as u64;
        if opts.count.is_some_and(|n| forwarded >= n) {
            return Ok(());
        }
    }
}

async fn settle(
    client: &Client,
    queue: &str,
    method: &str,
    leases: &[MessageLease],
    delay: Option<&str>,
) -> Result<(), Failure> {
    if leases.is_empty() {
        return Ok(());
    }
    let mut args = json!({"name": queue, "leases": leases});
    if let Some(d) = delay {
        args["delay"] = json!(d);
    }
    client.invoke(method, args).await?;
    Ok(())
}

/// POSTs one event; true when the receiver answered (it is printed either
/// way), false when it could not be reached.
async fn forward(http: &reqwest::Client, opts: &Opts, message: &LeasedMessage) -> bool {
    let event = message.event.clone().unwrap_or_default();
    let body = serde_json::to_vec(&webhook_body(&event)).unwrap_or_default();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let id = &message.message_id;
    let started = Instant::now();
    let answer = http
        .post(&opts.forward)
        .header("content-type", "application/json")
        .header("webhook-id", id)
        .header("webhook-timestamp", timestamp.to_string())
        .header("webhook-signature", sign(&opts.key, id, timestamp, &body))
        .body(body)
        .send()
        .await;
    let ms = started.elapsed().as_millis();
    let (status, error) = match &answer {
        Ok(r) => (Some(r.status().as_u16()), None),
        Err(e) => (None, Some(e.to_string())),
    };
    if opts.json {
        println!(
            "{}",
            json!({"webhook_id": id, "id": event.id, "type": event.r#type,
                   "status": status, "error": error, "ms": ms,
                   "delivery": message.delivery_count})
        );
    } else {
        match (status, &error) {
            (Some(s), _) => println!("{} {}  -> {s} ({ms} ms)", event.r#type, event.id),
            (None, Some(e)) => println!(
                "{} {}  -> not delivered: {e}; retrying in {RETRY_DELAY}",
                event.r#type, event.id
            ),
            _ => {}
        }
    }
    status.is_some()
}

/// Deletes the Subscription, then the Queue; a failure is said, not fatal.
async fn cleanup(client: &Client, subscription: &str, queue: &str) {
    for (method, name) in [
        ("events.subscriptions.delete", subscription),
        ("events.queues.delete", queue),
    ] {
        if let Err(e) = client
            .invoke(method, json!({"name": name, "allow_missing": true}))
            .await
        {
            eprintln!("warning: could not delete {name} ({e}); delete it with `sylphx api DELETE /v1/{name}`");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published Standard Webhooks test vector (the reference libraries'
    /// shared fixture; not a credential).
    #[test]
    fn signs_the_standard_webhooks_reference_vector() {
        let key = signing_key("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw").unwrap(); // gitleaks:allow
        assert_eq!(
            sign(
                &key,
                "msg_p5jXN8AQM9LWM0D4loKWxJek", // gitleaks:allow
                1614265330,
                br#"{"test": 2432232314}"#
            ),
            "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE="
        );
    }

    #[test]
    fn a_secret_is_whsec_and_enough_base64() {
        assert!(signing_key("whsec_not base64").is_err());
        assert!(signing_key("whsec_AAAA").is_err());
        let s = new_secret().unwrap();
        assert!(s.starts_with("whsec_"));
        assert_eq!(signing_key(&s).unwrap().len(), 24);
    }

    #[test]
    fn the_secret_is_kept_once_and_reused() {
        let dir = std::env::temp_dir().join(format!("sylphx-listen-secret-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = kept_secret(&dir).unwrap();
        assert_eq!(kept_secret(&dir).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(SECRET_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn forward_takes_a_url_or_a_bare_host() {
        assert_eq!(
            forward_url("http://localhost:3000/hook").unwrap(),
            "http://localhost:3000/hook"
        );
        assert_eq!(
            forward_url("localhost:3000/hook").unwrap(),
            "http://localhost:3000/hook"
        );
        assert!(forward_url("ftp://x").is_err());
    }

    #[test]
    fn the_body_is_the_structured_cloud_event_a_webhook_gets() {
        let event: CloudEvent = serde_json::from_value(json!({
            "id": "e1", "source": "app", "type": "order.created",
            "event_time": "2026-10-06T00:00:00Z", "data": {"n": 1},
            "extensions": {"tenant": "t1"}
        }))
        .unwrap();
        assert_eq!(
            webhook_body(&event),
            json!({"specversion": "1.0", "id": "e1", "source": "app",
                   "type": "order.created", "time": "2026-10-06T00:00:00Z",
                   "data": {"n": 1}, "tenant": "t1"})
        );
    }

    #[test]
    fn run_ids_are_valid_resource_ids() {
        let id = run_id().unwrap();
        assert_eq!(id.len(), "listen-".len() + 10);
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'));
    }
}
