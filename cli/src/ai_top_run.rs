//! `sylphx ai top`: the I/O around `ai_top.rs`. It reads `GET
//! /v1/operator/seats` (scope `ai:operator:seats:read`, a platform key),
//! keeps the weekly-utilisation readings the pace is measured from in
//! `<config dir>/ai-top-history.json` (0600; no other state), and prints the
//! view once or refreshes it until Ctrl-C.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use sylphx::{Client, Error, HttpRequest};

use crate::ai_top::{self, Sample};
use crate::context;

pub enum TopError {
    Api(Error),
    Msg(String),
}

impl From<Error> for TopError {
    fn from(e: Error) -> Self {
        TopError::Api(e)
    }
}

fn history_path() -> Option<PathBuf> {
    context::config_dir().map(|d| d.join("ai-top-history.json"))
}

fn load_history() -> Vec<Sample> {
    history_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .map(|v| ai_top::history_from_json(&v))
        .unwrap_or_default()
}

fn save_history(h: &[Sample]) -> Result<(), String> {
    let path = history_path().ok_or("no home directory: set SYLPHX_CONFIG_DIR")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = ai_top::history_to_json(h).to_string();
    context::write_private(&path, text.as_bytes()).map_err(|e| format!("{}: {e}", path.display()))
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

async fn fetch(client: &Client) -> Result<Vec<ai_top::Seat>, TopError> {
    let v: Value = client
        .call(HttpRequest {
            method: "GET",
            path: "/v1/operator/seats".into(),
            query: vec![],
            body: None,
            mutation: false,
            origin: None,
            effect_ids: false,
        })
        .await?;
    ai_top::parse_seats(&v).map_err(TopError::Msg)
}

/// One refresh: fetch, record, build the view.
async fn refresh(client: &Client, history: &mut Vec<Sample>) -> Result<ai_top::View, TopError> {
    let seats = fetch(client).await?;
    let now = now_secs();
    if ai_top::record(history, &seats, now) {
        if let Err(e) = save_history(history) {
            eprintln!("warning: pace history not saved: {e}");
        }
    }
    Ok(ai_top::build(&seats, history, now))
}

/// `json` prints one JSON document; `once` (or no terminal) prints the text
/// view once; otherwise the screen refreshes every `interval` seconds.
pub async fn run(client: &Client, json: bool, once: bool, interval: u64) -> Result<(), TopError> {
    let mut history = load_history();
    let tty = std::io::stdout().is_terminal();
    if json {
        let view = refresh(client, &mut history).await?;
        println!(
            "{}",
            serde_json::to_string_pretty(&view.to_json()).unwrap_or_default()
        );
        return Ok(());
    }
    let color = tty && std::env::var_os("NO_COLOR").is_none();
    if once || !tty {
        let view = refresh(client, &mut history).await?;
        print!("{}", view.render(color));
        return Ok(());
    }
    loop {
        let screen = match refresh(client, &mut history).await {
            Ok(view) => view.render(color),
            Err(TopError::Api(e)) => format!("fetch failed, retrying: {e}\n"),
            Err(TopError::Msg(m)) => format!("fetch failed, retrying: {m}\n"),
        };
        let mut out = std::io::stdout();
        let _ = write!(
            out,
            "\x1b[H\x1b[2J{screen}\n(refresh every {interval}s, Ctrl-C to quit)\n"
        );
        let _ = out.flush();
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            _ = tokio::signal::ctrl_c() => return Ok(()),
        }
    }
}
