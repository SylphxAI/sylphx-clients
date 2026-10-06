//! Git dependencies of a `sylphx build run`: the locked git sources of the
//! tree's `Cargo.lock` files, fetched on this machine and sent to the build
//! machine, which has no route to a git host.
//!
//! A build lease reaches only the build cache ([`super::lease_domains`]), so a
//! crate with a git dependency (`engine = { git = "https://github.com/…",
//! rev = "…" }`) used to fail in the guest's `git fetch` after its connect
//! timeouts, about ten minutes per run. The inputs travel with the build
//! instead, as a remote-execution client sends them (Bazel's external
//! repositories): this machine holds the git credentials, fetches each locked
//! commit once (`--depth 1`: about 12 MB where the full history is 114 MB),
//! and keeps it as a small bare repository in a tarball under the repository's
//! git directory. The workspace keeps one shallow mirror per URL at
//! `$W/git-deps/<key>.git` holding every commit sent so far ([`HAVE`] asks
//! which are missing, [`APPLY`] adds them), and the command runs with
//! `CARGO_NET_GIT_FETCH_WITH_CLI=true` and one `url.<mirror>.insteadOf <url>`
//! per dependency ([`env`]), so Cargo's own fetch reads the mirror and its
//! lock file, source ids and caches stay as they are. Git prints one
//! `warning: rejected refs/commit/… because shallow roots are not allowed to
//! be updated` the first time a Cargo home fetches a commit; the objects
//! arrive and the build goes on.
//!
//! A dependency this machine cannot fetch stops the run before any machine is
//! leased: exit 125, not retryable, naming the URL and the commit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use super::sync::Tree;

/// The mirrors' directory under the workspace.
pub const DIR: &str = "git-deps";

/// One locked git source: the URL as the lock file names it (Cargo's fetch
/// URL) and the full commit id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Dep {
    pub url: String,
    pub rev: String,
}

impl Dep {
    /// The mirror's name on the workspace: 16 hex digits of the URL's SHA-256.
    pub fn key(&self) -> String {
        Sha256::digest(self.url.as_bytes())[..8]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    /// `<key>-<rev>`: the tarball's name, here and on the workspace.
    pub fn name(&self) -> String {
        format!("{}-{}", self.key(), self.rev)
    }
}

/// The git sources of one `Cargo.lock`: `source = "git+<url>[?…]#<commit>"`.
/// A source without a full commit id is left alone (Cargo resolves it).
pub fn parse_lock(text: &str) -> BTreeSet<Dep> {
    text.lines()
        .filter_map(|l| {
            let v = l
                .trim()
                .strip_prefix("source = \"git+")?
                .strip_suffix('"')?;
            let (head, rev) = v.rsplit_once('#')?;
            let url = head.split('?').next()?;
            let full = rev.len() == 40 && rev.bytes().all(|b| b.is_ascii_hexdigit());
            (full && !url.is_empty()).then(|| Dep {
                url: url.to_string(),
                rev: rev.to_ascii_lowercase(),
            })
        })
        .collect()
}

/// The `Cargo.lock` paths Cargo can use for a command run in `rel` (a tree
/// path, `""` or `"."` for the root): the run directory's own and each of its
/// ancestors' up to the tree root, as Cargo looks up its workspace. A lock
/// elsewhere in the tree (another workspace, a fixture) is not read: its git
/// sources are neither fetched nor able to fail the run.
pub fn lock_paths(rel: &str) -> Vec<String> {
    let mut dir: Vec<&str> = rel
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .collect();
    let mut out = Vec::new();
    loop {
        out.push(if dir.is_empty() {
            "Cargo.lock".to_string()
        } else {
            format!("{}/Cargo.lock", dir.join("/"))
        });
        if dir.pop().is_none() {
            return out;
        }
    }
}

/// Every git source locked by a `Cargo.lock` of the synced tree that a
/// command run in `rel` can use ([`lock_paths`]).
pub fn locked(root: &Path, tree: &Tree, rel: &str) -> Result<Vec<Dep>, String> {
    let mut all = BTreeSet::new();
    for path in lock_paths(rel) {
        if !tree.contains_key(&path) {
            continue;
        }
        let text = std::fs::read_to_string(root.join(&path)).map_err(|e| format!("{path}: {e}"))?;
        all.extend(parse_lock(&text));
    }
    Ok(all.into_iter().collect())
}

/// The tarball of `dep` (one bare repository, `repo.git`, holding the commit
/// as `refs/commit/<rev>` with depth 1), made once in `cache` and kept: a
/// commit never changes. Fetches with this machine's git and its credentials,
/// never prompting.
pub fn pack(cache: &Path, dep: &Dep) -> Result<PathBuf, String> {
    let out = cache.join(format!("{}.tgz", dep.name()));
    if out.is_file() {
        return Ok(out);
    }
    if !["https://", "http://", "ssh://", "git://", "file://"]
        .iter()
        .any(|p| dep.url.starts_with(p))
    {
        return Err(format!(
            "git dependency {} at {}: only https://, http://, ssh://, git:// and file:// URLs are fetched",
            dep.url, dep.rev
        ));
    }
    let fail = |what: &str, e: String| {
        format!(
            "git dependency {} at {}: {what} on this machine failed: {e}",
            dep.url, dep.rev
        )
    };
    let work = cache.join(format!("{}.tmp-{}", dep.name(), std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let repo = work.join("repo.git");
    std::fs::create_dir_all(&repo).map_err(|e| fail("preparing the cache", e.to_string()))?;
    let made = (|| {
        run(Command::new("git")
            .arg("init")
            .arg("-q")
            .arg("--bare")
            .arg(&repo))
        .map_err(|e| fail("git init", e))?;
        run(Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("-C")
            .arg(&repo)
            .args([
                "-c",
                "http.lowSpeedLimit=1000",
                "-c",
                "http.lowSpeedTime=60",
                "fetch",
                "-q",
                "--depth",
                "1",
                "--",
            ])
            .arg(&dep.url)
            .arg(format!("+{0}:refs/commit/{0}", dep.rev)))
        .map_err(|e| fail("fetching it", e))?;
        let part = work.join("repo.tgz");
        run(Command::new("tar")
            .arg("-czf")
            .arg(&part)
            .arg("-C")
            .arg(&work)
            .arg("repo.git"))
        .map_err(|e| fail("packing it", e))?;
        std::fs::rename(&part, &out).map_err(|e| fail("keeping it", e.to_string()))
    })();
    let _ = std::fs::remove_dir_all(&work);
    made.map(|()| out)
}

fn run(c: &mut Command) -> Result<(), String> {
    let o = c.output().map_err(|e| e.to_string())?;
    if o.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&o.stderr);
    let tail: Vec<&str> = err.lines().rev().take(3).collect();
    Err(tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
}

/// Lists the dependencies (`$2…`, each `<key>-<rev>`) the workspace `$1`
/// lacks, one per line, and makes the upload directory. Exit 3: no git on
/// the machine (nothing can be sent to it).
pub const HAVE: &str = r#"W=$1
shift
command -v git >/dev/null 2>&1 || exit 3
D=$W/git-deps
mkdir -p "$D/in" || exit 1
for n in "$@"; do
  k=${n%%-*} r=${n#*-}
  git -C "$D/$k.git" cat-file -e "$r^{commit}" 2>/dev/null || printf '%s\n' "$n"
done
exit 0
"#;

/// Adds each uploaded dependency (`$2…`, each `<key>-<rev>`, its tarball at
/// `$1/git-deps/in/<key>-<rev>.tgz`) to its URL's mirror `<key>.git`: the
/// first commit of a URL becomes the mirror, a later one is fetched into it
/// (shallow, depth 1). The tarballs are removed.
pub const APPLY: &str = r#"set -eu
W=$1
shift
D=$W/git-deps
for n in "$@"; do
  k=${n%%-*} r=${n#*-}
  t=$D/in/$n
  rm -rf "$t"
  mkdir -p "$t"
  tar -xzf "$t.tgz" -C "$t"
  if [ -d "$D/$k.git" ]; then
    git -C "$D/$k.git" fetch -q --depth 1 --update-shallow "file://$t/repo.git" "+refs/commit/$r:refs/commit/$r"
  else
    mv "$t/repo.git" "$D/$k.git"
  fi
  rm -rf "$t" "$t.tgz"
done
"#;

/// The command's environment for the workspace `ws`: Cargo fetches git
/// sources with the git CLI, which reads each dependency's URL from its
/// mirror. Empty with no dependencies.
pub fn env(deps: &[Dep], ws: &str) -> BTreeMap<String, String> {
    let urls: BTreeSet<&Dep> = deps
        .iter()
        .map(|d| (d.url.as_str(), d))
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .collect();
    let mut env = BTreeMap::new();
    if urls.is_empty() {
        return env;
    }
    env.insert("CARGO_NET_GIT_FETCH_WITH_CLI".into(), "true".into());
    env.insert("GIT_CONFIG_COUNT".into(), urls.len().to_string());
    for (i, d) in urls.iter().enumerate() {
        env.insert(
            format!("GIT_CONFIG_KEY_{i}"),
            format!("url.file://{ws}/{DIR}/{}.git.insteadOf", d.key()),
        );
        env.insert(format!("GIT_CONFIG_VALUE_{i}"), d.url.clone());
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK: &str = r#"
[[package]]
name = "engine-core"
version = "0.1.0"
source = "git+https://github.com/acme/engine?rev=c86633d06441e81e6a28dde61e88d37bc3707ef1#c86633d06441e81e6a28dde61e88d37bc3707ef1"

[[package]]
name = "engine-ui"
version = "0.1.0"
source = "git+https://github.com/acme/engine?rev=c86633d06441e81e6a28dde61e88d37bc3707ef1#c86633d06441e81e6a28dde61e88d37bc3707ef1"

[[package]]
name = "kit"
version = "0.1.0"
source = "git+https://github.com/acme/kit?branch=main#31B7E141C592FF53EA98269E218F7570AE0D7D4D"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "00"

[[package]]
name = "short"
version = "0.1.0"
source = "git+https://github.com/o/short?rev=abc#abc"
"#;

    #[test]
    fn a_lock_file_names_each_locked_git_commit_once() {
        let deps: Vec<Dep> = parse_lock(LOCK).into_iter().collect();
        assert_eq!(
            deps,
            [
                Dep {
                    url: "https://github.com/acme/engine".into(),
                    rev: "c86633d06441e81e6a28dde61e88d37bc3707ef1".into(),
                },
                Dep {
                    url: "https://github.com/acme/kit".into(),
                    rev: "31b7e141c592ff53ea98269e218f7570ae0d7d4d".into(),
                },
            ]
        );
        assert_eq!(deps[1].key().len(), 16);
        assert_ne!(deps[0].key(), deps[1].key());
    }

    #[test]
    fn only_the_run_directory_and_its_ancestors_lock_files_count() {
        assert_eq!(lock_paths(""), ["Cargo.lock"]);
        assert_eq!(lock_paths("."), ["Cargo.lock"]);
        assert_eq!(
            lock_paths("services/auth/"),
            [
                "services/auth/Cargo.lock",
                "services/Cargo.lock",
                "Cargo.lock"
            ]
        );
        let root = std::env::temp_dir().join(format!(
            "sylphx-git-deps-scope-{}-{}",
            std::process::id(),
            unique()
        ));
        let s = Scratch(root.clone());
        let lock = |rev: char| {
            format!(
                "source = \"git+https://github.com/o/r{rev}?rev={0}#{0}\"\n",
                rev.to_string().repeat(40)
            )
        };
        let mut tree = Tree::new();
        for (p, rev) in [
            ("Cargo.lock", 'a'),
            ("services/auth/Cargo.lock", 'b'),
            ("kernel/hands/Cargo.lock", 'c'),
        ] {
            let f = s.0.join(p);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(&f, lock(rev)).unwrap();
            tree.insert(
                p.to_string(),
                super::super::sync::Entry {
                    digest: String::new(),
                    mode: super::super::sync::Mode::File,
                    size: 0,
                },
            );
        }
        let revs = |rel: &str| -> Vec<String> {
            locked(&s.0, &tree, rel)
                .unwrap()
                .into_iter()
                .map(|d| d.rev[..1].to_string())
                .collect()
        };
        assert_eq!(revs("services/auth"), ["a", "b"]);
        assert_eq!(revs("."), ["a"]);
        assert_eq!(revs("kernel/hands/src"), ["a", "c"]);
    }

    #[test]
    fn a_url_of_another_scheme_is_never_fetched() {
        let s = Scratch(std::env::temp_dir().join(format!(
            "sylphx-git-deps-scheme-{}-{}",
            std::process::id(),
            unique()
        )));
        std::fs::create_dir_all(&s.0).unwrap();
        for url in [
            "--upload-pack=touch x",
            "ext::sh -c x",
            "git@github.com:o/r",
        ] {
            let d = Dep {
                url: url.to_string(),
                rev: "0".repeat(40),
            };
            let e = pack(&s.0, &d).unwrap_err();
            assert!(e.contains("only https://"), "{url}: {e}");
        }
    }

    #[test]
    fn the_environment_maps_each_url_once_to_its_mirror() {
        let d = |url: &str, rev: &str| Dep {
            url: url.into(),
            rev: rev.into(),
        };
        assert!(env(&[], "/workspace").is_empty());
        let a = d("https://github.com/acme/engine", &"a".repeat(40));
        let b = d("https://github.com/acme/engine", &"b".repeat(40));
        let c = d("https://github.com/acme/kit", &"c".repeat(40));
        let e = env(&[a.clone(), b, c.clone()], "/workspace");
        assert_eq!(e["CARGO_NET_GIT_FETCH_WITH_CLI"], "true");
        assert_eq!(e["GIT_CONFIG_COUNT"], "2");
        let pairs: BTreeSet<(String, String)> = (0..2)
            .map(|i| {
                (
                    e[&format!("GIT_CONFIG_KEY_{i}")].clone(),
                    e[&format!("GIT_CONFIG_VALUE_{i}")].clone(),
                )
            })
            .collect();
        for dep in [a, c] {
            assert!(pairs.contains(&(
                format!("url.file:///workspace/git-deps/{}.git.insteadOf", dep.key()),
                dep.url.clone()
            )));
        }
    }

    struct Scratch(PathBuf);

    fn unique() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    fn sh(script: &str, args: &[&str]) -> (i32, String, String) {
        let o = Command::new("/bin/sh")
            .args(["-c", script, "sh"])
            .args(args)
            .output()
            .unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    }

    /// The whole path with real git: three commits upstream, two of them
    /// locked; each is packed here, the workspace takes what it lacks into one
    /// mirror, and Cargo's own fetch (`git fetch <url> +<rev>:refs/commit/<rev>`
    /// with the run's environment) gets each commit from the mirror under the
    /// upstream's URL, which nothing here can reach.
    #[test]
    fn locked_commits_reach_cargo_through_the_workspace_mirror() {
        let s = Scratch(std::env::temp_dir().join(format!(
            "sylphx-git-deps-{}-{}",
            std::process::id(),
            unique()
        )));
        let up = s.0.join("up");
        std::fs::create_dir_all(&up).unwrap();
        git(&up, &["init", "-q", "-b", "main"]);
        let mut revs = Vec::new();
        for i in 1..=3 {
            std::fs::write(up.join("f"), format!("{i}\n")).unwrap();
            git(&up, &["add", "-A"]);
            git(&up, &["commit", "-qm", &i.to_string()]);
            revs.push(git(&up, &["rev-parse", "HEAD"]));
        }
        let url = format!("file://{}", up.display());
        let dep = |r: &str| Dep {
            url: url.clone(),
            rev: r.to_string(),
        };
        let (one, three) = (dep(&revs[0]), dep(&revs[2]));
        let cache = s.0.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        let p1 = pack(&cache, &one).unwrap();
        // Kept: with the upstream out of reach, the same commit packs again.
        let away = s.0.join("away");
        std::fs::rename(&up, &away).unwrap();
        assert_eq!(pack(&cache, &one).unwrap(), p1);
        std::fs::rename(&away, &up).unwrap();
        let p3 = pack(&cache, &three).unwrap();

        let ws = s.0.join("ws");
        let w = ws.to_str().unwrap();
        let send = |deps: &[&Dep], tgz: &[&PathBuf]| {
            let names: Vec<String> = deps.iter().map(|d| d.name()).collect();
            let mut args = vec![w];
            args.extend(names.iter().map(String::as_str));
            let (code, out, err) = sh(HAVE, &args);
            assert_eq!(code, 0, "{err}");
            let missing: Vec<String> = out.lines().map(str::to_string).collect();
            for (d, t) in deps.iter().zip(tgz) {
                if missing.contains(&d.name()) {
                    std::fs::copy(t, ws.join(DIR).join("in").join(format!("{}.tgz", d.name())))
                        .unwrap();
                }
            }
            let mut args = vec![w];
            args.extend(missing.iter().map(String::as_str));
            let (code, _, err) = sh(APPLY, &args);
            assert_eq!(code, 0, "{err}");
            missing
        };
        assert_eq!(send(&[&one], &[&p1]), [one.name()]);
        assert_eq!(send(&[&one, &three], &[&p1, &p3]), [three.name()]);
        assert!(send(&[&one, &three], &[&p1, &p3]).is_empty());
        assert!(std::fs::read_dir(ws.join(DIR).join("in"))
            .unwrap()
            .next()
            .is_none());

        // The upstream is gone; Cargo's fetch reads the mirror.
        std::fs::remove_dir_all(&up).unwrap();
        let env = env(&[one.clone(), three.clone()], w);
        for d in [&one, &three] {
            let db = s.0.join(format!("db-{}", d.rev));
            std::fs::create_dir_all(&db).unwrap();
            git(&db, &["init", "-q", "--bare"]);
            let o = Command::new("git")
                .arg("-C")
                .arg(&db)
                .envs(&env)
                .args(["fetch", "--force", "--update-head-ok", &d.url])
                .arg(format!("+{0}:refs/commit/{0}", d.rev))
                .output()
                .unwrap();
            assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
            git(&db, &["cat-file", "-e", &format!("{}^{{tree}}", d.rev)]);
        }
    }

    #[test]
    fn a_commit_this_machine_cannot_fetch_names_url_and_commit() {
        let s = Scratch(std::env::temp_dir().join(format!(
            "sylphx-git-deps-missing-{}-{}",
            std::process::id(),
            unique()
        )));
        std::fs::create_dir_all(&s.0).unwrap();
        let d = Dep {
            url: format!("file://{}/nowhere", s.0.display()),
            rev: "0".repeat(40),
        };
        let e = pack(&s.0, &d).unwrap_err();
        assert!(e.contains(&d.url) && e.contains(&d.rev), "{e}");
        assert!(!s.0.join(format!("{}.tgz", d.name())).exists());
    }
}
