#!/usr/bin/env python3
"""Development input custody helpers; live boot qualification requires PR 3."""

import os
import stat
from pathlib import Path
import shutil
import sys

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / 'tests'))
from prepare_current_native_release import verify_candidate
from qualify_native_upgrade import custody, require, sha256, unavailable


def verify_runner_inputs(checkout, inputs, revision, tools):
    """Validate runner-owned inputs before making fresh root-owned copies."""
    owner = checkout.stat().st_uid
    custody(checkout, owner, directory=True)
    custody(inputs, owner, directory=True)
    for path in tools:
        custody(path, owner)
    # The source verifier scopes Git safe.directory to this canonical checkout
    # in each read-only command; no root-global Git setting changes.
    return verify_candidate(checkout, inputs, revision, 'development', owner=owner)


def stage_package_tool(source, lane):
    """Freeze a Cargo utility as a fresh inode before the root custody handoff."""
    info = source.lstat()
    require(stat.S_ISREG(info.st_mode) and info.st_uid == os.geteuid()
            and not info.st_mode & 0o022 and info.st_mode & 0o111,
            'unsafe Cargo package utility')
    before = sha256(source)
    custody(lane, os.geteuid(), directory=True)
    stage = lane / 'package-tool'
    stage.mkdir(mode=0o700)
    target = stage / 'ww-release-package'
    with source.open('rb') as reader, target.open('xb') as writer:
        shutil.copyfileobj(reader, writer)
    target.chmod(0o755)
    custody(target, info.st_uid)
    require(sha256(source) == before and sha256(target) == before,
            'package utility staging changed identity or bytes')
    (lane / 'package-tool-source.sha256').write_text(f'{before}  {source}\n')
    (lane / 'package-tool.sha256').write_text(f'{before}  {target}\n')
    return target


def main():
    return unavailable()


if __name__ == '__main__':
    sys.exit(main())
