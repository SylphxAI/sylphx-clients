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
    let reference = String::from_utf8(
        run(
            root,
            &["rev-parse", "--symbolic-full-name", "--verify", base],
        )
        .await?,
    )
    .map_err(|e| e.to_string())?
    .trim()
    .to_string();
    if !reference.starts_with("refs/") {
        return Err("--source-history needs a named local ref, such as origin/main".into());
    }
    let bundle = run(root, &["bundle", "create", "-", "HEAD", &reference]).await?;
    Ok((reference, bundle))
}

/// Install objects and refs without checking out files over the synced tree.
pub const APPLY: &str = r#"set -e
W=$1 B=$2
cd "$W/tree"
git init -q
git fetch --no-tags --update-head-ok "$W/.sylphx/source.bundle" "+HEAD:refs/sylphx/source-head" "+$B:$B"
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
