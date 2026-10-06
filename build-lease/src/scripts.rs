//! The shell the build guest runs: preparing the workspace, provisioning the
//! toolchain, running the command, listing artifacts. Shared by the CLI and
//! the Build service so both run a command the same way.

/// The free space a warm workspace keeps before a sync, whichever is larger
/// ([`BOOTSTRAP`] holds the same numbers). Sized from a measured peak: a
/// clean `cargo build --workspace --all-targets` plus `cargo clippy
/// --workspace --all-targets` of SylphxAI/cloud on a lease (2026-10-06,
/// incremental off, no debug info) grew an empty `target/` to 9.0 GiB, with
/// a 0.54 GiB Cargo home and a 0.28 GiB tree: 9.8 GiB for one whole state,
/// so 15 GiB leaves 50 % over it. sccache writes to the remote build cache
/// whenever the run has one, so its local directory stays empty. The 100 GiB
/// Volume (`VOLUME_GIB` in the CLI) therefore keeps about eight such states
/// before a prune, and stays as it is.
pub const WORKSPACE_MIN_FREE_GIB: u64 = 15;
pub const WORKSPACE_MIN_FREE_PERCENT: u64 = 15;
/// [`BOOTSTRAP`]'s status for a workspace too small even when empty.
pub const BOOTSTRAP_TOO_SMALL: i32 = 75;

/// Prepares the workspace as root: the mount root belongs to the guest user,
/// and the manifest is offered compressed for the client to read.
///
/// `$2` = 1 on a warm workspace Volume: everything on it is a cache (the
/// synced tree, `target/`, the Cargo home, sccache, toolchains), and
/// `target/` grows without bound (Cargo never collects old artifacts), so a
/// Volume fills until every run fails before its command. Before the sync
/// the free space must be at least [`WORKSPACE_MIN_FREE_GIB`] or
/// [`WORKSPACE_MIN_FREE_PERCENT`] of the Volume, whichever is larger. Short
/// of it, `target/` is removed (sccache refills it); still short, the whole
/// workspace is emptied, which makes it a fresh one (no manifest, so the
/// client syncs it cold). Still short when empty: exit 75. `$3` = 1 empties
/// it first anyway: the client asks for that on its one retry after a run
/// ran out of space before its command started. Prints `pruned` or `emptied`
/// on stdout when it did either.
pub const BOOTSTRAP: &str = r#"set -eu
W=$1 CHECK=${2:-0} EMPTY=${3:-0} MIN_GIB=15 MIN_PCT=15
mkdir -p "$W/.sylphx"
room() {
  df -Pk "$W" | awk -v gib="$MIN_GIB" -v pct="$MIN_PCT" 'NR == 2 { need = $2 * pct / 100; if (need < gib * 1048576) need = gib * 1048576; exit !($4 >= need) }'
}
empty() {
  find "$W" -mindepth 1 -maxdepth 1 ! -name lost+found -exec rm -rf {} +
  mkdir -p "$W/.sylphx"
  echo emptied
}
if [ "$EMPTY" = 1 ]; then
  empty
elif [ "$CHECK" = 1 ] && ! room; then
  rm -rf "$W/target"
  if room; then echo pruned; else empty; fi
fi
if [ "$CHECK" = 1 ] && ! room; then
  echo "the workspace has too little free space even when empty: $(df -Ph "$W" | awk 'NR == 2 { print $4 " of " $2 " free" }')" >&2
  exit 75
fi
chown user:user "$W" "$W/.sylphx" 2>/dev/null || true
rm -f "$W/.sylphx/manifest.gz"
if [ -f "$W/.sylphx/manifest" ]; then
  gzip -c "$W/.sylphx/manifest" > "$W/.sylphx/manifest.gz"
  chown user:user "$W/.sylphx/manifest.gz" 2>/dev/null || true
fi
"#;

/// The directories both guest scripts share: the build caches live on the
/// workspace (`$W`) so a warm machine reuses them.
macro_rules! guest_dirs {
    () => {
        r#"export CARGO_TARGET_DIR="$W/target" CARGO_HOME="$W/cargo" SCCACHE_DIR="$W/sccache"
export PATH="$W/cargo/bin:$PATH"
"#
    };
}

/// The template's rustup home: the `build` image bakes the platform's pinned
/// toolchain there (services/sandboxes/templates/build/Dockerfile). Named
/// here because the image's `ENV RUSTUP_HOME` never reaches a guest command:
/// envd starts each process with only `PATH`, `HOME`, `USER` and `LOGNAME`
/// from its own environment, so rustup would look in `~/.rustup` instead.
macro_rules! template_rustup_home {
    () => {
        "/opt/rustup"
    };
}

/// The rustup home both guest scripts use, chosen in the tree: the
/// template's ([`template_rustup_home`]), on the lease's own disk and fresh
/// from the image each lease, whenever it holds the toolchain the tree names,
/// so no download is needed and nothing a past lease left on the Volume is
/// run; otherwise the workspace's, where rustup installs the tree's choice
/// on first use. Both scripts choose alike, so the command runs the
/// toolchain [`PROVISION`] checked.
macro_rules! guest_toolchain {
    () => {
        concat!(
            r#"if command -v rustup >/dev/null 2>&1; then
  if RUSTUP_HOME=""#,
            template_rustup_home!(),
            r#"" RUSTUP_AUTO_INSTALL=0 rustup which rustc >/dev/null 2>&1; then
    export RUSTUP_HOME=""#,
            template_rustup_home!(),
            r#""
  else
    export RUSTUP_HOME="$W/rustup"
  fi
fi
"#
        )
    };
}

/// Makes `$W/tree` a git work tree: the real `.git` stays home (its packs
/// would cost more than the tree), so the guest keeps its own repository in
/// the warm workspace, created once with `git init`, and each run stages the
/// synced tree (`git add -A`, only the changed files once warm) and points
/// `HEAD` at one parentless commit of it, the message naming the local
/// `HEAD` (`SYLPHX_BUILD_GIT_HEAD`). Each run replaces that commit rather
/// than adding to a history, and `git gc --auto` prunes the replaced ones, so
/// a long-lived warm workspace does not grow per run. The
/// sync never sends or deletes `.git` (the CLI's `sync::plan` and `sync::APPLY`).
/// Bounded and fail-open: a missing `git` or a failed step prints one
/// `sylphx: warning:` line and the command still runs.
macro_rules! git_tree {
    () => {
        r#"if command -v git >/dev/null 2>&1; then
  TO=
  if command -v timeout >/dev/null 2>&1; then TO="timeout 300"; fi
  if ! $TO sh -c '
    cd "$1" || exit 1
    g() { git -c init.defaultBranch=main -c core.logAllRefUpdates=false -c core.hooksPath=/dev/null -c commit.gpgsign=false -c user.name=sylphx -c user.email=build@sylphx.invalid "$@"; }
    if [ ! -d .git ]; then g init -q && mkdir -p .git/info && printf "/target\n" >> .git/info/exclude || exit 1; fi
    g add -A && t=$(g write-tree) && c=$(g commit-tree "$t" -m "sylphx build run of $2") && g update-ref --no-deref HEAD "$c" || exit 1
    g reflog expire --expire=now --all; g -c gc.pruneExpire=now gc --auto --quiet
  ' sylphx-git "$W/tree" "${SYLPHX_BUILD_GIT_HEAD:-an unknown commit}" > "$W/.sylphx/git.log" 2>&1; then
    echo "sylphx: warning: the tree on the build machine is not a git work tree, so git calls in the command fail: $(tail -n 1 "$W/.sylphx/git.log" 2>/dev/null)" >&2
  fi
else
  echo "sylphx: warning: the build machine has no git, so git calls in the command fail" >&2
fi
"#
    };
}

/// Everything that must be ready before the user's command starts: the tree
/// directory and, for a Rust tree, a toolchain that runs. It runs as its own
/// process so a failure here is told apart from the command's own exit
/// status: any non-zero status is a platform failure (125), never the
/// command's. A tree without Rust files (looked for up to the tree root) does
/// not need the toolchain, so a failed install does not stop its command.
///
/// The toolchain is checked before use: its `rustc -vV` and `cargo -V` must
/// run. One on the workspace (a toolchain the template does not bake) outlives
/// the lease that installed it, so it must also match the checksums written
/// when it was installed and checked (`.sylphx-sha256` in its directory); a
/// copy that a lost lease left half-written or that a disk error made
/// unreadable fails, is removed and installed again. rustup's own component
/// list is left out, so a `rustup component add` in a command does not count
/// as damage (the files it adds are not listed, so they are not checked). One with no checksums
/// (installed by an older client) is installed again once. Installs go
/// through the build cache's toolchain tier (`RUSTUP_DIST_SERVER`, from
/// [`provision_env`]) and directly from static.rust-lang.org when the tier
/// fails. The log is `$W/.sylphx/toolchain.log`; its tail goes to stderr on
/// failure.
pub const PROVISION: &str = concat!(
    r#"W=$1 R=$2
"#,
    guest_dirs!(),
    r#"cd "$W/tree/$R" || { echo "the work tree is missing on the machine" >&2; exit 3; }
"#,
    guest_toolchain!(),
    r#"L="$W/.sylphx/toolchain.log"
: > "$L"
rust_tree() {
  d=$PWD
  while :; do
    if [ -e "$d/rust-toolchain.toml" ] || [ -e "$d/rust-toolchain" ] || [ -e "$d/Cargo.toml" ]; then return 0; fi
    [ "$d" = "$W/tree" ] && return 1
    d=$(dirname "$d")
  done
}
tc_dir() { t=$(RUSTUP_AUTO_INSTALL=0 rustup which rustc 2>/dev/null) && [ -n "$t" ] && echo "${t%/bin/rustc}"; }
works() { T=$(tc_dir) && "$T/bin/rustc" -vV >> "$L" 2>&1 && "$T/bin/cargo" -V >> "$L" 2>&1; }
sealed() { [ "$RUSTUP_HOME" != "$W/rustup" ] || ( cd "$T" && sha256sum -c --quiet --status .sylphx-sha256 ) >> "$L" 2>&1; }
fetch() { { rustup toolchain install || rustup default stable; } >> "$L" 2>&1; }
install() {
  fetch && return 0
  [ -n "${RUSTUP_DIST_SERVER:-}" ] || return 1
  echo "the build cache's toolchain tier failed; fetching from static.rust-lang.org" >> "$L"
  ( unset RUSTUP_DIST_SERVER RUSTUP_UPDATE_ROOT; fetch )
}
if command -v rustup >/dev/null 2>&1 && ! { works && sealed; }; then
  if [ "$RUSTUP_HOME" = "$W/rustup" ]; then
    if T=$(tc_dir); then
      N=${T##*/}
      echo "the workspace's toolchain $N failed its check; installing it again" >> "$L"
      rustup toolchain uninstall "$N" >> "$L" 2>&1
      rm -rf "$T" "$RUSTUP_HOME/update-hashes/$N"
    fi
    install
    if ! works && rust_tree; then
      echo "the toolchain still does not run; starting the workspace's rustup home over" >> "$L"
      rm -rf "$W/rustup"
      install
    fi
    if works; then
      ( cd "$T" && find . -type f ! -name '.sylphx-sha256*' ! -path ./lib/rustlib/components ! -path './lib/rustlib/manifest-*' -exec sha256sum {} + > .sylphx-sha256.new && mv .sylphx-sha256.new .sylphx-sha256 ) >> "$L" 2>&1
    fi
  fi
  if ! { works && sealed; } && rust_tree; then
    echo "the Rust toolchain could not be installed or does not run:" >&2
    tail -n 6 "$L" >&2
    exit 4
  fi
fi
exit 0
"#
);

/// [`PROVISION`]'s environment: only the cache environment's toolchain tier
/// (`RUSTUP_DIST_SERVER`, `RUSTUP_UPDATE_ROOT`: the build cache's
/// `/upstream/static.rust-lang.org`, public content checked there against
/// upstream's checksums and shared by every lease), never its tokens. Empty
/// with `--no-cache` or no cache: rustup fetches directly.
pub fn provision_env(
    cache: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    cache
        .iter()
        .filter(|(k, _)| matches!(k.as_str(), "RUSTUP_DIST_SERVER" | "RUSTUP_UPDATE_ROOT"))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Runs the command (`$4…`) in `$W/tree/$R` with the build caches on the
/// workspace: `target/`, the Cargo home, sccache, and rustup's toolchains
/// ([`PROVISION`] has installed them; [`guest_toolchain`] picks the same
/// rustup home). `$3` is 1 on a machine without a Volume: its disk ends with
/// the lease, so sccache runs only against a remote cache the machine is
/// given, never a local directory that would double `target/` on the same
/// disk. The command's exit status is the script's.
///
/// Cargo builds as CI does (`.github/workflows/ci.yml`): incremental
/// compilation off (`CARGO_INCREMENTAL=0`), which sccache cannot cache, so
/// the workspace's own crates hit the cache too, and no debug info for the
/// dev and test profiles (`CARGO_PROFILE_DEV_DEBUG=0`). A clean warm-cache
/// `cargo build --workspace` of SylphxAI/cloud on `xlarge` took 4 min 29 s
/// with these against 5 min 40 s without (docs/services/build/benchmark.md).
/// Either variable set by the caller (`--env`), even to an empty value, wins.
///
/// Crates come through the build cache's registry mirror
/// (`SYLPHX_CRATES_MIRROR`, from the cache token) when it answers: Cargo reads
/// `$W/.cargo/config.toml` as an ancestor of the tree, and the file is removed
/// when the mirror is absent, so Cargo then reaches crates.io directly, which
/// a lease reaches only with `--allow-host`. npm, bun and pnpm read the
/// token's `NPM_CONFIG_REGISTRY` themselves.
///
/// The synced tree is a git work tree before the command starts, so tests
/// and build scripts that call `git` behave as they do locally (see
/// [`git_tree`]).
pub const RUN: &str = concat!(
    r#"W=$1 R=$2 E=$3
shift 3
"#,
    guest_dirs!(),
    git_tree!(),
    r#"export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-20G}"
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL-0}" CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG-0}"
if [ -z "${RUSTC_WRAPPER:-}" ] && command -v sccache >/dev/null 2>&1; then
  if [ "$E" != 1 ] || [ -n "${SCCACHE_WEBDAV_ENDPOINT:-}" ]; then export RUSTC_WRAPPER=sccache; fi
fi
mkdir -p "$W/.cargo"
M=${SYLPHX_CRATES_MIRROR:-}
if [ -n "$M" ] && curl -fsS -m 10 -o /dev/null "${M#sparse+}config.json" 2>/dev/null; then
  printf '[source.crates-io]\nreplace-with = "sylphx-mirror"\n\n[source.sylphx-mirror]\nregistry = "%s"\n' "$M" > "$W/.cargo/config.toml"
else
  rm -f "$W/.cargo/config.toml"
fi
cd "$W/tree/$R" || exit 125
"#,
    guest_toolchain!(),
    r#"exec "$@"
"#
);

/// Lists the files matching the globs (`$3…`, relative to `$W/tree/$R`) with
/// their SHA-256, for the copy-back.
pub const ARTIFACTS: &str = r#"W=$1 R=$2
shift 2
cd "$W/tree/$R" || exit 0
IFS=
for g in "$@"; do
  for f in $g; do
    [ -f "$f" ] && sha256sum -- "$f"
  done
done
exit 0
"#;
