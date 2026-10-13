#!/usr/bin/env python3
"""Await npm's asynchronous version propagation within a 15-minute bound."""

import subprocess
import sys
import time

READBACK_SECONDS = 15 * 60
POLL_SECONDS = 10


def wait_for_version(name, version):
    # The registry indexed 0.29.11 just after the previous five-minute bound.
    # Bound both the whole readback and each npm request, not only the sleeps.
    deadline = time.monotonic() + READBACK_SECONDS
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        try:
            result = subprocess.run(
                ["npm", "view", f"{name}@{version}", "version", "--prefer-online"],
                capture_output=True, text=True, timeout=remaining,
            )
        except subprocess.TimeoutExpired:
            break
        if result.returncode == 0 and result.stdout.strip() == version:
            return
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        time.sleep(min(POLL_SECONDS, remaining))
    raise RuntimeError(f"{name}@{version} did not appear on npm within {READBACK_SECONDS} seconds")


if __name__ == "__main__":
    wait_for_version(*sys.argv[1:])
