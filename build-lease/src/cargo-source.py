"""Merge the lease source into Cargo home, preserving explicit caller sources."""
import datetime
import json
import os
from pathlib import Path
import stat
import sys
import tempfile
import tomllib


def encode(value):
    if isinstance(value, str):
        # TOML requires Unicode scalar values, not JSON's surrogate-pair escapes.
        return json.dumps(value, ensure_ascii=False).replace("\x7f", "\\u007f")
    if isinstance(value, bool):
        return str(value).lower()
    if isinstance(value, (datetime.datetime, datetime.date, datetime.time)):
        return value.isoformat()
    if isinstance(value, list):
        return "[" + ", ".join(encode(x) for x in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(encode(k) + " = " + encode(v)
                                 for k, v in value.items()) + " }"
    return repr(value)


def render(config):
    text = "\n".join(encode(k) + " = " + encode(v)
                     for k, v in config.items()) + "\n"
    tomllib.loads(text)
    return text


def write(path, text):
    # Preserve symlinked caller files and permissions. Never truncate a config
    # when writing fails (for example, on a full workspace volume).
    path.parent.mkdir(parents=True, exist_ok=True)
    path = path.resolve()
    mode = stat.S_IMODE(path.stat().st_mode) if path.exists() else 0o600
    with tempfile.NamedTemporaryFile(mode="w", dir=path.parent, delete=False) as out:
        temporary = Path(out.name)
        try:
            out.write(text)
            out.flush()
            os.chmod(temporary, mode)
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


home = Path(sys.argv[1])
state_path = home / ".sylphx-mirror.json"
# A warm lease can change or disable its mirror. Remove only the definitions
# this helper still owns; a caller edit relinquishes that ownership.
if state_path.exists():
    previous = json.loads(state_path.read_text())
    if previous["file"] not in ("config", "config.toml"):
        raise ValueError("invalid managed Cargo config path")
    old_path = home / previous["file"]
    if old_path.exists():
        old_text = old_path.read_text()
        config = tomllib.loads(old_text)
        sources = config.get("source", {})
        name = previous["source"]
        if (sources.get("crates-io") == {"replace-with": name}
                and sources.get(name) == {"registry": previous["registry"]}):
            del sources["crates-io"]
            # Another caller source may now depend on the managed registry.
            referenced = any(v.get("replace-with") == name for v in sources.values())
            remove_registry = previous.get("owns_registry", True) and not referenced
            if remove_registry:
                del sources[name]
            if not sources:
                config.pop("source")
            addition = previous["addition"]
            cleaned = (old_text[:-len(addition)]
                       if remove_registry and old_text.endswith(addition)
                       else render(config))
            write(old_path, cleaned)
    state_path.unlink()

mirror = os.environ.get("SYLPHX_CRATES_MIRROR", "")
if not mirror or "CARGO_REGISTRIES_CRATES_IO_INDEX" in os.environ:
    sys.exit(0)
path = home / ("config" if (home / "config").exists() else "config.toml")
original = path.read_text() if path.exists() else ""
config = tomllib.loads(original)
sources = config.get("source", {})
if "crates-io" in sources:
    sys.exit(0)
name = next((name for name, value in sources.items()
             if value == {"registry": mirror}), None)
owns_registry = name is None
if owns_registry:
    name = "sylphx-mirror"
    while name in sources:
        name += "-build"
addition = (
    '\n[source.crates-io]\nreplace-with = ' + encode(name) + '\n'
)
if owns_registry:
    addition += ('\n[source.' + encode(name) + ']\nregistry = '
                 + encode(mirror) + '\n')
merged = original + addition
try:
    tomllib.loads(merged)
except tomllib.TOMLDecodeError:
    # Inline source tables cannot be extended by a later table header.
    sources["crates-io"] = {"replace-with": name}
    if owns_registry:
        sources[name] = {"registry": mirror}
    config["source"] = sources
    merged = render(config)
# Record ownership before the config write: a failed write is safe to reconcile
# on the next invocation, and cannot leave an untracked managed replacement.
write(state_path, json.dumps({"file": path.name, "source": name,
                             "registry": mirror, "addition": addition,
                             "owns_registry": owns_registry}))
write(path, merged)
