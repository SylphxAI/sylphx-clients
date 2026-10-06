//! Content-addressed upload of `sylphx build run` (the remote build execution
//! ADR, decision 2, step 5): of the files a sync sends, the client uploads
//! only the blobs the org's build store lacks, and the build machine fills
//! the workspace from the store inside the Cell.
//!
//! * The client asks the store which digests it lacks
//!   (`POST {public}/v2/{instance}/blobs:findMissing`, REAPI over HTTP/JSON)
//!   and uploads those (`blobs:batchUpdate` in batches of at most
//!   [`BATCH_RAW`] bytes of content, a larger file as `PUT /cas/{sha256}`).
//!   Digests the store had or took are remembered per cache in the git dir
//!   ([`Known`]), so a later run asks only about new ones.
//! * The guest then fetches every listed file from `{cluster}/cas/{sha256}`
//!   with `curl --parallel`, checks each digest and reports the files it
//!   could not fill ([`FILL`]); the client sends those in the tarball.
//!
//! The token is the run's build cache token. It travels to the guest in the
//! fill process's environment, is written there only into a 0600 config file
//! that curl reads, and is never in an argument list, an event or a log.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde_json::{json, Value};

use super::sync::{self, Mode, Tree};
use sylphx_build_lease::guest::{Fault, Guest};

/// Content bytes per `batchUpdate`; base64 keeps the body near 4 MiB. A file
/// larger than this goes alone, as a `PUT`.
pub const BATCH_RAW: u64 = 3 << 20;
/// Digests per `findMissing` request.
const FIND_CHUNK: usize = 4096;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// The variable that carries the token into the fill process.
pub const TOKEN_ENV: &str = "SYLPHX_CAS_TOKEN";
/// sha256 of nothing: an empty file is sent in the tarball, never fetched.
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// One file to put in the store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub path: String,
    pub digest: String,
    pub size: u64,
}

/// The org's build store, as the run's cache token reaches it.
pub struct Store {
    http: reqwest::Client,
    /// The gateway as this machine reaches it.
    public: String,
    /// The gateway as the build machine reaches it.
    cluster: String,
    instance: String,
    token: String,
}

impl Store {
    /// `None` when the token cannot travel safely in a curl config line.
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
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        Some(Self {
            http,
            public: public.trim_end_matches('/').to_string(),
            cluster: cluster.to_string(),
            instance: instance.trim_matches('/').to_string(),
            token: token.to_string(),
        })
    }

    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Names the store the [`Known`] digests are for.
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

    async fn post(&self, url: String, body: Vec<u8>) -> Result<Value, String> {
        let resp = self
            .http
            .post(url)
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| unreachable_text(&e))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| unreachable_text(&e))?;
        if !status.is_success() {
            return Err(format!("the build store answered HTTP {}", status.as_u16()));
        }
        serde_json::from_slice(&bytes).map_err(|_| "the build store's reply is unreadable".into())
    }

    /// The digests of `want` the store lacks, and the bytes sent asking.
    pub async fn find_missing(
        &self,
        want: &BTreeMap<String, u64>,
    ) -> Result<(BTreeSet<String>, u64), String> {
        let all: Vec<(&String, &u64)> = want.iter().collect();
        let (mut missing, mut sent) = (BTreeSet::new(), 0u64);
        for chunk in all.chunks(FIND_CHUNK) {
            let body = find_missing_body(chunk.iter().map(|(d, s)| (d.as_str(), **s)));
            sent += body.len() as u64;
            let v = self.post(self.v2("blobs:findMissing"), body).await?;
            missing.extend(parse_missing(&v, want)?);
        }
        Ok((missing, sent))
    }

    /// Uploads `blobs` from `root`, answering the bytes sent and the paths
    /// that no longer match their digest (changed since they were hashed;
    /// they are left to the tarball).
    pub async fn upload(
        &self,
        root: &Path,
        blobs: &[Blob],
    ) -> Result<(u64, BTreeSet<String>), String> {
        let (batches, big) = batches(blobs, BATCH_RAW);
        let (mut sent, mut changed) = (0u64, BTreeSet::new());
        for batch in batches {
            let mut items = Vec::new();
            for i in batch {
                let b = &blobs[i];
                match read_checked(root, b) {
                    Some(bytes) => items.push((b, bytes)),
                    None => {
                        changed.insert(b.path.clone());
                    }
                }
            }
            if items.is_empty() {
                continue;
            }
            let body = batch_body(
                items
                    .iter()
                    .map(|(b, bytes)| (b.digest.as_str(), &bytes[..])),
            );
            sent += body.len() as u64;
            let v = self.post(self.v2("blobs:batchUpdate"), body).await?;
            check_batch(&v, items.len())?;
        }
        for i in big {
            let b = &blobs[i];
            let Some(bytes) = read_checked(root, b) else {
                changed.insert(b.path.clone());
                continue;
            };
            sent += bytes.len() as u64;
            let resp = self
                .http
                .put(format!("{}/cas/{}", self.public, b.digest))
                .bearer_auth(&self.token)
                .header("content-type", "application/octet-stream")
                .body(bytes)
                .send()
                .await
                .map_err(|e| unreachable_text(&e))?;
            if !resp.status().is_success() {
                return Err(format!(
                    "the build store answered HTTP {}",
                    resp.status().as_u16()
                ));
            }
        }
        Ok((sent, changed))
    }
}

fn unreachable_text(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "the build store timed out".into()
    } else {
        "the build store is unreachable".into()
    }
}

/// The file's bytes, if they still hash to its digest.
fn read_checked(root: &Path, b: &Blob) -> Option<Vec<u8>> {
    let bytes = std::fs::read(root.join(&b.path)).ok()?;
    (sync::sha256_hex(&bytes) == b.digest).then_some(bytes)
}

/// `{"blobDigests": [{"hash", "sizeBytes"}]}`; proto3 JSON writes int64 as a
/// string.
pub fn find_missing_body<'a>(digests: impl Iterator<Item = (&'a str, u64)>) -> Vec<u8> {
    let list: Vec<Value> = digests
        .map(|(h, s)| json!({"hash": h, "sizeBytes": s.to_string()}))
        .collect();
    serde_json::to_vec(&json!({ "blobDigests": list })).unwrap_or_default()
}

/// The `missingBlobDigests` of a reply, each one that was asked about.
pub fn parse_missing(v: &Value, asked: &BTreeMap<String, u64>) -> Result<Vec<String>, String> {
    let Some(list) = v.get("missingBlobDigests") else {
        // proto3 JSON leaves an empty list out.
        return if v.is_object() {
            Ok(Vec::new())
        } else {
            Err("the build store's reply is unreadable".into())
        };
    };
    let list = list
        .as_array()
        .ok_or("the build store's reply is unreadable")?;
    let mut out = Vec::new();
    for d in list {
        let h = d["hash"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .ok_or("the build store's reply is unreadable")?;
        if !asked.contains_key(&h) {
            return Err("the build store answered a digest it was not asked about".into());
        }
        out.push(h);
    }
    Ok(out)
}

/// `{"requests": [{"digest", "data"}]}` with the content in base64.
pub fn batch_body<'a>(items: impl Iterator<Item = (&'a str, &'a [u8])>) -> Vec<u8> {
    let list: Vec<Value> = items
        .map(|(h, b)| {
            json!({"digest": {"hash": h, "sizeBytes": b.len().to_string()}, "data": B64.encode(b)})
        })
        .collect();
    serde_json::to_vec(&json!({ "requests": list })).unwrap_or_default()
}

/// Every response of a `batchUpdate` reply is OK (status code 0 or absent).
fn check_batch(v: &Value, n: usize) -> Result<(), String> {
    let list = v["responses"]
        .as_array()
        .ok_or("the build store's reply is unreadable")?;
    if list.len() != n {
        return Err("the build store answered a short batch".into());
    }
    for r in list {
        let code = r
            .pointer("/status/code")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if code != 0 {
            return Err(format!("the build store refused a blob (status {code})"));
        }
    }
    Ok(())
}

/// Indexes of `blobs` grouped into batches of at most `limit` content bytes,
/// and the ones larger than `limit` (each sent alone).
pub fn batches(blobs: &[Blob], limit: u64) -> (Vec<Vec<usize>>, Vec<usize>) {
    let (mut out, mut big) = (Vec::new(), Vec::new());
    let (mut cur, mut raw) = (Vec::new(), 0u64);
    for (i, b) in blobs.iter().enumerate() {
        if b.size > limit {
            big.push(i);
            continue;
        }
        if !cur.is_empty() && raw + b.size > limit {
            out.push(std::mem::take(&mut cur));
            raw = 0;
        }
        cur.push(i);
        raw += b.size;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    (out, big)
}

/// A path the guest can fill: a non-empty regular file whose path quotes
/// safely in a curl config and a `sha256sum` check list. The rest (links,
/// empty files, odd names) go in the tarball.
pub fn fillable(path: &str, e: &sync::Entry) -> bool {
    e.mode != Mode::Link
        && e.size > 0
        && e.digest != EMPTY
        && !path.is_empty()
        && !path
            .chars()
            .any(|c| c.is_control() || c == '\\' || c == '"')
}

/// The paths of `upload` the store can carry.
pub fn split(tree: &Tree, upload: &[String]) -> Vec<String> {
    upload
        .iter()
        .filter(|p| tree.get(*p).is_some_and(|e| fillable(p, e)))
        .cloned()
        .collect()
}

/// The fill list: one manifest line per path (`<sha256> <mode> <path>`).
pub fn fill_list(tree: &Tree, paths: &[String]) -> String {
    paths
        .iter()
        .filter_map(|p| tree.get(p).map(|e| sync::line(p, e)))
        .collect()
}

/// The curl config [`FILL`] writes on the guest for one fill list line, as
/// the client would render it (used to test the script's quoting).
#[cfg(test)]
pub fn curl_entry(base: &str, out_dir: &str, line: &str) -> String {
    let line = line.trim_end_matches('\n');
    format!(
        "url = \"{base}/cas/{}\"\noutput = \"{out_dir}{}\"\n",
        &line[..64],
        &line[sync::PATH_AT - 1..]
    )
}

/// The paths [`FILL`] reported as not filled. A path it was not given makes
/// the whole answer untrusted (`None`).
pub fn parse_failed(stdout: &[u8], given: &[String]) -> Option<BTreeSet<String>> {
    let given: BTreeSet<&str> = given.iter().map(String::as_str).collect();
    let text = std::str::from_utf8(stdout).ok()?;
    let mut out = BTreeSet::new();
    for l in text.split('\n').filter(|l| !l.is_empty()) {
        if !given.contains(l) {
            return None;
        }
        out.insert(l.to_string());
    }
    Some(out)
}

/// Fills `.sylphx/fill/tree` from the build store on the guest, as the guest
/// user, with `curl`, `sha256sum`, `gzip`, `awk`, `sed`, `tr` and `xargs`.
/// `$1` is the workspace and `$2` the store's base URL inside the Cell; the
/// token comes in [`TOKEN_ENV`]. `.sylphx/fill.gz` is the gzip-compressed
/// fill list (manifest lines).
///
/// It touches nothing outside `.sylphx/fill` (the sync's apply step moves the
/// files into the tree), so it may fail at any point. Exit 0: stdout lists
/// the paths not filled, one per line, and `.sylphx/fill/ok` the manifest
/// lines of the filled ones. Any other exit: nothing was filled.
pub const FILL: &str = r#"set -u
export LC_ALL=C
W=$1 BASE=$2
S=$W/.sylphx F=$W/.sylphx/fill
rm -rf "$F"
mkdir -p "$F/tree" || exit 3
for c in curl sha256sum; do
  command -v "$c" >/dev/null 2>&1 || { echo "$c is missing" >&2; exit 3; }
done
gzip -dc "$S/fill.gz" > "$F/list" || exit 3
rm -f "$S/fill.gz"
old=$(umask)
umask 077
printf 'header = "Authorization: Bearer %s"\n' "${SYLPHX_CAS_TOKEN:?}" > "$F/auth" || exit 3
umask "$old"
awk -v B="$BASE" -v O="$F/tree/" '{ printf "url = \"%s/cas/%s\"\noutput = \"%s%s\"\n", B, substr($0, 1, 64), O, substr($0, 73) }' "$F/list" > "$F/curl" || exit 3
awk '{ print substr($0, 1, 64) "  " substr($0, 73) }' "$F/list" > "$F/sum" || exit 3
curl --parallel --parallel-max 32 --fail --silent --show-error --create-dirs --retry 2 -K "$F/auth" -K "$F/curl" 2> "$F/curl.err"
rm -f "$F/auth"
rc=0
(cd "$F/tree" && sha256sum -c --quiet "$F/sum") > "$F/bad" 2>/dev/null || rc=$?
sed -n -e 's/: FAILED open or read$//p' -e 's/: FAILED$//p' "$F/bad" > "$F/failed"
if [ "$rc" != 0 ] && [ ! -s "$F/failed" ]; then echo "checking the fetched files failed" >&2; exit 4; fi
if [ "$rc" = 0 ] && [ -s "$F/failed" ]; then echo "checking the fetched files failed" >&2; exit 4; fi
awk -v D="$F/failed" 'BEGIN { while ((getline l < D) > 0) b[l] = 1 } !(substr($0, 73) in b)' "$F/list" > "$F/ok" || exit 3
(cd "$F/tree" && tr '\n' '\0' < "$F/failed" | xargs -0 rm -f --) || exit 3
awk '$2 == "100755" { print substr($0, 73) }' "$F/ok" | tr '\n' '\0' > "$F/exec" || exit 3
if [ -s "$F/exec" ]; then (cd "$F/tree" && xargs -0 chmod 755 -- < "$F/exec") || exit 3; fi
cat "$F/failed"
"#;

/// The local work tree and its digests.
#[derive(Clone, Copy)]
pub struct Local<'a> {
    pub root: &'a Path,
    pub tree: &'a Tree,
}

/// What [`fill`] did.
#[derive(Debug, Default)]
pub struct Filled {
    /// The paths the guest filled from the store; the rest go in the tarball.
    pub paths: BTreeSet<String>,
    /// Bytes sent: store requests and the fill list.
    pub sent: u64,
    /// Why some or all files were not filled, for one warning line.
    pub warning: Option<String>,
}

/// Puts the blobs of `upload` that the store lacks into it and has the guest
/// fill those paths from it. A store or fill failure is a warning, never an
/// error: the files it did not fill go in the tarball. Only a guest fault
/// (the machine is gone) is an error.
pub async fn fill(
    g: &Guest,
    store: &Store,
    known: &mut Known,
    local: Local<'_>,
    upload: &[String],
    ws: &str,
    user: &str,
) -> Result<Filled, Fault> {
    let (root, tree) = (local.root, local.tree);
    let mut f = Filled::default();
    let mut paths = split(tree, upload);
    if paths.is_empty() {
        return Ok(f);
    }
    let (ask, from) = wanted(tree, &paths, &known.set);
    if !ask.is_empty() {
        let missing = match store.find_missing(&ask).await {
            Ok((m, sent)) => {
                f.sent += sent;
                m
            }
            Err(e) => {
                f.warning = Some(e);
                return Ok(f);
            }
        };
        let blobs: Vec<Blob> = missing
            .iter()
            .filter_map(|d| {
                Some(Blob {
                    path: from.get(d)?.clone(),
                    digest: d.clone(),
                    size: *ask.get(d)?,
                })
            })
            .collect();
        let changed = match store.upload(root, &blobs).await {
            Ok((sent, changed)) => {
                f.sent += sent;
                changed
            }
            Err(e) => {
                f.warning = Some(e);
                return Ok(f);
            }
        };
        let lost: BTreeSet<&String> = blobs
            .iter()
            .filter(|b| changed.contains(&b.path))
            .map(|b| &b.digest)
            .collect();
        known
            .set
            .extend(ask.keys().filter(|d| !lost.contains(d)).cloned());
        paths.retain(|p| tree.get(p).is_some_and(|e| !lost.contains(&e.digest)));
        if paths.is_empty() {
            return Ok(f);
        }
    }
    let list = gzip(&fill_list(tree, &paths));
    g.upload(&format!("{ws}/.sylphx/fill.gz"), user, &list)
        .await?;
    f.sent += list.len() as u64;
    let env: BTreeMap<String, String> = [(TOKEN_ENV.to_string(), store.token().to_string())].into();
    let args: Vec<String> = ["/bin/sh", "-c", FILL, "sylphx-fill", ws, store.cluster()]
        .map(String::from)
        .to_vec();
    let (code, out, err) = g.run(user, &args, &env).await?;
    if code != 0 {
        let why = String::from_utf8_lossy(&err);
        let why = why.lines().next().unwrap_or("").trim();
        f.warning = Some(if why.is_empty() {
            format!("the build machine could not fill from it (exit {code})")
        } else {
            format!("the build machine could not fill from it: {why}")
        });
        return Ok(f);
    }
    let Some(failed) = parse_failed(&out, &paths) else {
        f.warning = Some("the build machine's fill answer is unreadable".into());
        return Ok(f);
    };
    if !failed.is_empty() {
        f.warning = Some(format!(
            "{} file{} could not be fetched from it",
            failed.len(),
            if failed.len() == 1 { "" } else { "s" }
        ));
        for p in &failed {
            if let Some(e) = tree.get(p) {
                known.set.remove(&e.digest);
            }
        }
    }
    f.paths = paths.into_iter().filter(|p| !failed.contains(p)).collect();
    Ok(f)
}

/// Digests the store had or took, per cache, kept in the git dir between
/// runs so a run asks only about new ones. A digest the store has since lost
/// fails the guest's fill, which drops it here and sends the file directly.
pub struct Known {
    path: Option<PathBuf>,
    key: String,
    pub set: BTreeSet<String>,
}

const KNOWN_HEADER: &str = "sylphx-cas-known 1";

impl Known {
    pub fn load(path: Option<PathBuf>, key: &str) -> Self {
        let mut set = BTreeSet::new();
        if let Some(text) = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            let mut lines = text.split('\n');
            if lines.next() == Some(&format!("{KNOWN_HEADER} {key}")) {
                set.extend(
                    lines
                        .filter(|l| l.len() == 64 && l.bytes().all(|b| b.is_ascii_hexdigit()))
                        .map(str::to_string),
                );
            }
        }
        Self {
            path,
            key: key.to_string(),
            set,
        }
    }

    /// Keeps only the digests of `tree`, so the file stays the tree's size.
    pub fn save(&self, tree: &Tree) {
        let Some(path) = &self.path else { return };
        let live: BTreeSet<&str> = tree.values().map(|e| e.digest.as_str()).collect();
        let mut out = format!("{KNOWN_HEADER} {}\n", self.key);
        for d in self.set.iter().filter(|d| live.contains(d.as_str())) {
            out.push_str(d);
            out.push('\n');
        }
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        if std::fs::write(&tmp, out).is_ok() {
            let _ = std::fs::rename(tmp, path);
        }
    }
}

/// What the store sends for `paths` (fillable ones of the sync's upload):
/// the digests to ask about (not yet known) and, per digest, one path to
/// upload it from.
pub fn wanted(
    tree: &Tree,
    paths: &[String],
    known: &BTreeSet<String>,
) -> (BTreeMap<String, u64>, BTreeMap<String, String>) {
    let (mut ask, mut from) = (BTreeMap::new(), BTreeMap::new());
    for p in paths {
        let Some(e) = tree.get(p) else { continue };
        if known.contains(&e.digest) {
            continue;
        }
        ask.insert(e.digest.clone(), e.size);
        from.entry(e.digest.clone()).or_insert_with(|| p.clone());
    }
    (ask, from)
}

/// The fill list, gzip-compressed for the upload to the guest.
pub fn gzip(text: &str) -> Vec<u8> {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = gz.write_all(text.as_bytes());
    gz.finish().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync::Entry;

    fn ent(content: &str, mode: Mode) -> Entry {
        Entry {
            digest: sync::sha256_hex(content.as_bytes()),
            mode,
            size: content.len() as u64,
        }
    }

    fn blob(path: &str, size: u64) -> Blob {
        Blob {
            path: path.into(),
            digest: format!("{:064x}", size),
            size,
        }
    }

    #[test]
    fn only_digests_not_yet_known_are_asked_about_once_each() {
        let mut t = Tree::new();
        t.insert("a.rs".into(), ent("same", Mode::File));
        t.insert("b.rs".into(), ent("same", Mode::File));
        t.insert("c.rs".into(), ent("known", Mode::File));
        let known: BTreeSet<String> = [sync::sha256_hex(b"known")].into();
        let paths: Vec<String> = ["a.rs", "b.rs", "c.rs"].map(String::from).to_vec();
        let (ask, from) = wanted(&t, &paths, &known);
        assert_eq!(ask.len(), 1);
        assert_eq!(ask[&sync::sha256_hex(b"same")], 4);
        assert_eq!(from[&sync::sha256_hex(b"same")], "a.rs");
    }

    #[test]
    fn missing_and_present_digests_follow_the_reply() {
        let a = sync::sha256_hex(b"a");
        let b = sync::sha256_hex(b"b");
        let asked: BTreeMap<String, u64> = [(a.clone(), 1), (b.clone(), 1)].into();
        let body: Value = serde_json::from_slice(&find_missing_body(
            asked.iter().map(|(d, s)| (d.as_str(), *s)),
        ))
        .unwrap();
        assert_eq!(body["blobDigests"][0]["sizeBytes"], json!("1"));
        assert_eq!(
            parse_missing(
                &json!({"missingBlobDigests": [{"hash": a, "sizeBytes": "1"}]}),
                &asked
            )
            .unwrap(),
            vec![a.clone()]
        );
        // proto3 JSON omits an empty list: nothing is missing.
        assert_eq!(
            parse_missing(&json!({}), &asked).unwrap(),
            Vec::<String>::new()
        );
        // A digest that was not asked about is not trusted.
        let other = sync::sha256_hex(b"other");
        assert!(parse_missing(&json!({"missingBlobDigests": [{"hash": other}]}), &asked).is_err());
        assert!(parse_missing(&json!([]), &asked).is_err());
    }

    #[test]
    fn batches_stay_under_the_limit_and_large_files_go_alone() {
        let blobs = vec![
            blob("a", 2),
            blob("b", 2),
            blob("big", 11),
            blob("c", 5),
            blob("d", 10),
            blob("e", 1),
        ];
        let (b, big) = batches(&blobs, 10);
        assert_eq!(b, vec![vec![0, 1, 3], vec![4], vec![5]]);
        assert_eq!(big, vec![2]);
        for batch in &b {
            assert!(batch.iter().map(|i| blobs[*i].size).sum::<u64>() <= 10);
        }
        let body: Value =
            serde_json::from_slice(&batch_body([("ab", &b"xyz"[..])].into_iter())).unwrap();
        assert_eq!(body["requests"][0]["data"], json!(B64.encode("xyz")));
        assert_eq!(body["requests"][0]["digest"]["sizeBytes"], json!("3"));
    }

    #[test]
    fn links_empty_files_and_odd_names_stay_in_the_tarball() {
        assert!(fillable("src/a b.rs", &ent("x", Mode::File)));
        assert!(fillable("bin/run", &ent("x", Mode::Exec)));
        assert!(!fillable("l", &ent("target", Mode::Link)));
        assert!(!fillable("empty", &ent("", Mode::File)));
        assert!(!fillable("a\"b", &ent("x", Mode::File)));
        assert!(!fillable("a\\b", &ent("x", Mode::File)));
        assert!(!fillable("a\nb", &ent("x", Mode::File)));
    }

    #[test]
    fn the_token_is_in_no_argument_and_the_config_quotes_paths() {
        let st = Store::new(
            "https://cache.example",
            "http://cache.svc/",
            "bc_1",
            "tok.en-1",
        )
        .unwrap();
        assert_eq!(st.cluster(), "http://cache.svc");
        assert_eq!(
            st.v2("blobs:findMissing"),
            "https://cache.example/v2/bc_1/blobs:findMissing"
        );
        let e = ent("x", Mode::File);
        let line = sync::line("dir/a b.rs", &e);
        let c = curl_entry(st.cluster(), "/w/.sylphx/fill/tree/", &line);
        assert_eq!(
            c,
            format!(
                "url = \"http://cache.svc/cas/{}\"\noutput = \"/w/.sylphx/fill/tree/dir/a b.rs\"\n",
                e.digest
            )
        );
        assert!(!c.contains("tok.en-1"));
        // The script reads the token from its environment; argv never has it.
        assert!(FILL.contains("${SYLPHX_CAS_TOKEN:?}") && FILL.contains("umask 077"));
        // A token that would break out of the config's quotes is refused.
        assert!(Store::new("https://c", "http://c", "", "a\"b").is_none());
        assert!(Store::new("https://c", "http://c", "", "a b").is_none());
        assert!(Store::new("https://c", "ftp://c", "", "ab").is_none());
    }

    #[test]
    fn failed_paths_must_be_ones_given() {
        let given: Vec<String> = ["a.rs", "b c.rs"].map(String::from).to_vec();
        assert_eq!(
            parse_failed(b"b c.rs\n", &given),
            Some(["b c.rs".to_string()].into())
        );
        assert_eq!(parse_failed(b"", &given), Some(BTreeSet::new()));
        assert_eq!(parse_failed(b"nope\n", &given), None);
    }

    #[test]
    fn known_digests_are_per_cache_and_kept_to_the_tree() {
        let dir = std::env::temp_dir().join(format!("sylphx-known-{}", std::process::id()));
        let path = dir.join("cas-known");
        let mut t = Tree::new();
        t.insert("a".into(), ent("a", Mode::File));
        let mut k = Known::load(Some(path.clone()), "bc_1");
        k.set.insert(sync::sha256_hex(b"a"));
        k.set.insert(sync::sha256_hex(b"gone"));
        k.save(&t);
        let again = Known::load(Some(path.clone()), "bc_1");
        assert_eq!(again.set, [sync::sha256_hex(b"a")].into());
        assert!(Known::load(Some(path), "bc_2").set.is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    mod real_sh {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        struct Scratch(PathBuf);
        impl Scratch {
            fn new(tag: &str) -> Self {
                let p = std::env::temp_dir().join(format!(
                    "sylphx-fill-{tag}-{}-{:x}",
                    std::process::id(),
                    sylphx_build_lease::guest::rand_u64()
                ));
                std::fs::create_dir_all(&p).unwrap();
                Scratch(p)
            }
        }
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// A `curl` that serves `$FAKE_STORE/<sha256>` for each `url` and
        /// `output` pair of its configs, and logs its arguments, the configs'
        /// modes and the header it was given.
        const FAKE_CURL: &str = r#"#!/bin/sh
cfgs=""
for a in "$@"; do echo "arg:$a" >> "$FAKE_LOG"; done
while [ $# -gt 0 ]; do case "$1" in -K) cfgs="$cfgs $2"; shift 2;; *) shift;; esac; done
for c in $cfgs; do echo "mode:$(stat -c %a "$c")" >> "$FAKE_LOG"; done
cat $cfgs | grep '^header' >> "$FAKE_LOG"
cat $cfgs | sed -n -e 's/^url = "\(.*\)"$/\1/p' -e 's/^output = "\(.*\)"$/\1/p' | while IFS= read -r url && IFS= read -r out; do
  sha=${url##*/}
  [ -f "$FAKE_STORE/$sha" ] || continue
  mkdir -p "$(dirname "$out")"
  cp "$FAKE_STORE/$sha" "$out"
done
exit 0
"#;

        fn tool_dir(dir: &Path, with_curl: bool) {
            std::fs::create_dir_all(dir).unwrap();
            if with_curl {
                let c = dir.join("curl");
                std::fs::write(&c, FAKE_CURL).unwrap();
                std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        fn run_fill(
            ws: &Path,
            tools: &Path,
            path_env: &str,
            store: &Path,
            log: &Path,
        ) -> (i32, String, String) {
            let out = Command::new("/bin/sh")
                .args(["-c", FILL, "sylphx-fill"])
                .arg(ws)
                .arg("http://cache.svc")
                .env("PATH", format!("{}:{path_env}", tools.display()))
                .env(TOKEN_ENV, "tok-secret-123")
                .env("FAKE_STORE", store)
                .env("FAKE_LOG", log)
                .output()
                .unwrap();
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into(),
                String::from_utf8_lossy(&out.stderr).into(),
            )
        }

        /// The fill against a real `sh` with a fake `curl`: the good files
        /// arrive with their modes, a corrupted and a missing blob are
        /// reported (and their files removed), the token is only in a 0600
        /// config that is gone afterwards, and the apply step then makes a
        /// manifest equal to the tree.
        #[test]
        fn the_guest_fills_from_the_store_and_reports_what_it_could_not() {
            let s = Scratch::new("ok");
            let (ws, store_dir, tools, log) = (
                s.0.join("ws"),
                s.0.join("store"),
                s.0.join("bin"),
                s.0.join("log"),
            );
            std::fs::create_dir_all(ws.join(".sylphx")).unwrap();
            std::fs::create_dir_all(&store_dir).unwrap();
            tool_dir(&tools, true);
            let files: &[(&str, &str, Mode)] = &[
                ("src/a b.rs", "fn a() {}\n", Mode::File),
                ("bin/run", "#!/bin/sh\n", Mode::Exec),
                ("bad.rs", "good\n", Mode::File),
                ("gone.rs", "gone\n", Mode::File),
                ("empty", "", Mode::File),
            ];
            let mut tree = Tree::new();
            for (p, c, m) in files {
                let e = ent(c, *m);
                if *p == "bad.rs" {
                    std::fs::write(store_dir.join(&e.digest), "tampered\n").unwrap();
                } else if *p != "gone.rs" {
                    std::fs::write(store_dir.join(&e.digest), c).unwrap();
                }
                tree.insert(p.to_string(), e);
            }
            let upload: Vec<String> = tree.keys().cloned().collect();
            let paths = split(&tree, &upload);
            assert!(!paths.contains(&"empty".to_string()));
            std::fs::write(ws.join(".sylphx/fill.gz"), gzip(&fill_list(&tree, &paths))).unwrap();
            let sys = std::env::var("PATH").unwrap_or_default();
            let (code, out, err) = run_fill(&ws, &tools, &sys, &store_dir, &log);
            assert_eq!(code, 0, "{err}");
            let failed = parse_failed(out.as_bytes(), &paths).unwrap();
            assert_eq!(failed, ["bad.rs".to_string(), "gone.rs".to_string()].into());
            let ft = ws.join(".sylphx/fill/tree");
            assert_eq!(
                std::fs::read_to_string(ft.join("src/a b.rs")).unwrap(),
                "fn a() {}\n"
            );
            assert!(!ft.join("bad.rs").exists() && !ft.join("gone.rs").exists());
            let mode = std::fs::metadata(ft.join("bin/run"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111);
            let log = std::fs::read_to_string(&log).unwrap();
            assert!(
                log.contains("header = \"Authorization: Bearer tok-secret-123\""),
                "{log}"
            );
            assert!(log.contains("mode:600"), "{log}");
            for l in log.lines().filter(|l| l.starts_with("arg:")) {
                assert!(!l.contains("tok-secret"), "token in argv: {l}");
            }
            assert!(!ws.join(".sylphx/fill/auth").exists());
            // The script's curl config is the one the client would render.
            let want: String = fill_list(&tree, &paths)
                .lines()
                .map(|l| {
                    curl_entry(
                        "http://cache.svc",
                        &format!("{}/.sylphx/fill/tree/", ws.display()),
                        l,
                    )
                })
                .collect();
            assert_eq!(
                std::fs::read_to_string(ws.join(".sylphx/fill/curl")).unwrap(),
                want
            );
            assert!(!ws.join(".sylphx/fill.gz").exists());

            // The apply step: a full sync of the files not filled, then the
            // filled ones, gives a manifest equal to the tree.
            let filled: BTreeSet<String> = paths
                .iter()
                .filter(|p| !failed.contains(*p))
                .cloned()
                .collect();
            let plan = sync::plan(&tree, None);
            let l = sync::lists(&tree, &plan, &filled);
            let sd = ws.join(".sylphx");
            std::fs::write(sd.join("add"), &l.add).unwrap();
            let root = s.0.join("src");
            for (p, c, m) in files {
                let f = root.join(p);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(&f, c).unwrap();
                if *m == Mode::Exec {
                    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
                }
            }
            let direct: Vec<String> = upload
                .iter()
                .filter(|p| !filled.contains(*p))
                .cloned()
                .collect();
            for (i, t) in sync::tarballs(&root, &direct, &tree)
                .unwrap()
                .iter()
                .enumerate()
            {
                std::fs::write(sd.join(format!("in-{i:04}.tar.gz")), t).unwrap();
            }
            let ok = Command::new("sh")
                .args(["-c", sync::APPLY, "apply"])
                .arg(&ws)
                .args(["1", "1"])
                .status()
                .unwrap()
                .success();
            assert!(ok);
            let m = sync::parse(&std::fs::read_to_string(sd.join("manifest")).unwrap()).unwrap();
            let want: BTreeMap<String, (String, Mode)> = tree
                .iter()
                .map(|(p, e)| (p.clone(), (e.digest.clone(), e.mode)))
                .collect();
            let got: BTreeMap<String, (String, Mode)> = m
                .iter()
                .map(|(p, e)| (p.clone(), (e.digest.clone(), e.mode)))
                .collect();
            assert_eq!(got, want);
            for (p, c, _) in files {
                assert_eq!(
                    std::fs::read_to_string(ws.join("tree").join(p)).unwrap(),
                    *c,
                    "{p}"
                );
            }
            assert!(!sd.join("fill").exists());
        }

        /// No `curl` on the machine: the fill fails as a whole (so the
        /// client sends everything directly) and touches nothing else.
        #[test]
        fn a_machine_without_curl_fills_nothing() {
            let s = Scratch::new("nocurl");
            let (ws, tools) = (s.0.join("ws"), s.0.join("bin"));
            std::fs::create_dir_all(ws.join(".sylphx")).unwrap();
            tool_dir(&tools, false);
            // Only the tools the script needs before it looks for curl.
            for t in ["rm", "mkdir", "gzip", "sha256sum"] {
                let found = Command::new("sh")
                    .args(["-c", &format!("command -v {t}")])
                    .output()
                    .unwrap();
                let at = String::from_utf8_lossy(&found.stdout).trim().to_string();
                std::os::unix::fs::symlink(at, tools.join(t)).unwrap();
            }
            let mut tree = Tree::new();
            tree.insert("a.rs".into(), ent("a\n", Mode::File));
            let paths: Vec<String> = vec!["a.rs".into()];
            std::fs::write(ws.join(".sylphx/fill.gz"), gzip(&fill_list(&tree, &paths))).unwrap();
            let out = Command::new("/bin/sh")
                .args(["-c", FILL, "sylphx-fill"])
                .arg(&ws)
                .arg("http://cache.svc")
                .env("PATH", &tools)
                .env(TOKEN_ENV, "tok")
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(3));
            assert!(String::from_utf8_lossy(&out.stderr).contains("curl is missing"));
            assert!(!ws.join("tree").exists());
        }
    }

    /// A fake build store and guest on one local port: each request is
    /// logged (path, body bytes) and answered by `route`.
    mod fake {
        use super::*;
        use std::sync::{Arc, Mutex};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        pub type Log = Arc<Mutex<Vec<(String, usize)>>>;
        pub type Route = fn(&str, &[u8]) -> (u16, &'static str, Vec<u8>);

        pub async fn serve(route: Route) -> (String, Log) {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", l.local_addr().unwrap());
            let log: Log = Default::default();
            let log2 = log.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((mut c, _)) = l.accept().await else {
                        return;
                    };
                    let log = log2.clone();
                    tokio::spawn(async move {
                        let mut buf = Vec::new();
                        loop {
                            let head_end = loop {
                                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                                    break i + 4;
                                }
                                let mut chunk = [0u8; 65536];
                                match c.read(&mut chunk).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                }
                            };
                            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                            let path = head.split(' ').nth(1).unwrap_or("").to_string();
                            let len: usize = head
                                .lines()
                                .find_map(|l| {
                                    let (k, v) = l.split_once(':')?;
                                    k.eq_ignore_ascii_case("content-length")
                                        .then(|| v.trim().parse().ok())?
                                })
                                .unwrap_or(0);
                            while buf.len() < head_end + len {
                                let mut chunk = [0u8; 65536];
                                match c.read(&mut chunk).await {
                                    Ok(0) | Err(_) => return,
                                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                                }
                            }
                            let body: Vec<u8> =
                                buf.drain(..head_end + len).skip(head_end).collect();
                            log.lock().unwrap().push((path.clone(), body.len()));
                            let (status, ctype, out) = route(&path, &body);
                            let resp = format!(
                                "HTTP/1.1 {status} X\r\ncontent-type: {ctype}\r\ncontent-length: {}\r\n\r\n",
                                out.len()
                            );
                            if c.write_all(resp.as_bytes()).await.is_err()
                                || c.write_all(&out).await.is_err()
                            {
                                return;
                            }
                        }
                    });
                }
            });
            (base, log)
        }

        /// A process stream that writes `stdout` and exits `code`.
        pub fn process(stdout: &str, code: i32) -> Vec<u8> {
            let mut out =
                sylphx_build_lease::guest::envelope(&json!({"event": {"start": {"pid": 7}}}));
            if !stdout.is_empty() {
                out.extend(sylphx_build_lease::guest::envelope(
                    &json!({"event": {"data": {"stdout": B64.encode(stdout)}}}),
                ));
            }
            out.extend(sylphx_build_lease::guest::envelope(
                &json!({"event": {"end": {"exitCode": code, "exited": true}}}),
            ));
            let mut end = sylphx_build_lease::guest::envelope(&json!({}));
            end[0] = 2;
            out.extend(end);
            out
        }
    }

    fn three_files(root: &Path) -> Tree {
        let mut t = Tree::new();
        for (p, c) in [
            ("known.rs", "known\n"),
            ("present.rs", "present\n"),
            ("missing.rs", "missing\n"),
        ] {
            std::fs::write(root.join(p), c).unwrap();
            t.insert(p.into(), ent(c, Mode::File));
        }
        t
    }

    fn sent_to(log: &fake::Log, suffix: &str) -> usize {
        log.lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p.ends_with(suffix))
            .map(|(_, n)| *n)
            .sum()
    }

    /// Only the blob the store lacks is uploaded, the guest fills every
    /// path, and `sent` is exactly the bytes of the requests and the list.
    #[tokio::test]
    async fn only_missing_blobs_are_uploaded_and_every_byte_sent_is_counted() {
        fn route(path: &str, body: &[u8]) -> (u16, &'static str, Vec<u8>) {
            if path.ends_with("blobs:findMissing") {
                let v: Value = serde_json::from_slice(body).unwrap();
                let asked: Vec<&str> = v["blobDigests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|d| d["hash"].as_str().unwrap())
                    .collect();
                let miss = sync::sha256_hex(b"missing\n");
                assert!(
                    !asked.contains(&sync::sha256_hex(b"known\n").as_str()),
                    "a known digest was asked about"
                );
                let m: Vec<Value> = asked
                    .iter()
                    .filter(|h| **h == miss)
                    .map(|h| json!({"hash": h, "sizeBytes": "8"}))
                    .collect();
                return (
                    200,
                    "application/json",
                    serde_json::to_vec(&json!({"missingBlobDigests": m})).unwrap(),
                );
            }
            if path.ends_with("blobs:batchUpdate") {
                let v: Value = serde_json::from_slice(body).unwrap();
                let reqs = v["requests"].as_array().unwrap();
                assert_eq!(reqs.len(), 1);
                assert_eq!(
                    reqs[0]["digest"]["hash"],
                    json!(sync::sha256_hex(b"missing\n"))
                );
                return (
                    200,
                    "application/json",
                    serde_json::to_vec(
                        &json!({"responses": [{"digest": reqs[0]["digest"], "status": {}}]}),
                    )
                    .unwrap(),
                );
            }
            if path.starts_with("/files") {
                return (200, "application/json", b"[]".to_vec());
            }
            if path.ends_with("process.Process/Start") {
                return (200, "application/connect+json", fake::process("", 0));
            }
            (404, "text/plain", Vec::new())
        }
        let (base, log) = fake::serve(route).await;
        let root = std::env::temp_dir().join(format!("sylphx-store-ok-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let tree = three_files(&root);
        let st = Store::new(&base, "http://cache.svc", "bc_1", "tok").unwrap();
        let g = Guest::new(&base, "sbx", "lease-tok").unwrap();
        let mut known = Known::load(None, "k");
        known.set.insert(sync::sha256_hex(b"known\n"));
        let upload: Vec<String> = tree.keys().cloned().collect();
        let f = fill(
            &g,
            &st,
            &mut known,
            Local {
                root: &root,
                tree: &tree,
            },
            &upload,
            "/workspace",
            "user",
        )
        .await
        .unwrap();
        assert_eq!(f.warning, None);
        assert_eq!(f.paths.len(), 3);
        let want = sent_to(&log, "blobs:findMissing")
            + sent_to(&log, "blobs:batchUpdate")
            + gzip(&fill_list(&tree, &split(&tree, &upload))).len();
        assert_eq!(f.sent as usize, want);
        assert!(sent_to(&log, "fill.gz&username=user") > 0);
        // Both new digests are known now.
        assert_eq!(known.set.len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The store refusing, or the guest unable to fill, is a warning and an
    /// empty fill: every file then goes in the tarball.
    #[tokio::test]
    async fn a_failing_store_or_fill_falls_back_to_the_tarball() {
        fn down(path: &str, _: &[u8]) -> (u16, &'static str, Vec<u8>) {
            if path.contains("/v2/") {
                return (503, "text/plain", b"down".to_vec());
            }
            panic!("the guest was called after the store failed: {path}");
        }
        let root = std::env::temp_dir().join(format!("sylphx-store-fb-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let tree = three_files(&root);
        let upload: Vec<String> = tree.keys().cloned().collect();
        let (base, _) = fake::serve(down).await;
        let st = Store::new(&base, "http://cache.svc", "", "tok").unwrap();
        let g = Guest::new(&base, "sbx", "t").unwrap();
        let mut known = Known::load(None, "k");
        let f = fill(
            &g,
            &st,
            &mut known,
            Local {
                root: &root,
                tree: &tree,
            },
            &upload,
            "/workspace",
            "user",
        )
        .await
        .unwrap();
        assert!(f.paths.is_empty());
        assert!(f.warning.unwrap().contains("HTTP 503"));
        assert!(known.set.is_empty());

        fn no_curl(path: &str, _: &[u8]) -> (u16, &'static str, Vec<u8>) {
            if path.ends_with("blobs:findMissing") {
                return (200, "application/json", b"{}".to_vec());
            }
            if path.ends_with("process.Process/Start") {
                return (200, "application/connect+json", fake::process("", 3));
            }
            (200, "application/json", b"[]".to_vec())
        }
        let (base, _) = fake::serve(no_curl).await;
        let st = Store::new(&base, "http://cache.svc", "", "tok").unwrap();
        let g = Guest::new(&base, "sbx", "t").unwrap();
        let f = fill(
            &g,
            &st,
            &mut known,
            Local {
                root: &root,
                tree: &tree,
            },
            &upload,
            "/workspace",
            "user",
        )
        .await
        .unwrap();
        assert!(f.paths.is_empty());
        assert!(f.warning.unwrap().contains("exit 3"));

        fn partial(path: &str, _: &[u8]) -> (u16, &'static str, Vec<u8>) {
            if path.ends_with("blobs:findMissing") {
                return (200, "application/json", b"{}".to_vec());
            }
            if path.ends_with("process.Process/Start") {
                return (
                    200,
                    "application/connect+json",
                    fake::process("present.rs\n", 0),
                );
            }
            (200, "application/json", b"[]".to_vec())
        }
        let (base, _) = fake::serve(partial).await;
        let st = Store::new(&base, "http://cache.svc", "", "tok").unwrap();
        let g = Guest::new(&base, "sbx", "t").unwrap();
        let mut known = Known::load(None, "k");
        let f = fill(
            &g,
            &st,
            &mut known,
            Local {
                root: &root,
                tree: &tree,
            },
            &upload,
            "/workspace",
            "user",
        )
        .await
        .unwrap();
        assert_eq!(
            f.paths,
            ["known.rs".to_string(), "missing.rs".to_string()].into()
        );
        assert!(f.warning.unwrap().starts_with("1 file could not"));
        // The digest the store lost is asked about again next time.
        assert!(!known.set.contains(&sync::sha256_hex(b"present\n")));
        let _ = std::fs::remove_dir_all(&root);
    }
}
