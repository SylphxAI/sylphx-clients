//! The shell the build guest runs: preparing the workspace, provisioning the
//! toolchain, running the command, listing artifacts. Shared by the CLI and
//! the Build service so both run a command the same way.

use std::borrow::Cow;
macro_rules! command_started {
    () => {
        "\x1esylphx-command-started\x1f\n"
    };
}

/// Private guest protocol line, removed from stderr by [`Startup`].
pub const COMMAND_STARTED: &str = command_started!();

/// Tracks command admission while forwarding stderr without its protocol line.
/// Only a possible marker prefix is buffered; arbitrary output bytes are kept.
#[derive(Default)]
pub struct Startup {
    matched: usize,
    started: bool,
}

impl Startup {
    pub fn started(&self) -> bool {
        self.started
    }
    /// Recovers admission from durable guest state after detached output was
    /// lost. A partial protocol marker is not user stderr.
    pub fn recover(&mut self) {
        self.started = true;
        self.matched = 0;
    }
    pub fn push<'a>(&mut self, stderr: &'a [u8]) -> Cow<'a, [u8]> {
        let marker = COMMAND_STARTED.as_bytes();
        if self.matched == 0 && !stderr.contains(&marker[0]) {
            return Cow::Borrowed(stderr);
        }
        let mut output = Vec::with_capacity(stderr.len() + self.matched);
        for (index, &byte) in stderr.iter().enumerate() {
            if byte == marker[self.matched] {
                self.matched += 1;
            } else {
                output.extend_from_slice(&marker[..self.matched]);
                // The marker's initial record separator occurs nowhere else in it.
                self.matched = usize::from(byte == marker[0]);
                if self.matched == 0 {
                    output.push(byte);
                }
            }
            if self.matched == marker.len() {
                self.matched = 0;
                self.started = true;
                output.extend_from_slice(&stderr[index + 1..]);
                break;
            }
        }
        Cow::Owned(output)
    }

    /// Forwards an incomplete marker prefix when the process stream ends.
    pub fn finish(&mut self) -> Vec<u8> {
        let remainder = COMMAND_STARTED.as_bytes()[..self.matched].to_vec();
        self.matched = 0;
        remainder
    }
}

/// One guest action, with positional arguments `W, R, E, command…`.
/// Client-quoted setup executes in a subshell before bounded provisioning.
/// Startup failures are 125; command exits, including 125, retain their status.
pub fn start_script(setup: &str, provision: bool) -> String {
    fn quoted(script: &str) -> String {
        format!("'{}'", script.replace('\'', "'\\''"))
    }
    let mut script = format!(
        "set -e\ntrap 'exit 125' 0\nexport W=$1 R=$2 E=$3\n( sh -c {} sylphx-setup \"$@\" ) || exit 125\n",
        quoted(&format!("set -eu\n{setup}\n")),
    );
    if provision {
        script.push_str(&format!(
            "rm -f \"$W/.sylphx/provisioned-rustup-home\"\ntimeout 900 sh -c {} sylphx-provision \"$W\" \"$R\" || exit 125\nif [ -f \"$W/.sylphx/provisioned-rustup-home\" ]; then export RUSTUP_HOME=$(cat \"$W/.sylphx/provisioned-rustup-home\"); rm -f \"$W/.sylphx/provisioned-rustup-home\"; fi\n",
            quoted(PROVISION),
        ));
    }
    script.push_str("export SYLPHX_BUILD_START_MARKER=1\n");
    script.push_str(RUN);
    script
}

/// The directories both guest scripts share: the build caches live on the
/// workspace (`$W`) so a warm machine reuses them.
macro_rules! guest_dirs {
    () => {
        r#"export CARGO_TARGET_DIR="${CARGO_TARGET_DIR-$W/target}" CARGO_HOME="${CARGO_HOME-$W/cargo}" SCCACHE_DIR="$W/sccache"
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
            r#"if [ "${RUSTUP_HOME+x}" != x ] && command -v rustup >/dev/null 2>&1; then
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
        r#"if [ "${SYLPHX_BUILD_SOURCE_HISTORY:-}" = 1 ]; then
  # The CLI installed exact source commits and the index without checkout.
  # Do not replace HEAD with the ordinary parentless sync snapshot.
  :
elif command -v git >/dev/null 2>&1; then
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
/// through the build cache's toolchain tier (`RUSTUP_DIST_SERVER`) and
/// directly from static.rust-lang.org when the tier
/// fails. The log is `$W/.sylphx/toolchain.log`; its tail goes to stderr on
/// failure. When the template's toolchain runs, its same-named workspace
/// copy and update hash are removed; other workspace toolchains are kept.
pub const PROVISION: &str = concat!(
    r#"W=$1 R=$2
"#,
    guest_dirs!(),
    r#"cd "$W/tree/$R" || { echo "the work tree is missing on the machine" >&2; exit 3; }
"#,
    r#"rust_tree() {
  d=$PWD
  while :; do
    if [ -e "$d/rust-toolchain.toml" ] || [ -e "$d/rust-toolchain" ] || [ -e "$d/Cargo.toml" ]; then return 0; fi
    [ "$d" = "$W/tree" ] && return 1
    d=$(dirname "$d")
  done
}
rust_tree || exit 0
"#,
    guest_toolchain!(),
    r#"L="$W/.sylphx/toolchain.log"
: > "$L"
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
if [ "$RUSTUP_HOME" = ""#,
    template_rustup_home!(),
    r#"" ] && works; then
  N=${T##*/}
  rm -rf "$W/rustup/toolchains/$N" "$W/rustup/update-hashes/$N"
fi
if command -v rustup >/dev/null 2>&1; then printf '%s\n' "$RUSTUP_HOME" > "$W/.sylphx/provisioned-rustup-home" || exit 4; fi
exit 0
"#
);

/// Runs the command ($4…) in $W/tree/$R with workspace caches. $3 is 1
/// for an ephemeral workspace. Caller settings, including an explicitly empty
/// RUSTC_WRAPPER, win. Compiler-cache startup is deferred until compilation;
/// an outage compiles directly with one warning, without replaying compiler errors.
/// Cargo uses native source replacement, respecting explicit Cargo home source
/// definitions and CARGO_REGISTRIES_CRATES_IO_INDEX. No network preflight or
/// subcommand parsing can bypass the isolated lease's mirror route.
pub const RUN: &str = concat!(
    r#"W=$1 R=$2 E=$3
shift 3
"#,
    guest_dirs!(),
    git_tree!(),
    r#"export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-20G}"
I=1
if [ "$E" = 1 ]; then I=0; fi
export CARGO_INCREMENTAL="${CARGO_INCREMENTAL-$I}" CARGO_PROFILE_DEV_DEBUG="${CARGO_PROFILE_DEV_DEBUG-0}"
mkdir -p "$W/.cargo" "$W/.sylphx"
# Cargo discovers workspace config by cwd; temp-dir subprocesses need the
# same source in CARGO_HOME. Remove the old, narrower managed config.
rm -f "$W/.cargo/config.toml"
cd "$W/tree/$R" || exit 125
# Resolve a caller's relative home where Cargo runs, then keep it stable for
# subprocesses that change directory (for example, temporary test packages).
case "$CARGO_HOME" in /*) ;; *) export CARGO_HOME="$PWD/$CARGO_HOME" ;; esac
python3 - "$CARGO_HOME" <<'SYLPHX_CARGO_SOURCE'
"#,
    include_str!("cargo-source.py"),
    r#"SYLPHX_CARGO_SOURCE
if [ "$?" != 0 ]; then
  echo "sylphx: cannot configure Cargo source" >&2; exit 125
fi
"#,
    include_str!("registry-defaults.sh"),
    guest_toolchain!(),
    r#"# A workspace has one leased command; rustc processes share its state.
export SYLPHX_BUILD_CACHE_STATE="$W/.sylphx/lazy-cache"
rm -rf "$SYLPHX_BUILD_CACHE_STATE"
mkdir -p "$SYLPHX_BUILD_CACHE_STATE/bin"
if [ "${CARGO_INCREMENTAL:-0}" != 1 ] && [ "${RUSTC_WRAPPER+x}" != x ] && command -v sccache >/dev/null 2>&1; then
  if [ "$E" != 1 ] || [ -n "${SCCACHE_WEBDAV_ENDPOINT:-}" ]; then
    export SYLPHX_BUILD_SCCACHE=$(command -v sccache)
    cat > "$SYLPHX_BUILD_CACHE_STATE/bin/rustc-wrapper" <<'SYLPHX_RUSTC'
#!/bin/bash
# Cargo passes binary names verbatim in CARGO_BIN_EXE_<name>. The build
# template includes Bash; unlike dash it preserves non-shell environment names.
S=$SYLPHX_BUILD_CACHE_STATE
warn() {
  # Cargo suppresses stderr from successful rustc metadata probes. Do not
  # consume the one warning until a real compile can show it to the caller.
  for a in "$@"; do
    case "$a" in -vV|-V|--version|--print|--print=*) return 0 ;; esac
  done
  if mkdir "$S/warned" 2>/dev/null; then
    echo "sylphx: warning: the build cache did not answer, so this run compiles without it: $(grep -m 1 -v '^[[:space:]]*$' "$S/start.log" 2>/dev/null)" >&2
  fi
}
if [ ! -e "$S/ready" ] && [ ! -e "$S/failed" ]; then
  (
    flock -w 70 9 || exit 1
    [ ! -e "$S/ready" ] && [ ! -e "$S/failed" ] || exit 0
    # Check the gateway only at first compilation, never at command startup.
    reachable() {
      [ -z "${SCCACHE_WEBDAV_ENDPOINT:-}" ] || curl -sS --connect-timeout 3 -m 5 -o /dev/null "$SCCACHE_WEBDAV_ENDPOINT"
    }
    if reachable 2> "$S/start.log" && timeout 60 "$SYLPHX_BUILD_SCCACHE" --start-server > /dev/null 2>> "$S/start.log"; then
      : > "$S/ready"
    else
      : > "$S/failed"
    fi
  ) 9> "$S/start.lock" || { warn "$@"; exec "$@"; }
fi
if [ -e "$S/failed" ]; then warn "$@"; exec "$@"; fi
# Server I/O errors fail open inside sccache. Compiler errors are never retried.
export SCCACHE_IGNORE_SERVER_IO_ERROR=1
exec "$SYLPHX_BUILD_SCCACHE" "$@"
SYLPHX_RUSTC
    chmod +x "$SYLPHX_BUILD_CACHE_STATE/bin/rustc-wrapper"
    export RUSTC_WRAPPER="$SYLPHX_BUILD_CACHE_STATE/bin/rustc-wrapper"
  fi
fi

if [ "${SYLPHX_BUILD_START_MARKER:-}" = 1 ]; then
  if [ -n "${SYLPHX_BUILD_ACTION_STATE:-}" ]; then
    read SYLPHX_COMMAND_UPTIME _ < /proc/uptime
    printf '%s\n' "$SYLPHX_COMMAND_UPTIME" > "$SYLPHX_BUILD_ACTION_STATE.tmp"
    mv "$SYLPHX_BUILD_ACTION_STATE.tmp" "$SYLPHX_BUILD_ACTION_STATE"
  fi
  trap - 0
"#,
    "  printf '%s' '",
    command_started!(),
    "' >&2\n",
    r#"fi
exec "$@"
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

#[cfg(test)]
#[path = "scripts_tests.rs"]
mod tests;
