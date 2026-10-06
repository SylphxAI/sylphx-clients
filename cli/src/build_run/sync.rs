//! The workspace sync of `sylphx build run`: which files make up the tree,
//! their digests, the manifest a warm workspace keeps, the diff between the
//! two, and the tarballs and lists that carry the diff.
//!
//! The manifest is one line per file, `<sha256 hex> <mode> <path>`, after a
//! header line. A path is escaped so it stays on one line (`\\` and `\n`), and
//! the path always starts at byte [`PATH_AT`], so the guest can merge a diff
//! into its manifest with `awk` alone (see [`APPLY`]).

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// The first line of every manifest; a manifest without it is not trusted.
pub const HEADER: &str = "sylphx-manifest 1";
/// 64 hex digits, a space, six mode digits, a space: the path starts here
/// (1-based, as `awk`'s `substr` counts).
pub const PATH_AT: usize = 73;
/// A tarball holds at most this many bytes of file content, so one upload
/// stays well inside what a guest accepts in one request.
pub const TARBALL_RAW: u64 = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mode {
    File,
    Exec,
    Link,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::File => "100644",
            Mode::Exec => "100755",
            Mode::Link => "120000",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "100644" => Some(Mode::File),
            "100755" => Some(Mode::Exec),
            "120000" => Some(Mode::Link),
            _ => None,
        }
    }
}

/// One file of the tree: its content digest (of the link target for a link)
/// and its mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub digest: String,
    pub mode: Mode,
    /// Content bytes (the target's length for a link); not in the manifest.
    pub size: u64,
}

/// Path (with `/` separators, relative to the work-tree root) to entry.
pub type Tree = BTreeMap<String, Entry>;

/// What one sync sends.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// No usable manifest on the workspace: the tree is replaced whole.
    pub full: bool,
    /// Added or changed paths, sent in tarballs.
    pub upload: Vec<String>,
    /// Paths the workspace has and the tree no longer does.
    pub remove: Vec<String>,
}

impl Plan {
    pub fn upload_bytes(&self, tree: &Tree) -> u64 {
        self.upload
            .iter()
            .filter_map(|p| tree.get(p))
            .map(|e| e.size)
            .sum()
    }
}

/// Whether `path` is the guest's own git directory or inside it: the
/// workspace keeps its own repository there (see `git_tree!` in the run
/// script), so the sync never sends or deletes it.
pub fn in_git_dir(path: &str) -> bool {
    path == ".git" || path.starts_with(".git/")
}

/// The diff between this tree and what the workspace last materialized.
/// Paths in `.git` are never part of it.
pub fn plan(local: &Tree, remote: Option<&Tree>) -> Plan {
    let Some(remote) = remote else {
        return Plan {
            full: true,
            upload: local.keys().filter(|p| !in_git_dir(p)).cloned().collect(),
            remove: Vec::new(),
        };
    };
    let upload = local
        .iter()
        .filter(|(p, _)| !in_git_dir(p))
        .filter(|(p, e)| {
            remote
                .get(*p)
                .is_none_or(|r| r.digest != e.digest || r.mode != e.mode)
        })
        .map(|(p, _)| p.clone())
        .collect();
    let remove = remote
        .keys()
        .filter(|p| !local.contains_key(*p) && !in_git_dir(p))
        .cloned()
        .collect();
    Plan {
        full: false,
        upload,
        remove,
    }
}

/// `\` and newline escaped, so a path is one manifest line.
pub fn escape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for c in path.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out
}

fn unescape(s: &str) -> Option<String> {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c == '\\' {
            match it.next()? {
                '\\' => out.push('\\'),
                'n' => out.push('\n'),
                _ => return None,
            }
        } else {
            out.push(c);
        }
    }
    Some(out)
}

/// One manifest line for `path`.
pub fn line(path: &str, e: &Entry) -> String {
    format!("{} {} {}\n", e.digest, e.mode.as_str(), escape(path))
}

/// The whole manifest of a tree.
pub fn render(tree: &Tree) -> String {
    let mut out = format!("{HEADER}\n");
    for (p, e) in tree {
        out.push_str(&line(p, e));
    }
    out
}

/// The manifest a workspace answered; `None` when it is missing a header or
/// any line is malformed, which makes the next sync a full one.
pub fn parse(text: &str) -> Option<Tree> {
    let mut lines = text.split('\n');
    if lines.next()? != HEADER {
        return None;
    }
    let mut tree = Tree::new();
    for l in lines {
        if l.is_empty() {
            continue;
        }
        if l.len() < PATH_AT || !l.is_char_boundary(PATH_AT - 1) {
            return None;
        }
        let (head, path) = l.split_at(PATH_AT - 1);
        let digest = head.get(..64)?;
        if !digest.bytes().all(|b| b.is_ascii_hexdigit()) || head.as_bytes()[64] != b' ' {
            return None;
        }
        let mode = Mode::parse(head.get(65..71)?)?;
        if head.as_bytes()[71] != b' ' {
            return None;
        }
        // A later line for the same path wins: the guest appends a diff's
        // lines after the kept ones.
        tree.insert(
            unescape(path)?,
            Entry {
                digest: digest.to_ascii_lowercase(),
                mode,
                size: 0,
            },
        );
    }
    Some(tree)
}

/// The lists the guest applies besides the tarballs.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Lists {
    /// Escaped paths, one per line: manifest lines to drop (changed or removed).
    pub drop: String,
    /// Manifest lines to append (the header first on a full sync).
    pub add: String,
    /// Raw paths, NUL-terminated: files to delete from the tree.
    pub remove: Vec<u8>,
}

pub fn lists(tree: &Tree, plan: &Plan) -> Lists {
    let mut l = Lists::default();
    if plan.full {
        l.add = render(tree);
        return l;
    }
    for p in plan.upload.iter().chain(&plan.remove) {
        l.drop.push_str(&escape(p));
        l.drop.push('\n');
    }
    for p in &plan.upload {
        if let Some(e) = tree.get(p) {
            l.add.push_str(&line(p, e));
        }
    }
    for p in &plan.remove {
        l.remove.extend_from_slice(p.as_bytes());
        l.remove.push(0);
    }
    l
}

/// Applies one sync on the guest, as the guest user, with `sh`, `tar`,
/// `gzip`, `awk` and `xargs`. `$1` is the workspace, `$2` is 1 for a full
/// sync, 2 for a full sync that also empties the build caches (`--fresh`).
///
/// The manifest is moved aside first and written back last, so a sync that
/// is interrupted anywhere leaves no manifest and the next sync is full; a
/// manifest on the workspace always describes the tree exactly. Extracted
/// files get the time of extraction, so they are newer than any output built
/// from what they replace; files the sync does not touch keep their times,
/// which is what lets cargo reuse its fingerprints. The tree's `.git` (the
/// guest's own repository) is never swept, so a warm run only stages what
/// changed; a full sync replaces the tree, `.git` included.
pub const APPLY: &str = r#"set -eu
export LC_ALL=C
W=$1 FULL=$2
T=$W/tree S=$W/.sylphx
mkdir -p "$S"
if [ -f "$S/manifest" ]; then mv -f "$S/manifest" "$S/manifest.old"; fi
if [ "$FULL" != 0 ]; then
  rm -rf "$T" "$S/manifest.old"
  if [ "$FULL" = 2 ]; then rm -rf "$W/target" "$W/sccache"; fi
fi
mkdir -p "$T" "$W/target"
touch "$S/manifest.old" "$S/drop" "$S/add" "$S/remove"
if [ -s "$S/remove" ]; then
  (cd "$T" && xargs -0 rm -rf -- < "$S/remove")
  find "$T" -mindepth 1 -depth -type d -empty ! -path "$T/.git" ! -path "$T/.git/*" -delete
fi
for f in "$S"/in-*.tar.gz; do
  [ -e "$f" ] || continue
  tar -xzmf "$f" -C "$T"
  rm -f "$f"
done
if [ ! -e "$T/target" ] && [ ! -L "$T/target" ]; then ln -s ../target "$T/target"; fi
awk -v D="$S/drop" -v AT=73 'BEGIN { while ((getline l < D) > 0) d[l] = 1 } !(substr($0, AT) in d)' "$S/manifest.old" > "$S/manifest.next"
cat "$S/add" >> "$S/manifest.next"
mv -f "$S/manifest.next" "$S/manifest"
rm -f "$S/manifest.old" "$S/drop" "$S/add" "$S/remove"
"#;

/// The files a run sends: tracked and untracked-but-not-ignored, as
/// `git ls-files` lists them from the work-tree root, minus whatever
/// `.sylphxignore` (gitignore syntax) matches. Deleted files and submodules
/// are not files here and are skipped by [`scan`].
pub fn file_set(root: &Path) -> Result<Vec<String>, String> {
    let all = git_z(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )?;
    let ignore = root.join(".sylphxignore");
    let skip: BTreeSet<String> = if ignore.is_file() {
        git_z(
            root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "-i",
                "--exclude-from=.sylphxignore",
            ],
        )?
        .into_iter()
        .collect()
    } else {
        BTreeSet::new()
    };
    let set: BTreeSet<String> = all.into_iter().filter(|p| !skip.contains(p)).collect();
    Ok(set.into_iter().collect())
}

fn git_z(root: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    out.stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| {
            String::from_utf8(s.to_vec())
                .map_err(|_| format!("a path is not UTF-8: {}", String::from_utf8_lossy(s)))
        })
        .collect()
}

/// What a file's digest was computed from, so an unchanged file is not read
/// again (`git`'s own index does the same).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    size: u64,
    mtime_ns: i128,
    ctime_ns: i128,
    ino: u64,
}

fn stamp(md: &std::fs::Metadata) -> Stamp {
    let ns = |t: std::io::Result<SystemTime>| -> i128 {
        t.ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or(0)
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Stamp {
            size: md.len(),
            mtime_ns: ns(md.modified()),
            ctime_ns: md.ctime() as i128 * 1_000_000_000 + md.ctime_nsec() as i128,
            ino: md.ino(),
        }
    }
    #[cfg(not(unix))]
    {
        Stamp {
            size: md.len(),
            mtime_ns: ns(md.modified()),
            ctime_ns: 0,
            ino: 0,
        }
    }
}

/// Digests keyed by path and [`Stamp`], kept between runs.
#[derive(Debug, Default)]
pub struct StatCache {
    map: BTreeMap<String, (Stamp, Entry)>,
}

/// A file changed this recently may change again within the clock's
/// resolution without a new stamp, so its digest is not kept.
const RACY: Duration = Duration::from_secs(2);

impl StatCache {
    pub fn load(path: &Path) -> Self {
        let mut c = StatCache::default();
        let Ok(text) = std::fs::read_to_string(path) else {
            return c;
        };
        let mut lines = text.split('\n');
        if lines.next() != Some("sylphx-cas-index 1") {
            return c;
        }
        for l in lines {
            let mut f = l.splitn(7, ' ');
            let (Some(size), Some(m), Some(ct), Some(ino), Some(digest), Some(mode), Some(p)) = (
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
                f.next(),
            ) else {
                continue;
            };
            let (Ok(size), Ok(m), Ok(ct), Ok(ino), Some(mode), Some(p)) = (
                size.parse(),
                m.parse(),
                ct.parse(),
                ino.parse(),
                Mode::parse(mode),
                unescape(p),
            ) else {
                continue;
            };
            let st = Stamp {
                size,
                mtime_ns: m,
                ctime_ns: ct,
                ino,
            };
            c.map.insert(
                p,
                (
                    st,
                    Entry {
                        digest: digest.into(),
                        mode,
                        size,
                    },
                ),
            );
        }
        c
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut out = String::from("sylphx-cas-index 1\n");
        for (p, (s, e)) in &self.map {
            out.push_str(&format!(
                "{} {} {} {} {} {} {}\n",
                s.size,
                s.mtime_ns,
                s.ctime_ns,
                s.ino,
                e.digest,
                e.mode.as_str(),
                escape(p)
            ));
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, path)
    }
}

/// Hashes the files of `paths` under `root` (reusing `cache` where a file's
/// stamp is unchanged) and updates the cache to exactly this tree.
pub fn scan(root: &Path, paths: &[String], cache: &mut StatCache) -> Result<Tree, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0);
    let mut tree = Tree::new();
    let mut fresh = BTreeMap::new();
    for p in paths {
        let full = root.join(p);
        let Ok(md) = std::fs::symlink_metadata(&full) else {
            // Listed by the index but deleted in the work tree.
            continue;
        };
        let ft = md.file_type();
        if !(ft.is_file() || ft.is_symlink()) {
            // A submodule or other non-file.
            continue;
        }
        let st = stamp(&md);
        let cached = cache.map.get(p).filter(|(s, _)| *s == st).map(|(_, e)| e);
        let entry = match cached {
            Some(e) => e.clone(),
            None => hash(&full, &md).map_err(|e| format!("{p}: {e}"))?,
        };
        if now - st.mtime_ns > RACY.as_nanos() as i128 {
            fresh.insert(p.clone(), (st, entry.clone()));
        }
        tree.insert(p.clone(), entry);
    }
    cache.map = fresh;
    Ok(tree)
}

fn hash(path: &Path, md: &std::fs::Metadata) -> std::io::Result<Entry> {
    let mut h = Sha256::new();
    if md.file_type().is_symlink() {
        let target = link_target(path)?;
        h.update(target.as_bytes());
        return Ok(Entry {
            digest: hex(&h.finalize()),
            mode: Mode::Link,
            size: target.len() as u64,
        });
    }
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        size += n as u64;
        h.update(&buf[..n]);
    }
    Ok(Entry {
        digest: hex(&h.finalize()),
        mode: if executable(md) {
            Mode::Exec
        } else {
            Mode::File
        },
        size,
    })
}

fn link_target(path: &Path) -> std::io::Result<String> {
    let t = std::fs::read_link(path)?;
    t.to_str()
        .map(|s| s.replace('\\', "/"))
        .ok_or_else(|| std::io::Error::other("the link target is not UTF-8"))
}

#[cfg(unix)]
fn executable(md: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_: &std::fs::Metadata) -> bool {
    false
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// The upload of `paths` as gzip-compressed tarballs, each holding at most
/// [`TARBALL_RAW`] bytes of content (a larger file goes alone). A file that
/// changed since it was hashed is sent as it is now; the next sync corrects
/// the manifest's digest of it.
pub fn tarballs(root: &Path, paths: &[String], tree: &Tree) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::new();
    let mut tar = Tar::new();
    let (mut raw, mut entries) = (0u64, 0usize);
    for p in paths {
        let Some(e) = tree.get(p) else { continue };
        if entries > 0 && raw + e.size > TARBALL_RAW {
            out.push(tar.finish()?);
            tar = Tar::new();
            (raw, entries) = (0, 0);
        }
        let full = root.join(p);
        match e.mode {
            Mode::Link => {
                let target = link_target(&full).map_err(|err| format!("{p}: {err}"))?;
                tar.link(p, &target)?;
            }
            mode => {
                let bytes = std::fs::read(&full).map_err(|err| format!("{p}: {err}"))?;
                tar.file(p, mode == Mode::Exec, &bytes)?;
            }
        }
        raw += e.size;
        entries += 1;
    }
    if entries > 0 {
        out.push(tar.finish()?);
    }
    Ok(out)
}

/// A minimal POSIX (pax) tar writer: regular files and symlinks, long names
/// and link targets through pax `path` and `linkpath` records.
struct Tar {
    gz: flate2::write::GzEncoder<Vec<u8>>,
}

impl Tar {
    fn new() -> Self {
        Self {
            gz: flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()),
        }
    }

    fn file(&mut self, path: &str, exec: bool, bytes: &[u8]) -> Result<(), String> {
        let mode = if exec { 0o755 } else { 0o644 };
        self.entry(path, b'0', mode, bytes.len() as u64, "")?;
        self.write(bytes)?;
        self.pad(bytes.len() as u64)
    }

    fn link(&mut self, path: &str, target: &str) -> Result<(), String> {
        self.entry(path, b'2', 0o777, 0, target)
    }

    fn entry(
        &mut self,
        path: &str,
        kind: u8,
        mode: u32,
        size: u64,
        link: &str,
    ) -> Result<(), String> {
        let mut pax = String::new();
        if path.len() > 100 || !path.is_ascii() {
            pax.push_str(&record("path", path));
        }
        if link.len() > 100 || !link.is_ascii() {
            pax.push_str(&record("linkpath", link));
        }
        if !pax.is_empty() {
            let h = header("././@PaxHeader", b'x', 0o644, pax.len() as u64, "");
            self.write(&h)?;
            self.write(pax.as_bytes())?;
            self.pad(pax.len() as u64)?;
        }
        let h = header(path, kind, mode, size, link);
        self.write(&h)
    }

    fn write(&mut self, b: &[u8]) -> Result<(), String> {
        self.gz.write_all(b).map_err(|e| format!("tar: {e}"))
    }

    fn pad(&mut self, len: u64) -> Result<(), String> {
        let rem = (len % 512) as usize;
        if rem != 0 {
            self.write(&[0u8; 512][..512 - rem])?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Vec<u8>, String> {
        self.write(&[0u8; 1024])?;
        self.gz.finish().map_err(|e| format!("tar: {e}"))
    }
}

/// One pax record: `"<len> <key>=<value>\n"`, where `len` counts itself.
fn record(key: &str, value: &str) -> String {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while len.to_string().len() + body.len() != len {
        len = len.to_string().len() + body.len();
    }
    format!("{len}{body}")
}

/// One ustar header block. Names longer than its fields are cut here and
/// carried whole by a preceding pax record.
fn header(path: &str, kind: u8, mode: u32, size: u64, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    let put = |h: &mut [u8; 512], at: usize, len: usize, s: &[u8]| {
        let n = s.len().min(len);
        h[at..at + n].copy_from_slice(&s[..n]);
    };
    let octal = |h: &mut [u8; 512], at: usize, len: usize, v: u64| {
        let s = format!("{v:0width$o}", width = len - 1);
        put(h, at, len - 1, s.as_bytes());
    };
    put(&mut h, 0, 100, ascii_cut(path).as_bytes());
    octal(&mut h, 100, 8, mode as u64);
    octal(&mut h, 108, 8, 0);
    octal(&mut h, 116, 8, 0);
    octal(&mut h, 124, 12, size);
    octal(&mut h, 136, 12, 0);
    h[156] = kind;
    put(&mut h, 157, 100, ascii_cut(link).as_bytes());
    put(&mut h, 257, 6, b"ustar\0");
    put(&mut h, 263, 2, b"00");
    // The checksum is computed with its own field as spaces.
    h[148..156].copy_from_slice(b"        ");
    let sum: u32 = h.iter().map(|b| *b as u32).sum();
    let s = format!("{sum:06o}\0 ");
    h[148..156].copy_from_slice(s.as_bytes());
    h
}

fn ascii_cut(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii() { c } else { '_' })
        .take(100)
        .collect()
}

/// The commit checked out in the work tree at `root`, if it has one; the
/// guest names it in its own commit and exports it as
/// `SYLPHX_BUILD_GIT_HEAD`.
pub fn head(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .output()
        .ok()?;
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !sha.is_empty()).then_some(sha)
}

/// The work-tree root that encloses `dir`, and `dir` relative to it.
pub fn work_tree(dir: &Path) -> Result<(PathBuf, String), String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-toplevel", "--show-prefix"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!("{} is not inside a git work tree", dir.display()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let root = PathBuf::from(lines.next().unwrap_or_default());
    let prefix = lines.next().unwrap_or_default().trim_end_matches('/');
    Ok((root, prefix.to_string()))
}

/// Where the stat cache of this work tree lives (inside its git dir).
pub fn cache_path(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "sylphx/cas-index",
        ])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// The repository this tree is a checkout of: the normalized origin URL and
/// the root commit. Any branch or worktree of it shares warm workspaces.
pub fn repo_key(root: &Path) -> String {
    let git = |args: &[&str]| -> String {
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    };
    let origin = normalize_origin(&git(&["remote", "get-url", "origin"]));
    let first = git(&["rev-list", "--max-parents=0", "HEAD"]);
    let first = first.lines().last().unwrap_or_default();
    let mut key = sha256_hex(format!("{origin}\n{first}").as_bytes());
    key.truncate(20);
    key
}

/// `git@github.com:o/r.git`, `https://github.com/o/r` and `ssh://…` alike
/// become `github.com/o/r`.
pub fn normalize_origin(url: &str) -> String {
    let mut s = url.trim().to_ascii_lowercase();
    for scheme in ["https://", "http://", "ssh://", "git://"] {
        if let Some(rest) = s.strip_prefix(scheme) {
            s = rest.to_string();
        }
    }
    if let Some((user, rest)) = s.split_once('@') {
        if !user.contains('/') {
            s = rest.to_string();
        }
    }
    if let Some((host, path)) = s.split_once(':') {
        if !host.contains('/') {
            let path = path.trim_start_matches(|c: char| c.is_ascii_digit());
            s = format!("{host}/{}", path.trim_start_matches('/'));
        }
    }
    s.trim_end_matches('/').trim_end_matches(".git").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(digest: char, mode: Mode) -> Entry {
        Entry {
            digest: std::iter::repeat_n(digest, 64).collect(),
            mode,
            size: 1,
        }
    }

    fn tree(items: &[(&str, char, Mode)]) -> Tree {
        items
            .iter()
            .map(|(p, d, m)| (p.to_string(), e(*d, *m)))
            .collect()
    }

    /// A scratch directory removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let p = std::env::temp_dir().join(format!(
                "sylphx-build-sync-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write(root: &Path, p: &str, content: &str) {
        let full = root.join(p);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, content).unwrap();
    }

    fn git(root: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn a_missing_manifest_sends_everything() {
        let local = tree(&[("a", 'a', Mode::File), ("b/c", 'b', Mode::Exec)]);
        let p = plan(&local, None);
        assert!(p.full);
        assert_eq!(p.upload, vec!["a", "b/c"]);
        assert!(p.remove.is_empty());
    }

    #[test]
    fn only_changed_files_are_sent_and_removed_ones_deleted() {
        let remote = tree(&[
            ("same", 'a', Mode::File),
            ("edited", 'b', Mode::File),
            ("chmod", 'c', Mode::File),
            ("gone", 'd', Mode::File),
        ]);
        let local = tree(&[
            ("same", 'a', Mode::File),
            ("edited", 'e', Mode::File),
            ("chmod", 'c', Mode::Exec),
            ("new", 'f', Mode::Link),
        ]);
        let p = plan(&local, Some(&remote));
        assert!(!p.full);
        assert_eq!(p.upload, vec!["chmod", "edited", "new"]);
        assert_eq!(p.remove, vec!["gone"]);
        // An unchanged tree sends nothing.
        let p = plan(&local, Some(&local));
        assert_eq!(p, Plan::default());
    }

    #[test]
    fn the_manifest_round_trips_odd_paths() {
        let local = tree(&[
            ("plain.rs", 'a', Mode::File),
            ("with space/and\ttab", 'b', Mode::Exec),
            ("new\nline", 'c', Mode::File),
            ("back\\slash", 'd', Mode::Link),
            ("ünïcode/文件", 'e', Mode::File),
        ]);
        let text = render(&local);
        assert!(text.starts_with("sylphx-manifest 1\n"));
        assert_eq!(text.lines().count(), 1 + local.len(), "one line per file");
        let back = parse(&text).unwrap();
        assert_eq!(
            back.iter()
                .map(|(p, e)| (p.clone(), e.digest.clone(), e.mode))
                .collect::<Vec<_>>(),
            local
                .iter()
                .map(|(p, e)| (p.clone(), e.digest.clone(), e.mode))
                .collect::<Vec<_>>()
        );
        // Every path starts at the column the guest's awk cuts at.
        for l in text.lines().skip(1) {
            assert_eq!(&l[64..65], " ");
            assert_eq!(&l[71..72], " ");
        }
    }

    #[test]
    fn a_damaged_manifest_is_not_trusted() {
        let good = render(&tree(&[("a", 'a', Mode::File)]));
        assert!(parse(&good).is_some());
        assert!(parse("").is_none());
        assert!(parse(&good.replace(HEADER, "sylphx-manifest 0")).is_none());
        assert!(parse(&good.replace("100644", "100600")).is_none());
        assert!(parse(&format!("{good}short line\n")).is_none());
        let bad_hex = format!("{HEADER}\n{} 100644 a\n", "z".repeat(64));
        assert!(parse(&bad_hex).is_none());
    }

    #[test]
    fn a_later_line_for_a_path_wins() {
        let text = format!(
            "{HEADER}\n{} 100644 a\n{} 100755 a\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let t = parse(&text).unwrap();
        assert_eq!(t["a"].digest, "b".repeat(64));
        assert_eq!(t["a"].mode, Mode::Exec);
    }

    #[test]
    fn pax_records_count_their_own_length() {
        for v in ["x", &"y".repeat(90), &"z".repeat(5000)] {
            let r = record("path", v);
            let (n, _) = r.split_once(' ').unwrap();
            assert_eq!(n.parse::<usize>().unwrap(), r.len(), "{r}");
        }
    }

    #[test]
    fn origins_normalize_to_one_key() {
        for u in [
            "git@github.com:SylphxAI/cloud.git",
            "https://github.com/SylphxAI/cloud",
            "https://github.com/SylphxAI/cloud.git/",
            "ssh://git@github.com/SylphxAI/cloud.git",
            "https://user@github.com/SylphxAI/cloud",
        ] {
            assert_eq!(normalize_origin(u), "github.com/sylphxai/cloud", "{u}");
        }
    }

    /// Runs [`APPLY`] the way the guest does: the tarballs and lists in
    /// `$W/.sylphx`, then `sh -c APPLY`.
    fn apply(ws: &Path, root: &Path, local: &Tree, p: &Plan, full: u8) -> bool {
        let s = ws.join(".sylphx");
        std::fs::create_dir_all(&s).unwrap();
        for (i, t) in tarballs(root, &p.upload, local).unwrap().iter().enumerate() {
            std::fs::write(s.join(format!("in-{i:04}.tar.gz")), t).unwrap();
        }
        let l = lists(local, p);
        std::fs::write(s.join("drop"), &l.drop).unwrap();
        std::fs::write(s.join("add"), &l.add).unwrap();
        std::fs::write(s.join("remove"), &l.remove).unwrap();
        Command::new("sh")
            .args(["-c", APPLY, "apply"])
            .arg(ws)
            .arg(full.to_string())
            .status()
            .unwrap()
            .success()
    }

    fn remote(ws: &Path) -> Option<Tree> {
        std::fs::read_to_string(ws.join(".sylphx/manifest"))
            .ok()
            .and_then(|t| parse(&t))
    }

    fn mtime(p: &Path) -> SystemTime {
        std::fs::symlink_metadata(p).unwrap().modified().unwrap()
    }

    /// The whole loop against a real `sh`, `tar` and `awk`: a full sync, then a
    /// diff that edits, adds, deletes and replaces a directory with a file,
    /// after which the workspace's manifest equals the local tree, unchanged
    /// files keep their times, and the tree has exactly the local files.
    #[cfg(unix)]
    #[test]
    fn the_guest_applies_a_diff_and_keeps_untouched_mtimes() {
        let src = Scratch::new("src");
        let ws = Scratch::new("ws");
        let root = &src.0;
        git(root, &["init", "-q"]);
        write(root, "keep.rs", "fn keep() {}\n");
        write(root, "edit.rs", "fn v1() {}\n");
        write(root, "dir/inner.txt", "inner\n");
        write(root, "gone.txt", "bye\n");
        write(root, "ignored.log", "log\n");
        write(root, ".gitignore", "*.log\n");
        let deep = format!("{}/long.rs", "d".repeat(120));
        write(root, &deep, "// long path\n");
        write(root, "sp ace.txt", "space\n");
        std::os::unix::fs::symlink("keep.rs", root.join("link.rs")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            write(root, "run.sh", "#!/bin/sh\n");
            std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        git(root, &["add", "-A"]);

        let mut cache = StatCache::default();
        let files = file_set(root).unwrap();
        assert!(
            !files.iter().any(|f| f == "ignored.log"),
            "ignored files stay home"
        );
        let local = scan(root, &files, &mut cache).unwrap();
        let p = plan(&local, remote(&ws.0).as_ref());
        assert!(p.full);
        assert!(apply(&ws.0, root, &local, &p, 1));
        let tree_dir = ws.0.join("tree");
        assert_eq!(
            remote(&ws.0).map(|t| t.keys().cloned().collect::<Vec<_>>()),
            Some(local.keys().cloned().collect())
        );
        assert_eq!(
            std::fs::read_to_string(tree_dir.join(&deep)).unwrap(),
            "// long path\n"
        );
        assert_eq!(
            std::fs::read_link(tree_dir.join("link.rs")).unwrap(),
            PathBuf::from("keep.rs")
        );
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(tree_dir.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "the executable bit survives");
        }
        assert!(std::fs::symlink_metadata(tree_dir.join("target"))
            .unwrap()
            .file_type()
            .is_symlink());

        // Let the clock move past the first sync's times.
        std::thread::sleep(Duration::from_millis(1100));
        let keep_before = mtime(&tree_dir.join("keep.rs"));
        let edit_before = mtime(&tree_dir.join("edit.rs"));

        write(root, "edit.rs", "fn v2() {}\n");
        write(root, "added.rs", "fn added() {}\n");
        std::fs::remove_file(root.join("gone.txt")).unwrap();
        std::fs::remove_dir_all(root.join("dir")).unwrap();
        write(root, "dir", "now a file\n");
        let files = file_set(root).unwrap();
        let local = scan(root, &files, &mut cache).unwrap();
        let p = plan(&local, remote(&ws.0).as_ref());
        assert!(!p.full);
        assert_eq!(p.upload, vec!["added.rs", "dir", "edit.rs"]);
        assert_eq!(p.remove, vec!["dir/inner.txt", "gone.txt"]);
        // Only the diff travels: a few hundred bytes, not the tree.
        let sent: usize = tarballs(root, &p.upload, &local)
            .unwrap()
            .iter()
            .map(Vec::len)
            .sum();
        assert!(sent < 2048, "{sent} bytes");
        assert!(apply(&ws.0, root, &local, &p, 0));

        let after = remote(&ws.0).unwrap();
        assert_eq!(
            after
                .iter()
                .map(|(k, v)| (k.clone(), v.digest.clone(), v.mode))
                .collect::<Vec<_>>(),
            local
                .iter()
                .map(|(k, v)| (k.clone(), v.digest.clone(), v.mode))
                .collect::<Vec<_>>(),
            "the workspace manifest is the local tree"
        );
        assert_eq!(
            mtime(&tree_dir.join("keep.rs")),
            keep_before,
            "untouched files keep their mtime"
        );
        assert!(
            mtime(&tree_dir.join("edit.rs")) > edit_before,
            "a changed file is newer than before"
        );
        assert_eq!(
            std::fs::read_to_string(tree_dir.join("edit.rs")).unwrap(),
            "fn v2() {}\n"
        );
        assert_eq!(
            std::fs::read_to_string(tree_dir.join("dir")).unwrap(),
            "now a file\n"
        );
        assert!(!tree_dir.join("gone.txt").exists());

        // Nothing changed: nothing is sent and the manifest stays the same.
        let local2 = scan(root, &file_set(root).unwrap(), &mut cache).unwrap();
        assert_eq!(plan(&local2, Some(&after)), Plan::default());
    }

    #[test]
    fn the_guests_git_dir_is_never_sent_or_removed() {
        let remote = tree(&[(".git/HEAD", 'a', Mode::File), ("a", 'a', Mode::File)]);
        let local = tree(&[
            (".git/config", 'b', Mode::File),
            (".gitignore", 'c', Mode::File),
        ]);
        let p = plan(&local, Some(&remote));
        assert_eq!(p.upload, vec![".gitignore"]);
        assert_eq!(p.remove, vec!["a"]);
        assert_eq!(plan(&local, None).upload, vec![".gitignore"]);
    }

    /// The guest's repository in the tree, empty directories included,
    /// survives a warm sync that deletes files and sweeps empty directories.
    #[cfg(unix)]
    #[test]
    fn a_warm_sync_keeps_the_trees_git_dir() {
        let src = Scratch::new("src-git");
        let ws = Scratch::new("ws-git");
        let root = &src.0;
        git(root, &["init", "-q"]);
        write(root, "a.rs", "a\n");
        write(root, "dir/b.rs", "b\n");
        let mut cache = StatCache::default();
        let local = scan(root, &file_set(root).unwrap(), &mut cache).unwrap();
        assert!(apply(&ws.0, root, &local, &plan(&local, None), 1));
        let g = ws.0.join("tree/.git");
        std::fs::create_dir_all(g.join("refs/heads")).unwrap();
        write(&g, "HEAD", "ref: refs/heads/main\n");

        std::fs::remove_dir_all(root.join("dir")).unwrap();
        let local = scan(root, &file_set(root).unwrap(), &mut cache).unwrap();
        let p = plan(&local, remote(&ws.0).as_ref());
        assert_eq!(p.remove, vec!["dir/b.rs"]);
        assert!(apply(&ws.0, root, &local, &p, 0));
        assert!(!ws.0.join("tree/dir").exists(), "emptied directories go");
        assert!(g.join("refs/heads").is_dir(), "the git dir stays whole");
        assert!(g.join("HEAD").is_file());
    }

    /// A sync interrupted after the manifest was moved aside leaves no
    /// manifest, so the next plan is a full sync that replaces the tree.
    #[cfg(unix)]
    #[test]
    fn an_interrupted_sync_forces_a_full_sync() {
        let src = Scratch::new("src-int");
        let ws = Scratch::new("ws-int");
        let root = &src.0;
        git(root, &["init", "-q"]);
        write(root, "a.rs", "a\n");
        let mut cache = StatCache::default();
        let local = scan(root, &file_set(root).unwrap(), &mut cache).unwrap();
        assert!(apply(&ws.0, root, &local, &plan(&local, None), 1));
        assert!(remote(&ws.0).is_some());

        // The guest dies after its first step: the manifest is set aside and
        // a stray file from the half-applied sync is left in the tree.
        let s = ws.0.join(".sylphx");
        std::fs::rename(s.join("manifest"), s.join("manifest.old")).unwrap();
        write(&ws.0.join("tree"), "half-written.rs", "junk");
        assert!(remote(&ws.0).is_none());

        let p = plan(&local, remote(&ws.0).as_ref());
        assert!(p.full, "no manifest: send everything");
        assert!(apply(&ws.0, root, &local, &p, 1));
        assert!(
            !ws.0.join("tree/half-written.rs").exists(),
            "a full sync replaces the tree"
        );
        assert_eq!(remote(&ws.0).unwrap().len(), 1);
    }

    #[test]
    fn the_stat_cache_skips_unchanged_files_and_survives_a_reload() {
        let src = Scratch::new("cache");
        let root = &src.0;
        write(root, "a", "one");
        let old = SystemTime::now() - Duration::from_secs(60);
        let f = std::fs::File::options()
            .write(true)
            .open(root.join("a"))
            .unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        let paths = vec!["a".to_string()];
        let mut cache = StatCache::default();
        let t1 = scan(root, &paths, &mut cache).unwrap();
        let index = root.join("idx/cas-index");
        cache.save(&index).unwrap();
        let mut cache = StatCache::load(&index);
        assert_eq!(cache.map.len(), 1);
        // A poisoned cached digest proves the file was not read again.
        if let Some((_, e)) = cache.map.get_mut("a") {
            e.digest = "f".repeat(64);
        }
        let t2 = scan(root, &paths, &mut cache).unwrap();
        assert_eq!(t2["a"].digest, "f".repeat(64));
        // A content change moves the stamp, so the file is hashed again.
        write(root, "a", "two!");
        let t3 = scan(root, &paths, &mut cache).unwrap();
        assert_ne!(t3["a"].digest, t1["a"].digest);
        assert_eq!(t3["a"].digest, sha256_hex(b"two!"));
        // A just-written file is not kept, so a same-second edit is not missed.
        assert!(cache.map.is_empty());
    }

    #[test]
    fn sylphxignore_removes_files_from_the_set() {
        let src = Scratch::new("ignore");
        let root = &src.0;
        git(root, &["init", "-q"]);
        write(root, "keep.rs", "k");
        write(root, "fixtures/big.bin", "b");
        write(root, ".sylphxignore", "fixtures/\n");
        git(root, &["add", "-A"]);
        let files = file_set(root).unwrap();
        assert_eq!(files, vec![".sylphxignore", "keep.rs"]);
    }

    #[test]
    fn large_trees_split_into_several_tarballs() {
        let src = Scratch::new("split");
        let root = &src.0;
        write(root, "a", "x");
        write(root, "b", "y");
        let mut t = tree(&[("a", 'a', Mode::File), ("b", 'b', Mode::File)]);
        for e in t.values_mut() {
            e.size = TARBALL_RAW;
        }
        let parts = tarballs(root, &["a".into(), "b".into()], &t).unwrap();
        assert_eq!(parts.len(), 2);
        assert!(tarballs(root, &[], &t).unwrap().is_empty());
    }
}
