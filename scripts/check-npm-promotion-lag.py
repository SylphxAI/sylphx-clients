#!/usr/bin/env python3
"""Fail visibly if stable npm next is >1 release or >7 days ahead of latest."""
from datetime import datetime, timezone
import json
import re
import sys
from urllib.request import Request, urlopen


def version(text):
    return tuple(map(int, text.split('.'))) if re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', text) else None


def lag(packument, now):
    tags = packument['dist-tags']
    latest, candidate = tags['latest'], tags['next']
    low, high = version(latest), version(candidate)
    if low is None or high is None:
        raise ValueError('latest/next must name stable releases')
    if latest not in packument['versions'] or candidate not in packument['versions']:
        raise ValueError('latest/next is absent from the registry version inventory')
    if low > high:
        raise ValueError('next is older than latest; channel state is inconsistent')
    if low == high:
        return 0, 0
    pending = [v for v in packument['versions'] if version(v) is not None and low < version(v) <= high]
    if candidate not in pending:
        raise ValueError('next is absent from the registry version inventory')
    # Age of the oldest unpromoted release, not the last version's age or the
    # newest next timestamp (which can hide a permanently stuck channel).
    dates = [datetime.fromisoformat(packument['time'][v].replace('Z', '+00:00')) for v in pending]
    age = (now - min(dates)).total_seconds() / 86400
    if age < 0:
        raise ValueError('registry publication time is in the future')
    return len(pending), age


def main():
    failed = False
    for package in ('@sylphx/cli-linux-x64', '@sylphx/cli-linux-arm64', '@sylphx/cli-darwin-x64',
                    '@sylphx/cli-darwin-arm64', '@sylphx/cli', '@sylphx/mcp', '@sylphx/sdk'):
        try:
            request = Request(f'https://registry.npmjs.org/{package.replace("/", "%2f")}', headers={'User-Agent': 'Sylphx-promotion-lag/1.0'})
            with urlopen(request, timeout=30) as response:
                data = json.load(response)
            count, age = lag(data, datetime.now(timezone.utc))
            message = f'{package}: latest={data["dist-tags"]["latest"]} next={data["dist-tags"]["next"]}; lag={count} releases, {age:.2f} days'
            if count > 1 or age > 7:
                print(f'::error::{message}')
                failed = True
            else:
                print(message)
        except Exception as error:
            print(f'::error::{package}: promotion state UNKNOWN: {error}')
            failed = True
    return int(failed)


if __name__ == '__main__':
    sys.exit(main())
