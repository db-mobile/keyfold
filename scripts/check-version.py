#!/usr/bin/env python3
"""The workspace version is the one shipped version.

package.json, the Cargo workspace and tauri.conf.json must agree, and no
workspace member may declare its own version, because the package names and
the version the app reports are all derived from it.

Usage: scripts/check-version.py [--tag vX.Y.Z]

With --tag the version must also equal the tag (without its leading "v"), so
a mistyped release tag cannot produce packages whose file name and embedded
version differ.
"""
import argparse
import json
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MEMBERS = ['src-tauri', 'crates/core', 'crates/cli']


def check(tag):
    workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']
    package = json.loads((ROOT / 'package.json').read_text())['version']
    tauri = json.loads((ROOT / 'src-tauri/tauri.conf.json').read_text())['version']
    assert package == workspace == tauri, (
        f'versions differ: package.json={package} Cargo workspace={workspace} tauri.conf.json={tauri}'
    )
    for member in MEMBERS:
        declared = tomllib.loads((ROOT / member / 'Cargo.toml').read_text())['package'].get('version')
        assert declared == {'workspace': True}, (
            f'{member} must inherit version.workspace = true, found {declared!r}'
        )
    if tag is not None:
        assert tag == f'v{workspace}', f'tag {tag} does not match version {workspace} (expected v{workspace})'
    print(f'Version {workspace} agrees across package.json, the Cargo workspace and tauri.conf.json.')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--tag', help='release tag that must equal "v" + version')
    try:
        check(parser.parse_args().tag)
    except AssertionError as error:
        sys.exit(f'check-version: {error}')
