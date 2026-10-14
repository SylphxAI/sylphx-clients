# Managed hints are not high-priority registry env overrides. npm gets a
# global config default; Bun gets its lowest-tier effective user .npmrc.
# Existing registry keys, project npmrc, all bunfig files, env and CLI win.
if [ -n "${SYLPHX_NPM_MIRROR:-}" ] && [ "${NPM_CONFIG_GLOBALCONFIG+x}${npm_config_globalconfig+x}" = "" ]; then
  _sd_original_global=
  if command -v npm >/dev/null 2>&1; then
    _sd_original_global=$(npm config get globalconfig 2>/dev/null) || exit 125
  else
    for _sd_file in /usr/local/etc/npmrc /etc/npmrc; do
      if [ -f "$_sd_file" ]; then _sd_original_global=$_sd_file; break; fi
    done
  fi
  NPM_CONFIG_GLOBALCONFIG=${XDG_CONFIG_HOME:-${HOME:-/home/user}/.config}/sylphx/npmrc
  export NPM_CONFIG_GLOBALCONFIG
fi
# Image steps restore only files they change; mirror addresses never remain in
# the committed layer. No managed registry hints means no files are touched.
if [ "${SYLPHX_REGISTRY_EPHEMERAL:-0}" = 1 ] && [ -n "${SYLPHX_NPM_MIRROR:-}${SYLPHX_GOPROXY:-}${SYLPHX_PYPI_MIRROR:-}" ]; then
  _sr_backup=$(mktemp -d /dev/shm/sylphx-registry.XXXXXX) || exit 125
  _sr_home=${HOME:-/root}
  _sr_config=${XDG_CONFIG_HOME:-$_sr_home/.config}
  _sr_i=0
  for _sr_path in "${NPM_CONFIG_GLOBALCONFIG:-${npm_config_globalconfig:-$_sr_config/sylphx/npmrc}}" "${XDG_CONFIG_HOME:-$_sr_home}/.npmrc" "${GOENV:-$_sr_config/go/env}" "$_sr_config/pip/pip.conf" "$_sr_home/.pip/pip.conf" "$_sr_config/uv/uv.toml"; do
    [ "$_sr_path" != off ] || continue
    _sr_i=$((_sr_i + 1))
    printf '%s\n' "$_sr_path" > "$_sr_backup/path.$_sr_i"
    if [ -f "$_sr_path" ]; then cp -p "$_sr_path" "$_sr_backup/file.$_sr_i" || exit 125; fi
  done
  sylphx_restore_registry() {
    for _sr_entry in "$_sr_backup"/path.*; do
      _sr_path=$(cat "$_sr_entry")
      _sr_i=${_sr_entry##*.}
      if [ -f "$_sr_backup/file.$_sr_i" ]; then cp -p "$_sr_backup/file.$_sr_i" "$_sr_path" || exit 125; else rm -f "$_sr_path" || exit 125; fi
    done
    rm -rf "$_sr_backup"
  }
  trap sylphx_restore_registry 0
fi
sylphx_registry_defaults() {
  _sd_home=${HOME:-/home/user}
  _sd_config=${XDG_CONFIG_HOME:-$_sd_home/.config}
  if [ -n "${SYLPHX_NPM_MIRROR:-}" ]; then
    _sd_npm=${NPM_CONFIG_GLOBALCONFIG-${npm_config_globalconfig-$_sd_config/sylphx/npmrc}}
    if [ -n "$_sd_npm" ]; then
      if [ ! -e "$_sd_npm" ] && [ "$_sd_npm" = "$_sd_config/sylphx/npmrc" ] && [ -z "${_sd_original_global:-}" ] && command -v npm >/dev/null 2>&1; then
        _sd_original_global=$( (unset NPM_CONFIG_GLOBALCONFIG npm_config_globalconfig; npm config get globalconfig) 2>/dev/null) || return
      fi
      mkdir -p "$(dirname "$_sd_npm")" || return
      if [ ! -e "$_sd_npm" ] && [ -n "${_sd_original_global:-}" ] && [ -f "$_sd_original_global" ]; then
        cp -p "$_sd_original_global" "$_sd_npm" || return
      fi
      if ! grep -Eiq '^[[:space:]]*registry[[:space:]]*=' "$_sd_npm" 2>/dev/null; then
        printf '\n# sylphx-managed npm registry\nregistry=%s\n' "$SYLPHX_NPM_MIRROR" >> "$_sd_npm" || return
      fi
    fi
    # Bun reads user npmrc before project npmrc and global/project bunfig.
    # A bunfig registry is too high-priority to be a managed default.
    _sd_bun=${XDG_CONFIG_HOME:-$_sd_home}/.npmrc
    _sd_bun_allowed=1
    # With no XDG, this file also belongs to npm. Do not shadow its explicit
    # global registry by creating a higher-priority HOME/.npmrc default.
    if [ "$_sd_bun" = "$_sd_home/.npmrc" ] && grep -Eiq '^[[:space:]]*registry[[:space:]]*=' "$_sd_npm" 2>/dev/null && { ! grep -q '^# sylphx-managed npm registry$' "$_sd_npm" || ! grep -Fxq "registry=$SYLPHX_NPM_MIRROR" "$_sd_npm"; }; then _sd_bun_allowed=0; fi
    if [ "$_sd_bun_allowed" = 1 ] && [ "${NPM_CONFIG_REGISTRY+x}${npm_config_registry+x}${BUN_CONFIG_REGISTRY+x}" = "" ] && ! grep -Eiq '^[[:space:]]*registry[[:space:]]*=' "$_sd_bun" 2>/dev/null; then
      mkdir -p "$(dirname "$_sd_bun")" || return
      printf '\n# sylphx-managed bun npm registry\nregistry=%s\n' "$SYLPHX_NPM_MIRROR" >> "$_sd_bun" || return
    fi
  fi
  if [ -n "${SYLPHX_GOPROXY:-}" ] && [ "${GOPROXY+x}" = "" ]; then
    _sd_go=${GOENV:-$_sd_config/go/env}
    if [ "$_sd_go" != off ] && ! grep -q '^GOPROXY=' "$_sd_go" 2>/dev/null; then
      mkdir -p "$(dirname "$_sd_go")" || return
      printf 'GOPROXY=%s\n' "$SYLPHX_GOPROXY" >> "$_sd_go" || return
    fi
  fi
  if [ -n "${SYLPHX_PYPI_MIRROR:-}" ]; then
    # pip itself reads global, legacy user, site and PIP_CONFIG_FILE choices.
    if [ "${PIP_INDEX_URL+x}${PIP_EXTRA_INDEX_URL+x}${PIP_NO_INDEX+x}${PIP_CONFIG_FILE+x}" = "" ] && command -v pip >/dev/null 2>&1; then
      _sd_pip=$(pip config list 2>/dev/null) || return
      if ! printf '%s\n' "$_sd_pip" | grep -Eq '(^|\.)(index-url|extra-index-url|no-index)='; then
        pip config --user set global.index-url "$SYLPHX_PYPI_MIRROR" >/dev/null || return
        case "$SYLPHX_PYPI_MIRROR" in
          http://*)
            if [ "${PIP_TRUSTED_HOST+x}" = "" ] && ! printf '%s\n' "$_sd_pip" | grep -q 'trusted-host='; then
              _sd_host=${SYLPHX_PYPI_MIRROR#http://}; _sd_host=${_sd_host%%/*}
              pip config --user set global.trusted-host "$_sd_host" >/dev/null || return
            fi ;;
        esac
      fi
    fi
    # uv merges system < user < project configuration, then env and CLI.
    _sd_uv=$_sd_config/uv/uv.toml
    _sd_uv_override=3
    for _sd_file in "$_sd_uv" /etc/uv/uv.toml; do
      [ -e "$_sd_file" ] || continue
      if python3 -c 'import sys,tomllib; c=tomllib.load(open(sys.argv[1],"rb")); sys.exit(0 if any(k in c for k in ("index","index-url","default-index","extra-index-url","no-index")) else 3)' "$_sd_file"; then
        _sd_uv_override=0; break
      else
        _sd_status=$?
        [ "$_sd_status" = 3 ] || return "$_sd_status"
      fi
    done
    if [ "${UV_INDEX+x}${UV_DEFAULT_INDEX+x}${UV_INDEX_URL+x}${UV_EXTRA_INDEX_URL+x}${UV_NO_INDEX+x}${UV_CONFIG_FILE+x}" = "" ] && [ "$_sd_uv_override" = 3 ]; then
      mkdir -p "$(dirname "$_sd_uv")" || return
      _sd_tmp=$_sd_uv.sylphx.$$
      { printf 'index-url = "%s"\n' "$SYLPHX_PYPI_MIRROR"; if [ -f "$_sd_uv" ]; then cat "$_sd_uv"; fi; } > "$_sd_tmp" || return
      mv "$_sd_tmp" "$_sd_uv" || return
    fi
  fi
  unset _sd_home _sd_config _sd_npm _sd_original_global _sd_bun _sd_bun_allowed _sd_go _sd_pip _sd_host _sd_uv _sd_tmp _sd_uv_override _sd_file _sd_status
}
sylphx_registry_defaults || exit 125
