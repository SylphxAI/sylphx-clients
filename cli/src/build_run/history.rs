//! Optional offline source history for checks that compare HEAD with a base ref.
use std::path::Path;
use std::process::Stdio;

/// Git may spawn pack-objects while bundling. Cancel that whole process group,
/// not just its parent, when Ctrl-C drops the preparation future.
#[cfg(unix)]
struct GitGroup(Option<libc::pid_t>);

#[cfg(unix)]
impl Drop for GitGroup {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            // SAFETY: the child owns this group (`process_group(0)` at spawn).
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

async fn run(root: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(root)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let child = command
        .spawn()
        .map_err(|e| format!("preparing source history: {e}"))?;
    #[cfg(unix)]
    let mut group = GitGroup(child.id().map(|pid| pid as libc::pid_t));
    let out = child.wait_with_output().await;
    #[cfg(unix)]
    if out.is_ok() {
        // Git has joined its helpers and was reaped; do not signal a reused pid.
        group.0 = None;
    }
    let out = out.map_err(|e| format!("preparing source history: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "preparing source history: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

pub async fn prepare(root: &Path, base: &str) -> Result<(String, Vec<u8>), String> {
    let commit = String::from_utf8(
        run(
            root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{base}^{{commit}}"),
            ],
        )
        .await
        .map_err(|error| {
            format!("--source-history {base} does not resolve to a local commit: {error}")
        })?,
    )
    .map_err(|e| e.to_string())?
    .trim()
    .to_string();
    let symbolic = String::from_utf8(
        run(
            root,
            &[
                "rev-parse",
                "--symbolic-full-name",
                "--verify",
                "--end-of-options",
                base,
            ],
        )
        .await?,
    )
    .map_err(|e| e.to_string())?
    .trim()
    .to_string();
    let reference = if symbolic.starts_with("refs/") {
        symbolic
    } else {
        commit
    };
    let bundle = run(root, &["bundle", "create", "-", "HEAD", &reference]).await?;
    Ok((reference, bundle))
}

/// Leave ample room for multipart framing below the proxy's 100 MiB body limit.
pub const CHUNK_SIZE: usize = 32 * 1024 * 1024;

pub fn digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Only publish a complete, byte-identical bundle. Parts are lease-local, and
/// are removed on both success and failure; no Git command runs on partial data.
pub const REASSEMBLE: &str = r#"set -eu
W=$1 N=$2 S=$3 H=$4
P="$W/.sylphx/source.bundle.part"
T="$W/.sylphx/source.bundle.tmp"
trap 'rm -f "$T" "$P".*' EXIT
trap 'exit 1' HUP INT TERM
: > "$T"
i=0
while [ "$i" -lt "$N" ]; do
  cat "$P.$i" >> "$T"
  i=$((i + 1))
done
test "$(wc -c < "$T")" -eq "$S"
printf '%s  %s\n' "$H" "$T" | sha256sum -c --quiet
mv -f "$T" "$W/.sylphx/source.bundle"
"#;

pub const CLEAN_PARTS: &str =
    r#"rm -f "$1/.sylphx/source.bundle.part."* "$1/.sylphx/source.bundle.tmp""#;

/// Install objects and refs without checking out files over the synced tree.
pub const APPLY: &str = r#"set -e
W=$1 B=$2
trap 'rm -f "$W/.sylphx/source.bundle"' EXIT
trap 'exit 1' HUP INT TERM
cd "$W/tree"
git init -q
case "$B" in
  refs/*) git fetch --no-tags --update-head-ok "$W/.sylphx/source.bundle" "+HEAD:refs/sylphx/source-head" "+$B:$B" ;;
  *)
    # Bundle packs include the raw commit's graph even though its header only
    # advertises named refs. Fetch imports the whole pack; retain the base too.
    git fetch --no-tags --update-head-ok "$W/.sylphx/source.bundle" "+HEAD:refs/sylphx/source-head"
    git update-ref refs/sylphx/source-base "$B"
    ;;
esac
git update-ref --no-deref HEAD refs/sylphx/source-head
git reset --mixed -q HEAD
rm "$W/.sylphx/source.bundle"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "sylphx-source-history-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn git(root: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .current_dir(root)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8(o.stdout).unwrap().trim().into()
    }

    fn reassemble(ws: &Path, bytes: &[u8], count: usize) -> std::process::Output {
        Command::new("sh")
            .args(["-c", REASSEMBLE, "history"])
            .arg(ws)
            .arg(count.to_string())
            .arg(bytes.len().to_string())
            .arg(digest(bytes))
            .output()
            .unwrap()
    }

    fn stage(ws: &Path, bytes: &[u8]) -> usize {
        let mut count = 0;
        for (i, part) in bytes.chunks(CHUNK_SIZE).enumerate() {
            assert!(part.len() <= CHUNK_SIZE);
            // Exercise the actual upload encoder, including multipart framing.
            let body = sylphx_build_lease::guest::multipart("sylphx0000000000000000", part);
            assert!(body.len() < 33 * 1024 * 1024);
            fs::write(ws.join(format!(".sylphx/source.bundle.part.{i}")), part).unwrap();
            count += 1;
        }
        count
    }

    #[tokio::test]
    async fn multi_chunk_history_roundtrip() {
        let s = Scratch::new();
        let src = s.0.join("src");
        let ws = s.0.join("workspace with spaces");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(ws.join("tree")).unwrap();
        fs::create_dir_all(ws.join(".sylphx")).unwrap();
        git(&src, &["init", "-q"]);
        // Deterministic incompressible content makes the real Git bundle
        // exceed the production threshold, not a test-only chunk size.
        let mut state = 0x12345678u32;
        let bytes: Vec<u8> = (0..CHUNK_SIZE + 65536)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        fs::write(src.join("large"), &bytes).unwrap();
        git(&src, &["add", "large"]);
        git(&src, &["commit", "-qm", "large"]);
        let head = git(&src, &["rev-parse", "HEAD"]);
        let (reference, bundle) = prepare(&src, &head).await.unwrap();
        let count = stage(&ws, &bundle);
        assert!(count > 1, "bundle must exceed the production threshold");
        let o = reassemble(&ws, &bundle, count);
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(fs::read(ws.join(".sylphx/source.bundle")).unwrap(), bundle);
        assert_eq!(fs::read_dir(ws.join(".sylphx")).unwrap().count(), 1);
        let o = Command::new("sh")
            .args(["-c", APPLY, "history"])
            .arg(&ws)
            .arg(reference)
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        assert_eq!(git(&ws.join("tree"), &["rev-parse", "HEAD"]), head);
        let blob = Command::new("git")
            .arg("-C")
            .arg(ws.join("tree"))
            .args(["show", "HEAD:large"])
            .output()
            .unwrap();
        assert!(blob.status.success());
        assert_eq!(blob.stdout, bytes);
        assert_eq!(fs::read_dir(ws.join(".sylphx")).unwrap().count(), 0);
    }

    #[test]
    fn reassembly_rejects_missing_truncated_and_corrupt_parts() {
        let s = Scratch::new();
        fs::create_dir_all(s.0.join(".sylphx")).unwrap();
        for (part, count) in [
            (&b"payload"[..], 2),
            (&b"short"[..], 1),
            (&b"corrupt"[..], 1),
        ] {
            stage(&s.0, part);
            let o = reassemble(&s.0, b"payload", count);
            assert!(!o.status.success());
            assert_eq!(fs::read_dir(s.0.join(".sylphx")).unwrap().count(), 0);
        }
    }

    #[tokio::test]
    async fn offline_history_preserves_commits_merge_base_and_dirty_files_on_warm_sync() {
        let s = Scratch::new();
        let src = s.0.join("src");
        let ws = s.0.join("ws");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(ws.join("tree")).unwrap();
        fs::create_dir_all(ws.join(".sylphx")).unwrap();
        git(&src, &["init", "-q"]);
        fs::write(src.join("file"), "base\n").unwrap();
        git(&src, &["add", "file"]);
        git(&src, &["commit", "-qm", "base"]);
        let base = git(&src, &["rev-parse", "HEAD"]);
        git(&src, &["update-ref", "refs/remotes/origin/main", &base]);
        for content in ["first\n", "second\n"] {
            fs::write(src.join("file"), content).unwrap();
            git(&src, &["commit", "-qam", "change"]);
            let head = git(&src, &["rev-parse", "HEAD"]);
            let (reference, bundle) = prepare(&src, "origin/main").await.unwrap();
            assert_eq!(reference, "refs/remotes/origin/main");
            fs::write(ws.join(".sylphx/source.bundle"), bundle).unwrap();
            fs::write(ws.join("tree/file"), "dirty\n").unwrap();
            fs::write(ws.join("tree/untracked"), "local\n").unwrap();
            let o = Command::new("sh")
                .args(["-c", APPLY, "history"])
                .arg(&ws)
                .arg(&reference)
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            let tree = ws.join("tree");
            assert_eq!(git(&tree, &["rev-parse", "HEAD"]), head);
            assert_eq!(git(&tree, &["merge-base", "origin/main", "HEAD"]), base);
            assert_eq!(
                git(&tree, &["diff", "--name-only", "origin/main", "HEAD"]),
                "file"
            );
            assert_eq!(fs::read_to_string(tree.join("file")).unwrap(), "dirty\n");
            assert_eq!(
                fs::read_to_string(tree.join("untracked")).unwrap(),
                "local\n"
            );
        }
    }

    #[tokio::test]
    async fn raw_sha_ships_a_divergent_base_without_changing_local_refs() {
        let s = Scratch::new();
        let src = s.0.join("src");
        let ws = s.0.join("ws");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(ws.join("tree")).unwrap();
        fs::create_dir_all(ws.join(".sylphx")).unwrap();
        git(&src, &["init", "-q"]);
        fs::write(src.join("file"), "common\n").unwrap();
        git(&src, &["add", "file"]);
        git(&src, &["commit", "-qm", "common"]);
        let common = git(&src, &["rev-parse", "HEAD"]);
        fs::write(src.join("file"), "base\n").unwrap();
        git(&src, &["commit", "-qam", "base"]);
        let base = git(&src, &["rev-parse", "HEAD"]);
        git(&src, &["checkout", "-q", "--detach", &common]);
        fs::write(src.join("file"), "head\n").unwrap();
        git(&src, &["commit", "-qam", "head"]);
        let head = git(&src, &["rev-parse", "HEAD"]);
        let refs = git(&src, &["show-ref"]);
        let (reference, bundle) = prepare(&src, &base).await.unwrap();
        assert_eq!(reference, base);
        assert_eq!(git(&src, &["show-ref"]), refs);
        fs::write(ws.join(".sylphx/source.bundle"), bundle).unwrap();
        fs::write(ws.join("tree/file"), "dirty\n").unwrap();
        let output = Command::new("sh")
            .args(["-c", APPLY, "history"])
            .arg(&ws)
            .arg(&reference)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let tree = ws.join("tree");
        assert_eq!(git(&tree, &["rev-parse", "HEAD"]), head);
        assert_eq!(git(&tree, &["merge-base", &base, "HEAD"]), common);
        assert_eq!(git(&tree, &["rev-parse", "refs/sylphx/source-base"]), base);
        assert_eq!(fs::read_to_string(tree.join("file")).unwrap(), "dirty\n");
    }

    #[tokio::test]
    async fn missing_sha_reports_the_missing_local_commit() {
        let s = Scratch::new();
        git(&s.0, &["init", "-q"]);
        let missing = "ffffffffffffffffffffffffffffffffffffffff";
        let error = prepare(&s.0, missing).await.unwrap_err();
        assert!(error.contains(missing), "{error}");
        assert!(
            error.contains("does not resolve to a local commit"),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_local_git_work_cancels_helpers_without_blocking_the_runtime() {
        use std::time::Duration;

        let s = Scratch::new();
        let args = [
            "-c",
            "alias.history-wait=!echo started > started; sleep 1; echo survived > survived",
            "history-wait",
        ];
        let mut work = Box::pin(run(&s.0, &args));
        // This timer and file observation run on the same single-threaded
        // runtime as Git preparation: synchronous output() would block them.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    result = &mut work => panic!("Git ended before cancellation: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {
                        if s.0.join("started").exists() {
                            break;
                        }
                    }
                }
            }
        })
        .await
        .unwrap();
        drop(work);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !s.0.join("survived").exists(),
            "Git's helper outlived cancellation"
        );
    }
}
