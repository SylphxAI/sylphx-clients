//! Cold workspace materialization: Namespace-style in-cell reuse, REAPI
//! FindMissingBlobs, and content-addressed gzip/pax layers. A versioned tree
//! manifest is the packing action; /ac stores its ActionResult, /cas its bytes.
//! Cache failures fall back to direct tarballs; no tenant-specific path exists.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::{stream, StreamExt};
use prost::Message;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sylphx_build_lease::guest::{Fault, Guest};

use super::{pack_guest, packs, sync};
use sync::Tree;

const PARALLEL: usize = 16;
const FIND_CHUNK: usize = 4096;
pub const TOKEN_ENV: &str = "SYLPHX_CAS_TOKEN";

pub struct Store {
    http: reqwest::Client,
    public: String,
    cluster: String,
    instance: String,
    token: String,
}

// Wire subset of remote_execution.proto: ActionResult.stdout_digest (6),
// Digest.hash (1), Digest.size_bytes (2). Other fields are safely ignored.
#[derive(Clone, PartialEq, Message)]
struct ActionResult {
    #[prost(message, optional, tag = "6")]
    stdout_digest: Option<Digest>,
}
#[derive(Clone, PartialEq, Message)]
struct Digest {
    #[prost(string, tag = "1")]
    hash: String,
    #[prost(int64, tag = "2")]
    size_bytes: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Ref {
    digest: String,
    size: u64,
}
fn hash_ok(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
impl Ref {
    fn action(&self) -> Vec<u8> {
        ActionResult {
            stdout_digest: Some(Digest {
                hash: self.digest.clone(),
                size_bytes: self.size as i64,
            }),
        }
        .encode_to_vec()
    }
    fn parse(bytes: &[u8]) -> Option<Self> {
        let d = ActionResult::decode(bytes).ok()?.stdout_digest?;
        (hash_ok(&d.hash) && d.size_bytes > 0).then_some(Self {
            digest: d.hash,
            size: d.size_bytes as u64,
        })
    }
}

impl Store {
    pub fn new(public: &str, cluster: &str, instance: &str, token: &str) -> Option<Self> {
        let token_ok = !token.is_empty()
            && token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-._~+/=".contains(&b));
        let cluster = cluster.trim().trim_end_matches('/');
        let cluster_ok = (cluster.starts_with("http://") || cluster.starts_with("https://"))
            && !cluster.contains(['"', '\\', ' ', '\n']);
        if !token_ok || !cluster_ok || public.is_empty() {
            return None;
        }
        Some(Self {
            http: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(300))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .ok()?,
            public: public.trim_end_matches('/').into(),
            cluster: cluster.into(),
            instance: instance.trim_matches('/').into(),
            token: token.into(),
        })
    }
    pub fn instance_key(&self) -> String {
        format!("{} {}", self.public, self.instance)
    }
    fn v2(&self, route: &str) -> String {
        if self.instance.is_empty() {
            format!("{}/v2/{route}", self.public)
        } else {
            format!("{}/v2/{}/{route}", self.public, self.instance)
        }
    }
    async fn lookup(&self, key: &str) -> Result<Option<Ref>, String> {
        let response = self
            .http
            .get(format!("{}/ac/{key}", self.public))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(unreachable_text)?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        check_status(response.status())?;
        if response.content_length().is_some_and(|n| n > 1024) {
            return Ok(None);
        }
        let bytes = response.bytes().await.map_err(unreachable_text)?;
        Ok(Ref::parse(&bytes))
    }
    async fn put(&self, route: &str, body: Vec<u8>) -> Result<(), String> {
        let response = self
            .http
            .put(format!("{}/{route}", self.public))
            .bearer_auth(&self.token)
            .header("content-type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(unreachable_text)?;
        check_status(response.status())?;
        if response
            .headers()
            .get("x-build-cache")
            .is_some_and(|v| v == "dropped")
        {
            return Err("the build store dropped the upload".into());
        }
        Ok(())
    }
    async fn find_missing(
        &self,
        want: &BTreeMap<String, u64>,
        sent: &mut u64,
    ) -> Result<BTreeSet<String>, String> {
        let all: Vec<_> = want.iter().collect();
        let mut missing = BTreeSet::new();
        for chunk in all.chunks(FIND_CHUNK) {
            let digests: Vec<_> = chunk
                .iter()
                .map(|(h, s)| json!({"hash":h, "sizeBytes":s.to_string()}))
                .collect();
            let body =
                serde_json::to_vec(&json!({"blobDigests":digests})).map_err(|e| e.to_string())?;
            *sent += body.len() as u64;
            let response = self
                .http
                .post(self.v2("blobs:findMissing"))
                .bearer_auth(&self.token)
                .header("content-type", "application/json")
                .body(body)
                .send()
                .await
                .map_err(unreachable_text)?;
            check_status(response.status())?;
            let v: Value = response.json().await.map_err(unreachable_text)?;
            if !v.is_object() {
                return Err("the build store's reply is unreadable".into());
            }
            if let Some(list) = v.get("missingBlobDigests") {
                for item in list
                    .as_array()
                    .ok_or("the build store's reply is unreadable")?
                {
                    let hash = item["hash"]
                        .as_str()
                        .ok_or("the build store's reply is unreadable")?;
                    if !want.contains_key(hash) {
                        return Err("the build store returned an unrequested digest".into());
                    }
                    missing.insert(hash.into());
                }
            }
        }
        Ok(missing)
    }
}
fn check_status(status: reqwest::StatusCode) -> Result<(), String> {
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("the build store answered HTTP {}", status.as_u16()))
    }
}
fn unreachable_text(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "the build store timed out".into()
    } else {
        "the build store is unreachable".into()
    }
}

#[derive(Clone, Copy)]
pub struct Local<'a> {
    pub root: &'a Path,
    pub tree: &'a Tree,
}
#[derive(Debug, Default)]
pub struct Filled {
    pub paths: BTreeSet<String>,
    pub sent: u64,
    pub warning: Option<String>,
}

/// Local acceleration only. CAS presence is checked each run; cached actions
/// and store actions are both verified against file contents on the guest.
#[derive(Serialize, Deserialize)]
struct Index {
    instance: String,
    entries: BTreeMap<String, Ref>,
}
pub struct Known {
    path: Option<PathBuf>,
    index: Index,
}
impl Known {
    pub fn load(path: Option<PathBuf>, key: &str) -> Self {
        let index = path
            .as_ref()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice::<Index>(&b).ok())
            .filter(|i| i.instance == key)
            .unwrap_or_else(|| Index {
                instance: key.into(),
                entries: BTreeMap::new(),
            });
        Self { path, index }
    }
    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        let Ok(body) = serde_json::to_vec(&self.index) else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension(format!("{}.tmp", sylphx_build_lease::guest::rand_u64()));
        if std::fs::write(&tmp, body).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
        let _ = std::fs::remove_file(tmp);
    }
}

/// Upload missing pack bytes, not a desk-side full-tree archive. Action-cache
/// reads/uploads are bounded parallel work; a warm client never rebuilds packs.
pub async fn fill(
    g: &Guest,
    store: &Store,
    known: &mut Known,
    local: Local<'_>,
    plan: &sync::Plan,
    ws: &str,
    user: &str,
) -> Result<Filled, Fault> {
    let mut f = Filled::default();
    let packs = packs::plan(local.tree, &plan.upload);
    let live: BTreeSet<&str> = packs.iter().map(|p| p.key.as_str()).collect();
    known
        .index
        .entries
        .retain(|k, r| live.contains(k.as_str()) && hash_ok(&r.digest) && r.size > 0);
    let unknown: Vec<_> = packs
        .iter()
        .filter(|p| !known.index.entries.contains_key(&p.key))
        .collect();
    let lookups: Vec<_> = stream::iter(
        unknown
            .into_iter()
            .map(|p| async move { (p, store.lookup(&p.key).await) }),
    )
    .buffer_unordered(PARALLEL)
    .collect()
    .await;
    for (p, result) in lookups {
        match result {
            Ok(Some(r)) => {
                known.index.entries.insert(p.key.clone(), r);
            }
            Ok(None) => {}
            Err(e) => {
                f.warning = Some(e);
                return Ok(f);
            }
        }
    }
    let want: BTreeMap<_, _> = known
        .index
        .entries
        .values()
        .map(|r| (r.digest.clone(), r.size))
        .collect();
    let missing = match store.find_missing(&want, &mut f.sent).await {
        Ok(m) => m,
        Err(e) => {
            f.warning = Some(e);
            return Ok(f);
        }
    };
    let mut uploads = Vec::new();
    let mut ready = Vec::new();
    for p in packs {
        let held = known
            .index
            .entries
            .get(&p.key)
            .filter(|r| !missing.contains(&r.digest));
        if let Some(r) = held {
            ready.push((p.key, r.digest.clone(), p.paths));
            continue;
        }
        let bytes = match packs::encode(local.root, local.tree, &p) {
            Ok(b) => b,
            Err(e) => {
                f.warning = Some(e);
                known.index.entries.remove(&p.key);
                continue;
            }
        };
        let r = Ref {
            digest: sync::sha256_hex(&bytes),
            size: bytes.len() as u64,
        };
        uploads.push((p, r, bytes));
    }
    let rebuilt: BTreeMap<_, _> = uploads
        .iter()
        .map(|(_, r, _)| (r.digest.clone(), r.size))
        .collect();
    let absent = match store.find_missing(&rebuilt, &mut f.sent).await {
        Ok(m) => m,
        Err(e) => {
            f.warning = Some(e);
            return Ok(f);
        }
    };
    let done: Vec<_> = stream::iter(uploads.into_iter().map(|(p, r, bytes)| {
        let upload = absent.contains(&r.digest);
        async move {
            let action = r.action();
            let mut sent = 0;
            let result = async {
                if upload {
                    sent += bytes.len() as u64;
                    store.put(&format!("cas/{}", r.digest), bytes).await?;
                }
                sent += action.len() as u64;
                store.put(&format!("ac/{}", p.key), action).await
            }
            .await;
            (p, r, sent, result)
        }
    }))
    .buffer_unordered(PARALLEL)
    .collect()
    .await;
    for (p, r, sent, result) in done {
        f.sent += sent;
        match result {
            Ok(()) => {
                ready.push((p.key.clone(), r.digest.clone(), p.paths));
                known.index.entries.insert(p.key, r);
            }
            Err(e) => {
                f.warning = Some(e);
                known.index.entries.remove(&p.key);
            }
        }
    }
    if ready.is_empty() {
        return Ok(f);
    }
    ready.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let list = gzip(&pack_guest::list(local.tree, &ready));
    f.sent += list.len() as u64;
    g.upload(&format!("{ws}/.sylphx/fill.gz"), user, &list)
        .await?;
    let env = [(TOKEN_ENV.into(), store.token.clone())].into();
    let args = [
        "/bin/sh",
        "-c",
        pack_guest::FILL,
        "sylphx-pack-fill",
        ws,
        &store.cluster,
    ]
    .map(String::from)
    .to_vec();
    let (code, out, _) = g.run(user, &args, &env).await?;
    if code != 0 {
        f.warning = Some(format!(
            "the build machine could not fill packs (exit {code})"
        ));
        return Ok(f);
    }
    let keys: Vec<_> = ready.iter().map(|p| p.0.clone()).collect();
    let Some(failed) = pack_guest::failed(&out, &keys) else {
        f.warning = Some("the build machine's pack answer is unreadable".into());
        return Ok(f);
    };
    for (key, _, paths) in ready {
        if failed.contains(&key) {
            known.index.entries.remove(&key);
        } else {
            f.paths.extend(paths);
        }
    }
    if !failed.is_empty() {
        f.warning = Some(format!(
            "{} packs could not be fetched or verified",
            failed.len()
        ));
    }
    Ok(f)
}

pub fn gzip(text: &str) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    // Writing to Vec cannot fail.
    gz.write_all(text.as_bytes()).expect("gzip Vec write");
    gz.finish().expect("gzip Vec finish")
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[derive(Default)]
    struct Cache {
        blobs: BTreeMap<String, Vec<u8>>,
        actions: BTreeMap<String, Vec<u8>>,
        uploads: Vec<String>,
        guest_fail: String,
        down: bool,
    }
    async fn serve() -> (String, Arc<Mutex<Cache>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let cache = Arc::new(Mutex::new(Cache::default()));
        let state = cache.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut conn, _)) = listener.accept().await else {
                    return;
                };
                let state = state.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    loop {
                        let end = loop {
                            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                break i + 4;
                            }
                            let mut chunk = [0; 8192];
                            match conn.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let mut words = head.split_whitespace();
                        let method = words.next().unwrap();
                        let path = words.next().unwrap().to_string();
                        let len: usize = head
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse().ok())?
                            })
                            .unwrap_or(0);
                        while buf.len() < end + len {
                            let mut chunk = [0; 8192];
                            match conn.read(&mut chunk).await {
                                Ok(0) | Err(_) => return,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let body: Vec<u8> = buf.drain(..end + len).skip(end).collect();
                        let (status, ctype, out) = {
                            let mut c = state.lock().unwrap();
                            if c.down && (path.starts_with("/ac/") || path.starts_with("/v2/")) {
                                (503, "text/plain", Vec::new())
                            } else if path.starts_with("/ac/") && method == "GET" {
                                match c.actions.get(&path[4..]) {
                                    Some(b) => (200, "application/octet-stream", b.clone()),
                                    None => (404, "text/plain", Vec::new()),
                                }
                            } else if path.starts_with("/cas/") && method == "PUT" {
                                let digest = path[5..].to_string();
                                assert_eq!(digest, sync::sha256_hex(&body));
                                c.blobs.insert(digest.clone(), body);
                                c.uploads.push(digest);
                                (200, "text/plain", Vec::new())
                            } else if path.starts_with("/ac/") && method == "PUT" {
                                assert!(Ref::parse(&body).is_some());
                                c.actions.insert(path[4..].into(), body);
                                (200, "text/plain", Vec::new())
                            } else if path.ends_with("blobs:findMissing") {
                                let v: Value = serde_json::from_slice(&body).unwrap();
                                let missing: Vec<_> = v["blobDigests"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .filter(|d| !c.blobs.contains_key(d["hash"].as_str().unwrap()))
                                    .cloned()
                                    .collect();
                                (
                                    200,
                                    "application/json",
                                    serde_json::to_vec(&json!({"missingBlobDigests":missing}))
                                        .unwrap(),
                                )
                            } else if path.ends_with("process.Process/Start") {
                                let mut out = sylphx_build_lease::guest::envelope(
                                    &json!({"event":{"start":{"pid":7}}}),
                                );
                                if !c.guest_fail.is_empty() {
                                    out.extend(sylphx_build_lease::guest::envelope(&json!({"event":{"data":{"stdout":STANDARD.encode(&c.guest_fail)}}})));
                                }
                                out.extend(sylphx_build_lease::guest::envelope(
                                    &json!({"event":{"end":{"exitCode":0,"exited":true}}}),
                                ));
                                let mut end = sylphx_build_lease::guest::envelope(&json!({}));
                                end[0] = 2;
                                out.extend(end);
                                (200, "application/connect+json", out)
                            } else if path.starts_with("/files") {
                                (200, "application/json", b"[]".to_vec())
                            } else {
                                (404, "text/plain", Vec::new())
                            }
                        };
                        let h=format!("HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n\r\n",out.len());
                        if conn.write_all(h.as_bytes()).await.is_err()
                            || conn.write_all(&out).await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        (base, cache)
    }
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "sylphx-pack-store-{}-{:x}",
                std::process::id(),
                sylphx_build_lease::guest::rand_u64()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
        fn tree(&self) -> Tree {
            sync::scan(
                &self.0,
                &sync::file_set(&self.0).unwrap(),
                &mut sync::StatCache::default(),
            )
            .unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    async fn materialize(st: &Store, g: &Guest, k: &mut Known, root: &Path, tree: &Tree) -> Filled {
        fill(
            g,
            st,
            k,
            Local { root, tree },
            &sync::plan(tree, None),
            "/workspace",
            "user",
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn cold_workspaces_upload_only_changed_packs_and_recover_eviction() {
        let root = Scratch::new();
        for i in 0..512 {
            std::fs::write(root.0.join(format!("file-{i:04}")), format!("contents-{i}")).unwrap();
        }
        let tree = root.tree();
        let (base, state) = serve().await;
        let st = Store::new(&base, &base, "cache", "tok").unwrap();
        let g = Guest::new(&base, "sandbox", "lease").unwrap();
        let mut known = Known::load(None, &st.instance_key());
        let first = materialize(&st, &g, &mut known, &root.0, &tree).await;
        assert_eq!(first.warning, None);
        assert_eq!(first.paths, tree.keys().cloned().collect());
        let initial_uploads = state.lock().unwrap().uploads.len();
        let second = materialize(&st, &g, &mut known, &root.0, &tree).await;
        assert_eq!(second.paths, first.paths);
        assert!(second.sent < 1 << 20);
        assert_eq!(state.lock().unwrap().uploads.len(), initial_uploads);
        // A different client reuses the org's AC without rebuilding archives.
        let mut fresh = Known::load(None, &st.instance_key());
        assert_eq!(
            materialize(&st, &g, &mut fresh, &root.0, &tree).await.paths,
            first.paths
        );
        assert_eq!(state.lock().unwrap().uploads.len(), initial_uploads);
        // An evicted action mapping does not justify re-uploading CAS bytes.
        state.lock().unwrap().actions.clear();
        let mut no_actions = Known::load(None, &st.instance_key());
        let replay = materialize(&st, &g, &mut no_actions, &root.0, &tree).await;
        assert_eq!(replay.paths, first.paths);
        assert_eq!(state.lock().unwrap().uploads.len(), initial_uploads);
        std::fs::write(root.0.join("file-0250"), "edited").unwrap();
        let edited = root.tree();
        let third = materialize(&st, &g, &mut known, &root.0, &edited).await;
        assert_eq!(third.paths, first.paths);
        assert!(third.sent < 5 << 20);
        assert_eq!(state.lock().unwrap().uploads.len(), initial_uploads + 1);
        let missing = known.index.entries.values().next().unwrap().digest.clone();
        state.lock().unwrap().blobs.remove(&missing);
        let fourth = materialize(&st, &g, &mut known, &root.0, &edited).await;
        assert_eq!(fourth.paths, first.paths);
        assert!(state.lock().unwrap().blobs.contains_key(&missing));
    }
    #[tokio::test]
    async fn cache_failure_and_guest_bad_reply_leave_all_paths_for_direct_transfer() {
        let root = Scratch::new();
        std::fs::write(root.0.join("a"), "a").unwrap();
        let tree = root.tree();
        let (base, state) = serve().await;
        let st = Store::new(&base, &base, "cache", "tok").unwrap();
        let g = Guest::new(&base, "sandbox", "lease").unwrap();
        let mut known = Known::load(None, &st.instance_key());
        state.lock().unwrap().down = true;
        let f = materialize(&st, &g, &mut known, &root.0, &tree).await;
        assert!(f.paths.is_empty());
        assert!(f.warning.unwrap().contains("503"));
        state.lock().unwrap().down = false;
        state.lock().unwrap().guest_fail = "unrequested-pack\n".into();
        let f = materialize(&st, &g, &mut known, &root.0, &tree).await;
        assert!(f.paths.is_empty());
        assert!(f.warning.unwrap().contains("unreadable"));
    }
    #[test]
    fn local_index_is_scoped_and_rejects_invalid_references() {
        let root = Scratch::new();
        let path = root.0.join("index");
        let mut k = Known::load(Some(path.clone()), "one");
        k.index.entries.insert(
            sync::sha256_hex(b"manifest"),
            Ref {
                digest: sync::sha256_hex(b"pack"),
                size: 5,
            },
        );
        k.save();
        assert_eq!(
            Known::load(Some(path.clone()), "one").index.entries.len(),
            1
        );
        assert!(Known::load(Some(path), "other").index.entries.is_empty());
        assert!(Ref::parse(
            &ActionResult {
                stdout_digest: Some(Digest {
                    hash: "wrong".into(),
                    size_bytes: 1
                })
            }
            .encode_to_vec()
        )
        .is_none());
        assert!(Ref::parse(
            &ActionResult {
                stdout_digest: Some(Digest {
                    hash: sync::sha256_hex(b"pack"),
                    size_bytes: -1
                })
            }
            .encode_to_vec()
        )
        .is_none());
    }
}
