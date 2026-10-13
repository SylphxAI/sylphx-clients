//! Guest-side tree-pack fill. The gzip upload `.sylphx/fill.gz` contains
//! `sylphx-pack-fill 1`, then `pack <manifest-key> <tar.gz-CAS-digest>` followed
//! by that pack's ordinary sync manifest lines, repeated for each pack. Keys
//! and digests are lowercase SHA-256 hex; paths are safe relative regular-file
//! paths. No manifest header occurs inside a pack. Exit zero reports only
//! failed pack keys, one per line; a nonzero exit invalidates the entire fill.

use std::collections::BTreeSet;
use std::fmt::Write;

use super::sync;

/// Encode the pack inventory and exact expected file contents and modes.
pub fn list(tree: &sync::Tree, packs: &[(String, String, Vec<String>)]) -> String {
    let mut out = String::from("sylphx-pack-fill 1\n");
    for (key, digest, paths) in packs {
        let _ = writeln!(out, "pack {key} {digest}");
        for path in paths {
            // The planner supplies only paths present in this snapshot. Do not
            // silently omit a missing path and accidentally trust a partial pack.
            let e = &tree[path];
            // Eligible pack paths need no sync escaping; write directly rather
            // than allocating a temporary manifest string for every file.
            let _ = writeln!(out, "{} {} {path}", e.digest, e.mode.as_str());
        }
    }
    out
}

/// An unknown key or invalid UTF-8 makes the whole guest reply untrusted.
pub fn failed(stdout: &[u8], given: &[String]) -> Option<BTreeSet<String>> {
    let given: BTreeSet<&str> = given.iter().map(String::as_str).collect();
    let mut out = BTreeSet::new();
    for key in std::str::from_utf8(stdout)
        .ok()?
        .split('\n')
        .filter(|s| !s.is_empty())
    {
        if !given.contains(key) {
            return None;
        }
        out.insert(key.to_owned());
    }
    Some(out)
}

/// Run with `/bin/sh`: arguments are workspace and cluster store URL. The
/// bearer token arrives only through `SYLPHX_CAS_TOKEN`, is escaped into a
/// private curl config, removed from the environment, and cleaned on exit.
/// GNU tar lists *effective* names/types before extraction (including PAX
/// overrides); only the exact expected regular files may enter staging.
/// Each pack is fully checked in staging before APPLY publishes the workspace.
pub const FILL: &str = r#"set -eu
export LC_ALL=C
W=$(cd "$1" && pwd) BASE=$2
S=$W/.sylphx F=$W/.sylphx/fill
export F BASE
umask 077
rm -rf "$F"
mkdir -p "$F/tree" "$F/stage"
trap 'rm -f "$F/auth" "$F/curl"' 0
trap 'exit 3' 1 2 15
for c in curl sha256sum gzip awk tar xargs chmod; do
  command -v "$c" >/dev/null 2>&1 || { printf '%s is missing\n' "$c" >&2; exit 3; }
done
unset TAR_OPTIONS
gzip -dc "$S/fill.gz" > "$F/list" || exit 3
rm -f "$S/fill.gz"
# Parse the trusted client inventory defensively before using any field in
# paths, checksum lists or curl configs. Pack indexes, not keys, name staging.
awk '
BEGIN { F=ENVIRON["F"] }
function hex(s) { return length(s) == 64 && s !~ /[^0-9a-f]/ }
function safe(p, n,a,i) {
  if (p == "" || p ~ /[[:cntrl:]]/ || index(p,"\\") || index(p,"\"") || substr(p,1,1) == "/") return 0
  n = split(p,a,"/")
  for (i=1; i<=n; i++) if (a[i] == "" || a[i] == "." || a[i] == "..") return 0
  return 1
}
NR == 1 { if ($0 != "sylphx-pack-fill 1") exit 1; next }
$1 == "pack" {
  if (NF != 3 || !hex($2) || !hex($3) || keys[$2]++) exit 1
  if (id && !count) exit 1
  if (id) close(F "/stage/" id ".manifest")
  id++; count = 0
  print id " " $2 " " $3 > (F "/packs")
  next
}
{
  p = substr($0,73)
  if (!id || !hex(substr($0,1,64)) || substr($0,65,1) != " " || substr($0,72,1) != " " ||
      ($2 != "100644" && $2 != "100755") || !safe(p) || paths[p]++) exit 1
  print $0 > (F "/stage/" id ".manifest"); count++
}
END { if (!id || !count) exit 1 }
' "$F/list" || exit 3
# Neither the token nor server error bodies are ever forwarded to diagnostics.
awk 'BEGIN {
  t = ENVIRON["SYLPHX_CAS_TOKEN"]
  if (t == "" || t ~ /[[:cntrl:]]/) exit 1
  escaped = ""
  for (i=1; i<=length(t); i++) {
    c=substr(t,i,1); if (c == "\\" || c == "\"") escaped=escaped "\\"
    escaped=escaped c
  }
  print "header = \"Authorization: Bearer " escaped "\""
}' > "$F/auth" || exit 3
unset SYLPHX_CAS_TOKEN
# Escape the two non-manifest inputs as curl config strings too (workspace
# paths can contain spaces/quotes). HTTP(S) only; no redirect token leakage.
awk '
BEGIN { F=ENVIRON["F"]; B=ENVIRON["BASE"] }
function quote(s, i,c,r) {
  if (s ~ /[[:cntrl:]]/) exit 1
  r = ""
  for (i=1; i<=length(s); i++) {
    c=substr(s,i,1); if (c == "\\" || c == "\"") r=r "\\"; r=r c
  }
  return "\"" r "\""
}
{ print "url = " quote(B "/cas/" $3); print "output = " quote(F "/stage/" $1 ".tar.gz") }
' "$F/packs" > "$F/curl" || exit 3
curl --disable --parallel --parallel-max 16 --fail --silent --connect-timeout 10 --max-time 120 --proto '=http,https' -K "$F/auth" -K "$F/curl" >/dev/null 2>/dev/null || :
rm -f "$F/auth" "$F/curl"
: > "$F/ok"
: > "$F/failed"
: > "$F/stage/file"
: > "$F/stage/exec"
while read -r id key digest; do
  A=$F/stage/$id.tar.gz M=$F/stage/$id.manifest D=$F/tree
  valid=1
  printf '%s  %s\n' "$digest" "$id.tar.gz" > "$F/stage/archive.sum"
  (cd "$F/stage" && sha256sum -c --quiet archive.sum) >/dev/null 2>/dev/null || valid=0
  if [ "$valid" = 1 ]; then
    # GNU tar exposes effective PAX names and types in one listing. LC_ALL=C
    # fixes its five metadata fields; literal safe names preserve spaces/Unicode.
    # Retain absolute names so validation cannot normalize away a leading slash.
    tar --absolute-names --numeric-owner --quoting-style=literal -tvzf "$A" > "$F/stage/types" 2>/dev/null || valid=0
    awk -v I="$id" 'BEGIN {
      F=ENVIRON["F"]; M=F "/stage/" I ".manifest"
      while ((getline l < M) > 0) { expected[substr(l,73)]=1; want++ }
      close(M)
    } {
      if (substr($0,1,1) != "-") bad=1
      if (!sub(/^[^ ]+[ ]+[^ ]+[ ]+[^ ]+[ ]+[^ ]+[ ]+[^ ]+[ ]/, "")) bad=1
      if (!($0 in expected) || seen[$0]++) bad=1
      names++
    }
    END { if (bad || names != want) exit 1 }
    ' "$F/stage/types" || valid=0
  fi
  if [ "$valid" = 1 ]; then
    tar --no-same-owner --no-same-permissions -xzf "$A" -C "$D" 2>/dev/null || valid=0
  fi
  if [ "$valid" = 1 ]; then
    awk '{ print substr($0,1,64) "  " substr($0,73) }' "$M" > "$F/stage/files.sum"
    (cd "$D" && sha256sum -c --quiet "$F/stage/files.sum") >/dev/null 2>/dev/null || valid=0
  fi
  if [ "$valid" = 1 ]; then
    awk 'BEGIN { F=ENVIRON["F"] } {
      p=substr($0,73)
      dest=F "/stage/" ($2 == "100755" ? "exec" : "file")
      printf "%s%c", p, 0 >> dest
      print $0 >> (F "/ok")
    }' "$M" || exit 3
  else
    printf '%s\n' "$key" >> "$F/failed"
    # No workspace publication has happened. Remove only this pack's disjoint
    # trusted paths, including any files from an interrupted extraction.
    awk '{ printf "%s%c", substr($0,73), 0 }' "$M" > "$F/stage/remove"
    (cd "$F/tree" && xargs -0 -r rm -f -- < "$F/stage/remove") || exit 3
  fi
done < "$F/packs"
# Batch modes, no per-file fork or extra full-tree archive/copy. APPLY owns the
# single publication into the workspace after every pack has been checked.
(cd "$F/tree" && xargs -0 -r chmod 644 -- < "$F/stage/file" && xargs -0 -r chmod 755 -- < "$F/stage/exec") || exit 3
rm -rf "$F/stage"
cat "$F/failed"
"#;

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output};
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };
    use std::thread::{self, JoinHandle};
    use std::time::Duration;

    const TOKEN: &str = "pack-token-\"quoted\"-\\secret";

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sylphx-pack-fill-{}-{:x}",
                std::process::id(),
                sylphx_build_lease::guest::rand_u64()
            ));
            std::fs::create_dir_all(path.join(".sylphx")).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Real HTTP fixture: records authorization and the live auth config mode.
    struct Http {
        base: String,
        requests: Arc<Mutex<Vec<(String, String, u32)>>>,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }
    impl Http {
        fn new(ws: &Path, blobs: BTreeMap<String, Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let (log, done) = (requests.clone(), stop.clone());
            let auth = ws.join(".sylphx/fill/auth");
            let thread = thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(connection) => connection,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(2));
                            continue;
                        }
                        Err(e) => panic!("accept: {e}"),
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_secs(5)))
                        .unwrap();
                    let mut head = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !head.windows(4).any(|s| s == b"\r\n\r\n") {
                        let n = stream.read(&mut chunk).unwrap();
                        assert_ne!(n, 0);
                        head.extend_from_slice(&chunk[..n]);
                        assert!(head.len() < 65536);
                    }
                    let head = String::from_utf8(head).unwrap();
                    let path = head.split_whitespace().nth(1).unwrap().to_string();
                    let token = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("authorization")
                                .then(|| value.trim().to_string())
                        })
                        .unwrap_or_default();
                    let mode = std::fs::metadata(&auth).unwrap().permissions().mode() & 0o777;
                    log.lock().unwrap().push((path.clone(), token, mode));
                    let body = blobs.get(&path);
                    let (status, body) = match body {
                        Some(body) => (200, body.as_slice()),
                        None => (404, b"absent".as_slice()),
                    };
                    write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
            });
            Self {
                base,
                requests,
                stop,
                thread: Some(thread),
            }
        }
    }
    impl Drop for Http {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    fn entry(bytes: &[u8], mode: sync::Mode) -> sync::Entry {
        sync::Entry {
            digest: sync::sha256_hex(bytes),
            mode,
            size: bytes.len() as u64,
        }
    }

    fn archive(root: &Path, files: &[(&str, &[u8], sync::Mode)]) -> Vec<u8> {
        let mut tree = sync::Tree::new();
        for (path, bytes, mode) in files {
            let at = root.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            if *mode == sync::Mode::Link {
                std::os::unix::fs::symlink(std::str::from_utf8(bytes).unwrap(), &at).unwrap();
            } else {
                std::fs::write(&at, bytes).unwrap();
            }
            tree.insert((*path).to_owned(), entry(bytes, *mode));
        }
        sync::tarballs(root, &tree.keys().cloned().collect::<Vec<_>>(), &tree)
            .unwrap()
            .remove(0)
    }

    fn run(
        ws: &Path,
        base: &str,
        tree: &sync::Tree,
        packs: &[(String, String, Vec<String>)],
    ) -> Output {
        std::fs::write(
            ws.join(".sylphx/fill.gz"),
            super::super::store::gzip(&list(tree, packs)),
        )
        .unwrap();
        Command::new("/bin/sh")
            .args(["-c", FILL, "pack-fill"])
            .arg(ws)
            .arg(base)
            .env("SYLPHX_CAS_TOKEN", TOKEN)
            .env("NO_PROXY", "127.0.0.1")
            .env("no_proxy", "127.0.0.1")
            .output()
            .unwrap()
    }

    #[test]
    fn rejects_untrusted_failure_replies() {
        let keys = vec!["a".to_string(), "b".to_string()];
        assert_eq!(failed(b"b\nb\n", &keys), Some(["b".to_string()].into()));
        assert_eq!(failed(b"", &keys), Some(BTreeSet::new()));
        assert_eq!(failed(b"a\nunknown\n", &keys), None);
        assert_eq!(failed(b"\xff", &keys), None);
    }

    #[test]
    fn fills_exact_files_and_modes_and_falls_back_per_failed_pack() {
        use sync::Mode::{Exec, File};
        let scratch = Scratch::new();
        let ws = &scratch.0;
        let long = format!("nested/{}/unicode-é.rs", "long".repeat(30));
        let good_files = [
            ("bin/run tool", b"run\n".as_slice(), Exec),
            (long.as_str(), b"source\n".as_slice(), File),
        ];
        let good = archive(&ws.join("src-good"), &good_files);
        let bad = archive(
            &ws.join("src-bad"),
            &[
                ("bad/first", b"first", File),
                ("bad/second", b"wrong", File),
            ],
        );
        let extra = archive(
            &ws.join("src-extra"),
            &[
                ("extra/expected", b"yes", File),
                ("extra/unexpected", b"no", File),
            ],
        );
        let mut tree = sync::Tree::new();
        for (path, bytes, mode) in good_files {
            tree.insert(path.to_string(), entry(bytes, mode));
        }
        for (path, bytes) in [
            ("bad/first", b"first".as_slice()),
            ("bad/second", b"right".as_slice()),
            ("extra/expected", b"yes".as_slice()),
            ("missing", b"missing".as_slice()),
            ("corrupt", b"corrupt".as_slice()),
        ] {
            tree.insert(path.to_string(), entry(bytes, File));
        }
        let mut blobs = BTreeMap::new();
        let mut packs = Vec::new();
        for (key, bytes, paths, served) in [
            (
                "good",
                good,
                vec!["bin/run tool".to_string(), long.clone()],
                true,
            ),
            (
                "bad",
                bad,
                vec!["bad/first".into(), "bad/second".into()],
                true,
            ),
            ("extra", extra, vec!["extra/expected".into()], true),
            (
                "missing",
                b"missing archive".to_vec(),
                vec!["missing".into()],
                false,
            ),
            (
                "corrupt",
                b"original archive".to_vec(),
                vec!["corrupt".into()],
                true,
            ),
        ] {
            let digest = sync::sha256_hex(&bytes);
            if served {
                blobs.insert(
                    format!("/cas/{digest}"),
                    if key == "corrupt" {
                        b"damaged".to_vec()
                    } else {
                        bytes
                    },
                );
            }
            packs.push((sync::sha256_hex(key.as_bytes()), digest, paths));
        }
        let server = Http::new(ws, blobs);
        let output = run(ws, &server.base, &tree, &packs);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let keys: Vec<_> = packs.iter().map(|p| p.0.clone()).collect();
        assert_eq!(
            failed(&output.stdout, &keys),
            Some(keys[1..].iter().cloned().collect())
        );
        let fill = ws.join(".sylphx/fill");
        for (path, bytes, mode) in good_files {
            let at = fill.join("tree").join(path);
            assert_eq!(std::fs::read(&at).unwrap(), bytes);
            assert_eq!(
                std::fs::metadata(&at).unwrap().permissions().mode() & 0o777,
                if mode == Exec { 0o755 } else { 0o644 }
            );
        }
        assert_eq!(
            std::fs::read_to_string(fill.join("ok")).unwrap(),
            packs[0]
                .2
                .iter()
                .map(|p| sync::line(p, &tree[p]))
                .collect::<String>()
        );
        for path in tree.keys().filter(|p| !packs[0].2.contains(p)) {
            assert!(
                !fill.join("tree").join(path).exists(),
                "partial pack: {path}"
            );
        }
        assert!(!fill.join("tree/extra/unexpected").exists());
        assert!(!fill.join("auth").exists());
        assert!(!fill.join("stage").exists());
        assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
        let log = server.requests.lock().unwrap();
        assert_eq!(log.len(), packs.len());
        for (_, auth, mode) in log.iter() {
            assert_eq!(auth, &format!("Bearer {TOKEN}"));
            assert_eq!(*mode, 0o600);
        }
        drop(log);

        // The existing apply consumes successful pack lines and a direct
        // fallback tar for failures; its final manifest describes exact input.
        let filled: BTreeSet<_> = packs[0].2.iter().cloned().collect();
        let upload: Vec<_> = tree.keys().cloned().collect();
        let plan = sync::Plan {
            full: true,
            upload: upload.clone(),
            remove: Vec::new(),
        };
        let lists = sync::lists(&tree, &plan, &filled);
        let sd = ws.join(".sylphx");
        std::fs::write(sd.join("add"), lists.add).unwrap();
        std::fs::write(sd.join("drop"), lists.drop).unwrap();
        std::fs::write(sd.join("remove"), lists.remove).unwrap();
        let root = ws.join("fallback");
        for (path, bytes) in [
            ("bad/first", b"first".as_slice()),
            ("bad/second", b"right".as_slice()),
            ("extra/expected", b"yes".as_slice()),
            ("missing", b"missing".as_slice()),
            ("corrupt", b"corrupt".as_slice()),
        ] {
            let at = root.join(path);
            std::fs::create_dir_all(at.parent().unwrap()).unwrap();
            std::fs::write(at, bytes).unwrap();
        }
        let direct: Vec<_> = upload.into_iter().filter(|p| !filled.contains(p)).collect();
        std::fs::write(
            sd.join("in-0000.tar.gz"),
            sync::tarballs(&root, &direct, &tree).unwrap().remove(0),
        )
        .unwrap();
        let apply = Command::new("/bin/sh")
            .args(["-c", sync::APPLY, "apply"])
            .arg(ws)
            .args(["1", "1"])
            .output()
            .unwrap();
        assert!(
            apply.status.success(),
            "{}",
            String::from_utf8_lossy(&apply.stderr)
        );
        let manifest = sync::parse(&std::fs::read_to_string(sd.join("manifest")).unwrap()).unwrap();
        assert_eq!(manifest.len(), tree.len());
        for (path, expected) in &tree {
            assert_eq!(manifest[path].digest, expected.digest);
            assert_eq!(manifest[path].mode, expected.mode);
            assert_eq!(
                sync::sha256_hex(&std::fs::read(ws.join("tree").join(path)).unwrap()),
                expected.digest
            );
        }
    }

    #[test]
    fn poisoned_archives_never_escape_staging_or_publish_files() {
        use sync::Mode::{File, Link};
        let scratch = Scratch::new();
        let ws = &scratch.0;
        let source = ws.join("poison");
        std::fs::create_dir_all(&source).unwrap();
        let mut tree = sync::Tree::new();
        let mut packs = Vec::new();
        let mut blobs = BTreeMap::new();
        let outside = ws.join("escaped");
        for (i, kind) in [
            "parent",
            "absolute",
            "symlink",
            "directory",
            "duplicate",
            "newline",
            "invalid-gzip",
            "hardlink",
        ]
        .iter()
        .enumerate()
        {
            let path = format!("case{i}");
            let at = source.join(&path);
            tree.insert(path.clone(), entry(b"data", File));
            let mut expected = vec![path.clone()];
            let bytes = match *kind {
                "symlink" => archive(&ws.join("link-source"), &[(path.as_str(), b"data", Link)]),
                "invalid-gzip" => b"not a gzip or tar archive".to_vec(),
                "newline" => archive(
                    &ws.join("newline-source"),
                    &[(format!("{path}\nother").as_str(), b"data", File)],
                ),
                _ => {
                    if *kind == "directory" {
                        std::fs::create_dir(&at).unwrap();
                    } else {
                        std::fs::write(&at, b"data").unwrap();
                    }
                    let mut command = Command::new("tar");
                    command.args(["-czf", "-"]).arg("-C").arg(&source);
                    if *kind == "parent" {
                        command.arg(format!("--transform=s|^{path}$|../../../../escaped|"));
                    } else if *kind == "absolute" {
                        command.arg(format!("--transform=s|^{path}$|{}|", outside.display()));
                    }
                    command.arg(&path);
                    if *kind == "duplicate" {
                        command.arg(&path);
                    } else if *kind == "hardlink" {
                        let linked = format!("{path}-linked");
                        std::fs::hard_link(&at, source.join(&linked)).unwrap();
                        tree.insert(linked.clone(), entry(b"data", File));
                        expected.push(linked.clone());
                        command.arg(linked);
                    }
                    let out = command.output().unwrap();
                    assert!(
                        out.status.success(),
                        "fixture {kind}: {}",
                        String::from_utf8_lossy(&out.stderr)
                    );
                    out.stdout
                }
            };
            let digest = sync::sha256_hex(&bytes);
            blobs.insert(format!("/cas/{digest}"), bytes);
            packs.push((sync::sha256_hex(kind.as_bytes()), digest, expected));
        }
        let server = Http::new(ws, blobs);
        let out = run(ws, &server.base, &tree, &packs);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let keys: Vec<_> = packs.iter().map(|p| p.0.clone()).collect();
        assert_eq!(failed(&out.stdout, &keys), Some(keys.into_iter().collect()));
        let fill = ws.join(".sylphx/fill");
        assert_eq!(std::fs::read(fill.join("ok")).unwrap(), b"");
        assert_eq!(std::fs::read_dir(fill.join("tree")).unwrap().count(), 0);
        assert!(!outside.exists());
        assert!(!fill.join("auth").exists());
        assert!(!fill.join("stage").exists());
    }
}
