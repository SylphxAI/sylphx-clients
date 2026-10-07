//! Docs over a Sylphx Data search index: a Markdown tree chunked by heading,
//! each chunk a document with its path and line numbers, searched and read
//! back by path and lines.
//!
//! The pattern is the one docs-search leaders run (Typesense's and
//! Meilisearch's docs scrapers, Algolia DocSearch, Context7): an indexer run
//! on each merge writes heading-sized records to a search index, and readers
//! get a ranked list of `path:line` references plus the text. Here the index
//! is an ordinary search index of the caller's environment and every call
//! goes through the public Search API with the environment's key, so a
//! reader needs `data:read` on that environment and no access to the
//! repository the docs came from.
//!
//! - [`sync`] writes one document per chunk, stamped with a revision, then
//!   deletes the documents of older revisions (`sylphx data docs sync`).
//! - [`search`] answers the best chunks for a query, each with the line that
//!   matched (`docs_search`, `sylphx data docs search`).
//! - [`read`] reassembles a file, or a line range of it, from its chunks
//!   (`docs_read`, `sylphx data docs read`).

use std::path::Path;

use serde_json::{json, Value};
use sylphx::data::{DeleteDocumentRequest, PutDocumentRequest, SearchFilter, SearchRequest};
use sylphx::{Client, Error, Transport};

/// Environment variable naming the search index the docs tools read.
pub const INDEX_ENV: &str = "SYLPHX_DOCS_INDEX";
/// Environment variable naming the source the docs tools read, when one
/// index holds several.
pub const SOURCE_ENV: &str = "SYLPHX_DOCS_SOURCE";

/// A chunk longer than this is split into line windows of at most this many
/// bytes, so a long section still ranks by the part that matched.
pub const MAX_CHUNK_BYTES: usize = 24 * 1024;
/// Headings at this level or above start a chunk (`#`, `##`, `###`).
const SPLIT_LEVEL: usize = 3;
/// The most lines [`read`] returns when no end line is given.
pub const MAX_READ_LINES: usize = 600;
/// Hits [`search`] returns by default and at most.
pub const DEFAULT_SEARCH_LIMIT: u32 = 8;
pub const MAX_SEARCH_LIMIT: u32 = 20;
/// The marker field and value on every document [`sync`] writes: only
/// documents carrying it are ever read as docs or deleted as stale, so other
/// documents in the same index are never touched.
pub const KIND_FIELD: &str = "kind";
pub const KIND: &str = "docs-section";
/// Page size of the listing reads; the Search API caps offset + limit at 1000.
const PAGE: u32 = 100;
const MAX_OFFSET: u32 = 900;

/// The index (and optional source) the docs tools read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocsIndex {
    pub index: String,
    pub source: Option<String>,
}

impl DocsIndex {
    /// From [`INDEX_ENV`] and [`SOURCE_ENV`]; `None` when no index is named.
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        get(INDEX_ENV).map(|index| Self {
            index: index.trim().to_string(),
            source: get(SOURCE_ENV).map(|s| s.trim().to_string()),
        })
    }

    /// Readers: this connector's documents, of the named source if any.
    fn filters(&self, mut more: Vec<SearchFilter>) -> Vec<SearchFilter> {
        more.push(filter(KIND_FIELD, "eq", json!(KIND)));
        if let Some(source) = &self.source {
            more.push(filter("source", "eq", json!(source)));
        }
        more
    }

    /// The documents a sync owns: this connector's, of exactly this source
    /// (`""` when none is named, so a sync without a source never touches
    /// another source's documents), stamped with a revision other than
    /// `revision`.
    fn stale_filters(&self, revision: &str) -> Vec<SearchFilter> {
        vec![
            filter(KIND_FIELD, "eq", json!(KIND)),
            filter("source", "eq", json!(self.source.as_deref().unwrap_or(""))),
            exists("revision"),
            filter("revision", "ne", json!(revision)),
        ]
    }
}

/// One heading-sized piece of a Markdown file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// The file's path below the root, `/`-separated.
    pub path: String,
    /// The file's first `#` heading, or its path.
    pub title: String,
    /// The headings above this chunk, outermost first, joined by ` > `.
    pub heading: String,
    /// First and last line, 1-based and inclusive.
    pub start_line: usize,
    pub end_line: usize,
    /// The lines, joined by `\n`.
    pub text: String,
}

/// An ATX heading's level and text: up to three spaces, one to six `#`, then
/// a space or the end of the line.
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let level = rest.len() - rest.trim_start_matches('#').len();
    if level == 0 || level > 6 {
        return None;
    }
    let after = &rest[level..];
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    // A closing run of `#` counts only after a space (`## C#` keeps its `#`).
    let mut text = after.trim();
    let bare = text.trim_end_matches('#');
    if bare.is_empty() || bare.ends_with(' ') || bare.ends_with('\t') {
        text = bare.trim_end();
    }
    Some((level, text.to_string()))
}

/// The fence a line opens or closes: its character and length.
fn fence(line: &str) -> Option<(char, usize)> {
    let t = line.trim_start();
    if line.len() - t.len() > 3 {
        return None;
    }
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let n = t.chars().take_while(|x| *x == c).count();
    (n >= 3).then_some((c, n))
}

/// Splits a Markdown file at its `#` to `###` headings (outside code
/// fences). Text before the first heading is a chunk of its own; the chunks
/// cover every line once, in order, so joining their texts with `\n` gives
/// the file back (less a final newline).
pub fn chunk_markdown(path: &str, text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return vec![];
    }
    let mut title: Option<String> = None;
    // (start index, heading breadcrumb) of each section.
    let mut starts: Vec<(usize, String)> = vec![(0, String::new())];
    let mut stack: Vec<(usize, String)> = vec![];
    let mut open: Option<(char, usize)> = None;
    for (i, line) in lines.iter().enumerate() {
        if let Some((c, n)) = fence(line) {
            match open {
                None => open = Some((c, n)),
                Some((oc, on)) if oc == c && n >= on && line.trim().chars().all(|x| x == c) => {
                    open = None
                }
                _ => {}
            }
            continue;
        }
        if open.is_some() {
            continue;
        }
        let Some((level, heading)) = atx_heading(line) else {
            continue;
        };
        if level == 1 && title.is_none() && !heading.is_empty() {
            title = Some(heading.clone());
        }
        if level > SPLIT_LEVEL {
            continue;
        }
        stack.retain(|(l, _)| *l < level);
        stack.push((level, heading));
        let crumb = stack
            .iter()
            .map(|(_, h)| h.as_str())
            .collect::<Vec<_>>()
            .join(" > ");
        if i == 0 {
            starts[0].1 = crumb;
        } else {
            starts.push((i, crumb));
        }
    }
    let title = title.unwrap_or_else(|| path.to_string());
    let mut out = Vec::new();
    for (k, (start, heading)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(lines.len(), |(s, _)| *s);
        if *start >= end {
            continue;
        }
        // Windows of at most MAX_CHUNK_BYTES (a longer single line stays whole).
        let mut w = *start;
        while w < end {
            let mut bytes = 0;
            let mut e = w;
            while e < end && (e == w || bytes + lines[e].len() < MAX_CHUNK_BYTES) {
                bytes += lines[e].len() + 1;
                e += 1;
            }
            out.push(Chunk {
                path: path.to_string(),
                title: title.clone(),
                heading: heading.clone(),
                start_line: w + 1,
                end_line: e,
                text: lines[w..e].join("\n"),
            });
            w = e;
        }
    }
    out
}

/// The document id of a chunk: readable, stable across runs, and one path
/// segment (the Search API's ids carry no `/`).
pub fn document_id(source: Option<&str>, path: &str, start_line: usize) -> String {
    let path = path.replace('/', "~");
    match source {
        Some(s) => format!("{}:{path}:L{start_line}", s.replace('/', "~")),
        None => format!("{path}:L{start_line}"),
    }
}

/// Whether `path` (`/`-separated) matches `pattern`: `*` is any run within
/// one segment, `**` any run of whole segments (none included), `?` one
/// character.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    fn segs(p: &[&str], s: &[&str]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(&"**") => (0..=s.len()).any(|i| segs(&p[1..], &s[i..])),
            Some(seg) => {
                !s.is_empty() && one(seg.as_bytes(), s[0].as_bytes()) && segs(&p[1..], &s[1..])
            }
        }
    }
    fn one(p: &[u8], s: &[u8]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some(b'*') => (0..=s.len()).any(|i| one(&p[1..], &s[i..])),
            Some(b'?') => !s.is_empty() && one(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && one(&p[1..], &s[1..]),
        }
    }
    let p: Vec<&str> = pattern.split('/').filter(|x| !x.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|x| !x.is_empty()).collect();
    segs(&p, &s)
}

/// The Markdown files below `root` that match any of `patterns`, as
/// (`/`-separated relative path, text), sorted by path. Hidden directories
/// and `node_modules` are skipped.
pub fn collect_files(root: &Path, patterns: &[String]) -> std::io::Result<Vec<(String, String)>> {
    fn walk(dir: &Path, rel: &str, out: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !name.starts_with('.') && name != "node_modules" {
                    walk(&entry.path(), &rel, out)?;
                }
            } else if kind.is_file() {
                out.push(rel);
            }
        }
        Ok(())
    }
    let mut all = Vec::new();
    walk(root, "", &mut all)?;
    all.sort();
    let mut out = Vec::new();
    for rel in all {
        if patterns.iter().any(|p| glob_match(p, &rel)) {
            out.push((rel.clone(), std::fs::read_to_string(root.join(&rel))?));
        }
    }
    Ok(out)
}

/// The query's words, as lower-case stems: quotes dropped, `or` and
/// `-excluded` words skipped, and a common English ending (`ing`, `ed`,
/// `es`, `s`, then a final `e`) trimmed so `scoring`, `scored` and `score`
/// meet.
fn stems(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in query.split_whitespace() {
        if word.starts_with('-') || word.eq_ignore_ascii_case("or") {
            continue;
        }
        let w: String = word
            .chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        if w.is_empty() {
            continue;
        }
        let mut s = w.as_str();
        for suffix in ["ing", "ed", "es", "s"] {
            if s.len() > suffix.len() + 2 && s.ends_with(suffix) {
                s = &s[..s.len() - suffix.len()];
                break;
            }
        }
        if s.len() > 3 && s.ends_with('e') {
            s = &s[..s.len() - 1];
        }
        if !out.iter().any(|x| x == s) {
            out.push(s.to_string());
        }
    }
    out
}

/// The line of a chunk that best matches `query`: the one holding the most
/// distinct query words, then the one holding them as a phrase (earliest on
/// a tie), else the chunk's first line.
/// Returns its 1-based number and its text.
pub fn best_line(text: &str, start_line: usize, query: &str) -> (usize, String) {
    let stems = stems(query);
    // The words side by side, in order, rank a line above one that only holds them.
    let phrase = (stems.len() > 1).then(|| stems.join(" "));
    let mut best: Option<(usize, usize, &str)> = None;
    for (i, line) in text.lines().enumerate() {
        let lower = line.to_lowercase();
        let n = 2 * stems.iter().filter(|s| lower.contains(s.as_str())).count()
            + usize::from(phrase.as_ref().is_some_and(|p| lower.contains(p.as_str())));
        if n > 0 && best.is_none_or(|(b, _, _)| n > b) {
            best = Some((n, i, line));
        }
    }
    match best {
        Some((_, i, line)) => (start_line + i, line.trim().to_string()),
        None => (
            start_line,
            text.lines().next().unwrap_or_default().trim().to_string(),
        ),
    }
}

/// At most `max` bytes of `s`, cut on a character boundary.
fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn exists(field: &str) -> SearchFilter {
    let mut f = SearchFilter::default();
    f.field = field.to_string();
    f.op = "exists".to_string();
    f
}

fn filter(field: &str, op: &str, value: Value) -> SearchFilter {
    let mut f = SearchFilter::default();
    f.field = field.to_string();
    f.op = op.to_string();
    f.value_json = value.to_string();
    f
}

/// A stored chunk: the document JSON (base64 on the wire) decoded.
fn stored(document_json: &str) -> Option<Value> {
    let bytes = b64_decode(document_json)?;
    serde_json::from_slice(&bytes).ok()
}

async fn page<T: Transport>(
    client: &Client<T>,
    index: &str,
    query: &str,
    filters: Vec<SearchFilter>,
    limit: u32,
    offset: u32,
) -> Result<Vec<(String, Value, f64)>, Error> {
    let mut req = SearchRequest::default();
    req.index_id = index.to_string();
    req.query = query.to_string();
    req.limit = Some(limit);
    req.offset = offset;
    req.filters = filters;
    let res = client.data().search().query(req).await?;
    Ok(res
        .hits
        .into_iter()
        .filter_map(|h| {
            let d = h.document?;
            Some((d.document_id, stored(&d.document_json)?, h.score))
        })
        .collect())
}

fn as_line(v: &Value) -> usize {
    v.as_u64().unwrap_or(1) as usize
}

/// The best chunks for `query`, each with the line that matched:
/// `{ "hits": [{ "ref": "standards/risk.md:42", "path", "line", "title",
/// "heading", "start_line", "end_line", "snippet", "score" }] }`.
pub async fn search<T: Transport>(
    client: &Client<T>,
    docs: &DocsIndex,
    query: &str,
    limit: Option<u32>,
) -> Result<Value, Error> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::InvalidArgument("query is required".into()));
    }
    let limit = limit
        .unwrap_or(DEFAULT_SEARCH_LIMIT)
        .clamp(1, MAX_SEARCH_LIMIT);
    let hits = page(client, &docs.index, query, docs.filters(vec![]), limit, 0).await?;
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|(_, d, score)| {
            let path = d["path"].as_str().unwrap_or_default().to_string();
            let (line, text) = best_line(
                d["text"].as_str().unwrap_or_default(),
                as_line(&d["start_line"]),
                query,
            );
            json!({
                "ref": format!("{path}:{line}"),
                "path": path,
                "line": line,
                "title": d["title"],
                "heading": d["heading"],
                "start_line": d["start_line"],
                "end_line": d["end_line"],
                "snippet": clip(&text, 300),
                "score": score,
            })
        })
        .collect();
    Ok(json!({ "query": query, "hits": hits }))
}

/// A file, or lines `start_line..=end_line` of it, reassembled from its
/// chunks: `{ "path", "title", "start_line", "end_line", "total_lines",
/// "truncated", "text" }`. Without an end line at most [`MAX_READ_LINES`]
/// lines come back and `truncated` says whether more follow.
pub async fn read<T: Transport>(
    client: &Client<T>,
    docs: &DocsIndex,
    path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Result<Value, Error> {
    let path = path.trim().trim_start_matches("./").to_string();
    if path.is_empty() {
        return Err(Error::InvalidArgument("path is required".into()));
    }
    let mut chunks: Vec<Value> = Vec::new();
    let mut offset = 0;
    loop {
        let got = page(
            client,
            &docs.index,
            "",
            docs.filters(vec![filter("path", "eq", json!(path))]),
            PAGE,
            offset,
        )
        .await?;
        let n = got.len() as u32;
        chunks.extend(got.into_iter().map(|(_, d, _)| d));
        if n < PAGE || offset >= MAX_OFFSET {
            break;
        }
        offset += PAGE;
    }
    if chunks.is_empty() {
        return Err(Error::InvalidArgument(format!(
            "no document at `{path}` in this index; find one with docs_search"
        )));
    }
    chunks.sort_by_key(|d| as_line(&d["start_line"]));
    let title = chunks[0]["title"].clone();
    let mut lines: Vec<String> = Vec::new();
    for d in &chunks {
        // Chunks are contiguous; a gap (a sync caught midway) reads as blank lines.
        let start = as_line(&d["start_line"]);
        while lines.len() + 1 < start {
            lines.push(String::new());
        }
        let text = d["text"].as_str().unwrap_or_default();
        let skip = (lines.len() + 1).saturating_sub(start);
        lines.extend(text.lines().skip(skip).map(str::to_string));
    }
    let total = lines.len();
    let from = start_line.unwrap_or(1).max(1);
    if from > total {
        return Err(Error::InvalidArgument(format!(
            "`{path}` has {total} lines; start_line {from} is past the end"
        )));
    }
    let (to, truncated) = match end_line {
        Some(e) => (e.clamp(from, total), false),
        None => {
            let to = total.min(from + MAX_READ_LINES - 1);
            (to, to < total)
        }
    };
    Ok(json!({
        "path": path,
        "title": title,
        "start_line": from,
        "end_line": to,
        "total_lines": total,
        "truncated": truncated,
        "text": lines[from - 1..to].join("\n"),
    }))
}

/// What [`sync`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub files: usize,
    pub chunks: usize,
    pub written: usize,
    pub deleted: usize,
    /// Documents of this source a real run would delete (counted by a dry
    /// run, up to 1000; equal to `deleted` after a real run).
    pub stale: usize,
}

/// Writes every chunk of `files` to the index, stamped with `revision`, then
/// deletes this source's documents whose revision differs: a file or
/// section removed since the last run leaves no stale hit. Only documents
/// the connector wrote ([`KIND`]) and of exactly this source (none named:
/// documents with no source) are deleted; anything else in the index stays.
/// With `dry_run` nothing is written or deleted and `stale` counts what a
/// real run would delete. An empty file set is refused, so a wrong pattern
/// never empties the index.
pub async fn sync<T: Transport>(
    client: &Client<T>,
    docs: &DocsIndex,
    revision: &str,
    files: &[(String, String)],
    dry_run: bool,
) -> Result<SyncReport, Error> {
    if files.is_empty() {
        return Err(Error::InvalidArgument(
            "no files matched: nothing synced and nothing deleted".into(),
        ));
    }
    if revision.trim().is_empty() {
        return Err(Error::InvalidArgument("revision is required".into()));
    }
    let mut report = SyncReport {
        files: files.len(),
        ..Default::default()
    };
    let mut ids = std::collections::HashSet::new();
    for (path, text) in files {
        for c in chunk_markdown(path, text) {
            report.chunks += 1;
            let id = document_id(docs.source.as_deref(), &c.path, c.start_line);
            if dry_run {
                ids.insert(id);
                continue;
            }
            let mut doc = json!({
                "path": c.path,
                "title": c.title,
                "heading": c.heading,
                "start_line": c.start_line,
                "end_line": c.end_line,
                "text": c.text,
                "revision": revision,
                "source": docs.source.as_deref().unwrap_or(""),
            });
            doc[KIND_FIELD] = json!(KIND);
            let mut req = PutDocumentRequest::default();
            req.index_id = docs.index.clone();
            req.document_id = id;
            req.document_json = b64_encode(doc.to_string().as_bytes());
            client.data().documents().put(req).await?;
            report.written += 1;
        }
    }
    if dry_run {
        // What a real run would delete: owned documents it would not rewrite.
        let mut offset = 0;
        loop {
            let got = page(
                client,
                &docs.index,
                "",
                docs.stale_filters(revision),
                PAGE,
                offset,
            )
            .await?;
            let n = got.len() as u32;
            report.stale += got.iter().filter(|(id, _, _)| !ids.contains(id)).count();
            if n < PAGE || offset >= MAX_OFFSET {
                break;
            }
            offset += PAGE;
        }
        return Ok(report);
    }
    // Each round deletes what it lists, so the next round lists the rest.
    for _ in 0..1000 {
        let stale = page(
            client,
            &docs.index,
            "",
            docs.stale_filters(revision),
            PAGE,
            0,
        )
        .await?;
        if stale.is_empty() {
            break;
        }
        for (id, _, _) in stale {
            let mut req = DeleteDocumentRequest::default();
            req.index_id = docs.index.clone();
            req.document_id = id;
            client.data().documents().delete(req).await?;
            report.deleted += 1;
        }
    }
    report.stale = report.deleted;
    Ok(report)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding: the wire form of a document's JSON.
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= c.len() {
                out.push(B64[(n >> shift) as usize & 63] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64, standard or URL-safe, padded or not.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0;
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RISK: &str = "# Risk\n\nIntro line.\n\n## Scoring\n\nRisks are scored as likelihood x impact.\n\n```sh\n# not a heading\n```\n\n### Bands\n\n1-6 accept.\n\n#### Detail\n\nStill in bands.\n## Records\n\nRecord each risk.";

    #[test]
    fn chunks_split_at_headings_outside_fences_and_cover_every_line() {
        let c = chunk_markdown("standards/risk.md", RISK);
        let spans: Vec<(usize, usize, &str)> = c
            .iter()
            .map(|c| (c.start_line, c.end_line, c.heading.as_str()))
            .collect();
        assert_eq!(
            spans,
            vec![
                (1, 4, "Risk"),
                (5, 12, "Risk > Scoring"),
                (13, 19, "Risk > Scoring > Bands"),
                (20, 22, "Risk > Records"),
            ]
        );
        assert!(c.iter().all(|c| c.title == "Risk"));
        let joined: Vec<&str> = c.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(joined.join("\n"), RISK);
    }

    #[test]
    fn a_preamble_is_its_own_chunk_and_the_path_is_the_fallback_title() {
        let c = chunk_markdown("a.md", "front\n\n## One\nx");
        assert_eq!(c.len(), 2);
        assert_eq!(
            (c[0].start_line, c[0].end_line, c[0].heading.as_str()),
            (1, 2, "")
        );
        assert_eq!(c[0].title, "a.md");
        assert_eq!(c[1].heading, "One");
        assert!(chunk_markdown("e.md", "").is_empty());
    }

    #[test]
    fn a_long_section_splits_into_windows() {
        let body = "word ".repeat(100);
        let text = format!("# T\n{}", vec![body.as_str(); 200].join("\n"));
        let c = chunk_markdown("l.md", &text);
        assert!(c.len() > 1);
        assert!(c.iter().all(|c| c.text.len() <= MAX_CHUNK_BYTES));
        assert_eq!(c.last().unwrap().end_line, 201);
        for w in c.windows(2) {
            assert_eq!(w[0].end_line + 1, w[1].start_line);
        }
    }

    #[test]
    fn atx_headings_parse() {
        assert_eq!(atx_heading("## Foo ##"), Some((2, "Foo".into())));
        assert_eq!(atx_heading("## C#"), Some((2, "C#".into())));
        assert_eq!(atx_heading("#tag"), None);
        assert_eq!(atx_heading("    # code"), None);
    }

    #[test]
    fn globs_match_segments() {
        assert!(glob_match("AGENTS.md", "AGENTS.md"));
        assert!(glob_match("standards/*.md", "standards/risk.md"));
        assert!(!glob_match("standards/*.md", "standards/x/risk.md"));
        assert!(glob_match("**/*.md", "README.md"));
        assert!(glob_match("**/*.md", "a/b/c.md"));
        assert!(glob_match("company/**", "company/x/y.md"));
        assert!(!glob_match("company/*.md", "AGENTS.md"));
        assert!(glob_match("r?sk.md", "risk.md"));
    }

    #[test]
    fn the_best_line_holds_the_most_query_words() {
        let text = "## Scoring\n\nA risk is noted.\nRisks are scored as likelihood x impact.";
        assert_eq!(
            best_line(text, 10, "risk scoring"),
            (13, "Risks are scored as likelihood x impact.".into())
        );
        assert_eq!(
            best_line(text, 10, "\"nothing\" -risk"),
            (10, "## Scoring".into())
        );
        let tie = "Score each risk.\nRisk scoring grid.";
        assert_eq!(best_line(tie, 1, "risk scoring").0, 2);
    }

    #[test]
    fn ids_are_one_segment() {
        assert_eq!(
            document_id(None, "standards/risk.md", 5),
            "standards~risk.md:L5"
        );
        assert_eq!(document_id(Some("law"), "AGENTS.md", 1), "law:AGENTS.md:L1");
    }

    #[test]
    fn base64_round_trips() {
        for s in ["", "a", "ab", "abc", "abcd", "{\"text\":\"línea\"}"] {
            let e = b64_encode(s.as_bytes());
            assert_eq!(e.len() % 4, 0);
            assert_eq!(b64_decode(&e).unwrap(), s.as_bytes());
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_decode("TWE").unwrap(), b"Ma");
    }
}
