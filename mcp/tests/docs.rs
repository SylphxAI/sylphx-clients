//! The docs connector end to end over an in-memory Search API: a sync
//! writes heading chunks and deletes stale ones, and the MCP docs tools
//! answer path:line hits and line ranges.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::{json, Value};
use sylphx::{Client, Error, HttpRequest, HttpResponse, Transport};
use sylphx_mcp::docs::{self, b64_decode, b64_encode, DocsIndex};
use sylphx_mcp::Server;

/// Search documents by id, with the Search API's filters (`eq`, `ne`) and a
/// plain text match (every query word's first four letters).
#[derive(Default)]
struct FakeSearch {
    docs: Mutex<BTreeMap<String, Value>>,
}

fn unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn field<'a>(body: &'a Value, snake: &str, camel: &str) -> &'a Value {
    if body[snake].is_null() {
        &body[camel]
    } else {
        &body[snake]
    }
}

impl FakeSearch {
    fn search(&self, body: &Value) -> Value {
        let query = body["query"].as_str().unwrap_or_default().to_lowercase();
        let words: Vec<String> = query
            .split_whitespace()
            .map(|w| w.chars().filter(|c| c.is_alphanumeric()).take(4).collect())
            .collect();
        let limit = body["limit"].as_u64().unwrap_or(20) as usize;
        let offset = body["offset"].as_u64().unwrap_or(0) as usize;
        let filters = body["filters"].as_array().cloned().unwrap_or_default();
        let docs = self.docs.lock().unwrap();
        let mut hits: Vec<(f64, &String, &Value)> = docs
            .iter()
            .filter(|(_, d)| {
                filters.iter().all(|f| {
                    let got = &d[f["field"].as_str().unwrap()];
                    let want = || -> Value {
                        serde_json::from_str(field(f, "value_json", "valueJson").as_str().unwrap())
                            .unwrap()
                    };
                    // As Data compiles them: `ne` is NOT containment, so a
                    // document without the field matches it.
                    match f["op"].as_str().unwrap() {
                        "exists" => !got.is_null(),
                        "eq" => *got == want(),
                        "ne" => *got != want(),
                        op => panic!("unexpected op {op}"),
                    }
                })
            })
            .filter_map(|(id, d)| {
                if words.is_empty() {
                    return Some((0.0, id, d));
                }
                let hay = d.to_string().to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str())).then(|| {
                    (
                        words
                            .iter()
                            .map(|w| hay.matches(w.as_str()).count())
                            .sum::<usize>() as f64,
                        id,
                        d,
                    )
                })
            })
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(b.1)));
        let hits: Vec<Value> = hits
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(score, id, d)| {
                json!({"document": {"document_id": id, "document_json": b64_encode(d.to_string().as_bytes()), "version": "1"}, "score": score})
            })
            .collect();
        json!({ "hits": hits })
    }
}

impl Transport for FakeSearch {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, Error> {
        let parts: Vec<&str> = request.path.trim_start_matches('/').split('/').collect();
        let body = request.body.clone().unwrap_or(Value::Null);
        let reply = match (request.method, parts.as_slice()) {
            ("PUT", ["v1", "documents", _index, id]) => {
                let raw = field(&body, "document_json", "documentJson")
                    .as_str()
                    .unwrap();
                let doc: Value = serde_json::from_slice(&b64_decode(raw).unwrap()).unwrap();
                self.docs.lock().unwrap().insert(unescape(id), doc);
                json!({"document": {"document_id": unescape(id)}})
            }
            ("DELETE", ["v1", "documents", _index, id]) => {
                let existed = self.docs.lock().unwrap().remove(&unescape(id)).is_some();
                json!({ "deleted": existed })
            }
            ("POST", ["v1", "search", _index]) => self.search(&body),
            other => panic!("unexpected request {other:?}"),
        };
        Ok(HttpResponse {
            status: 200,
            request_id: Some("req_t".into()),
            body: serde_json::to_vec(&reply).unwrap(),
        })
    }
}

const RISK: &str = "# Risk\n\nEverything is scored.\n\n## Scoring\n\nScore each risk as likelihood x impact against the monthly running cost.\nRisk scoring uses the 1-25 grid.\n\n## Records\n\nRecord accepted risks.\n";
const AGENTS: &str = "# Company\n\nRead the standards.\n";
const OLD: &str = "# Old\n\nGone soon.\n";

fn index() -> DocsIndex {
    DocsIndex {
        index: "law".into(),
        source: None,
    }
}

fn files(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect()
}

#[tokio::test]
async fn sync_writes_chunks_and_deletes_what_is_gone() {
    let client = Client::new(FakeSearch::default());
    let first = files(&[
        ("AGENTS.md", AGENTS),
        ("standards/risk.md", RISK),
        ("old.md", OLD),
    ]);
    let r = docs::sync(&client, &index(), "rev1", &first, false)
        .await
        .unwrap();
    assert_eq!((r.files, r.chunks, r.written, r.deleted), (3, 5, 5, 0));

    let second = files(&[("AGENTS.md", AGENTS), ("standards/risk.md", RISK)]);
    let r = docs::sync(&client, &index(), "rev2", &second, false)
        .await
        .unwrap();
    assert_eq!((r.written, r.deleted), (4, 1));
    let read = docs::read(&client, &index(), "old.md", None, None).await;
    assert!(read.is_err(), "the removed file's chunk is deleted");

    // A pattern that matches nothing is refused and deletes nothing.
    assert!(docs::sync(&client, &index(), "rev3", &[], false)
        .await
        .is_err());
    let r = docs::sync(&client, &index(), "rev3", &second, true)
        .await
        .unwrap();
    assert_eq!((r.chunks, r.written, r.deleted), (4, 0, 0));
    assert!(docs::read(&client, &index(), "AGENTS.md", None, None)
        .await
        .is_ok());
}

#[tokio::test]
async fn sync_deletes_only_its_own_sections_of_its_own_source() {
    let fake = FakeSearch::default();
    {
        let mut d = fake.docs.lock().unwrap();
        // A document some other writer keeps in the same index.
        d.insert(
            "order-1".into(),
            json!({"title": "an order", "path": "x.md"}),
        );
        // Another source's section.
        d.insert(
            "other:a.md:L1".into(),
            json!({"kind": "docs-section", "source": "other", "revision": "r0", "path": "a.md", "start_line": 1, "text": "# A"}),
        );
    }
    let client = Client::new(fake);
    let one = files(&[("AGENTS.md", AGENTS)]);
    // Without a source, then with one: neither touches the foreign document
    // or the other source's section.
    let r = docs::sync(&client, &index(), "rev1", &one, false)
        .await
        .unwrap();
    assert_eq!(r.deleted, 0);
    let mut named = index();
    named.source = Some("mine".into());
    docs::sync(&client, &named, "rev1", &one, false)
        .await
        .unwrap();
    let r = docs::sync(&client, &named, "rev2", &files(&[("b.md", OLD)]), true)
        .await
        .unwrap();
    assert_eq!(
        (r.written, r.stale),
        (0, 1),
        "a dry run counts what it would delete"
    );
    let r = docs::sync(&client, &named, "rev2", &files(&[("b.md", OLD)]), false)
        .await
        .unwrap();
    assert_eq!(r.deleted, 1, "only its own stale section");
    let r = docs::sync(&client, &index(), "rev2", &files(&[("b.md", OLD)]), false)
        .await
        .unwrap();
    assert_eq!(r.deleted, 1, "only the unnamed source's stale section");
    // Readers see only the connector's sections.
    assert!(docs::read(&client, &index(), "x.md", None, None)
        .await
        .is_err());
    let left = client.transport();
    let d = left.docs.lock().unwrap();
    assert!(d.contains_key("order-1"));
    assert!(d.contains_key("other:a.md:L1"));
    assert!(d.contains_key("b.md:L1") && d.contains_key("mine:b.md:L1"));
    assert_eq!(d.len(), 4);
}

async fn synced_server(with_docs: bool) -> Server<FakeSearch> {
    let client = Client::new(FakeSearch::default());
    let all = files(&[("AGENTS.md", AGENTS), ("standards/risk.md", RISK)]);
    docs::sync(&client, &index(), "rev1", &all, false)
        .await
        .unwrap();
    Server::new(Some(client)).with_docs(with_docs.then(index))
}

async fn call_tool(s: &Server<FakeSearch>, name: &str, args: Value) -> Value {
    let r = s
        .handle(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}))
        .await
        .unwrap();
    r["result"].clone()
}

fn tool_names(r: &Value) -> Vec<String> {
    r["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn docs_tools_are_listed_only_with_an_index() {
    let list = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let with = synced_server(true)
        .await
        .handle(list.clone())
        .await
        .unwrap();
    let names = tool_names(&with);
    assert!(names.contains(&"docs_search".to_string()) && names.contains(&"docs_read".to_string()));
    let tool = with["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "docs_search")
        .unwrap()
        .clone();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);

    let without = synced_server(false).await;
    assert!(!tool_names(&without.handle(list).await.unwrap()).contains(&"docs_search".to_string()));
    let r = call_tool(&without, "docs_search", json!({"query": "risk"})).await;
    assert_eq!(r["isError"], true);
    assert!(r["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("SYLPHX_DOCS_INDEX"));
}

#[tokio::test]
async fn docs_search_answers_the_page_and_line() {
    let s = synced_server(true).await;
    let r = call_tool(&s, "docs_search", json!({"query": "risk scoring"})).await;
    assert_eq!(r["isError"], false, "{r}");
    let top = &r["structuredContent"]["hits"][0];
    assert_eq!(top["path"], "standards/risk.md");
    assert_eq!(top["heading"], "Risk > Scoring");
    assert_eq!(top["ref"], "standards/risk.md:8");
    assert_eq!(top["snippet"], "Risk scoring uses the 1-25 grid.");
}

#[tokio::test]
async fn docs_read_returns_a_file_or_its_lines() {
    let s = synced_server(true).await;
    let r = call_tool(&s, "docs_read", json!({"path": "standards/risk.md"})).await;
    let v = &r["structuredContent"];
    assert_eq!(v["total_lines"], 12);
    assert_eq!(v["truncated"], false);
    assert_eq!(format!("{}\n", v["text"].as_str().unwrap()), RISK);

    let r = call_tool(
        &s,
        "docs_read",
        json!({"path": "standards/risk.md", "start_line": 7, "end_line": 8}),
    )
    .await;
    assert_eq!(
        r["structuredContent"]["text"],
        "Score each risk as likelihood x impact against the monthly running cost.\nRisk scoring uses the 1-25 grid."
    );

    let r = call_tool(&s, "docs_read", json!({"path": "nope.md"})).await;
    assert_eq!(r["isError"], true);
}
