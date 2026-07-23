#!/usr/bin/env python3
"""Stage and pack the Server launcher plus one native npm candidate."""

import argparse
import json
import shutil
import stat
import subprocess
import sys
from pathlib import Path

from common import CandidateError, ROOT, executable_name, target_details


def copy_distribution_files(destination: Path) -> None:
    for name in ("LICENSE", "NOTICE", "README.md"):
        shutil.copy2(ROOT / name, destination / name)


def npm_pack(source: Path, destination: Path) -> dict:
    npm = "npm.cmd" if sys.platform == "win32" else "npm"
    completed = subprocess.run(
        [
            npm,
            "pack",
            "--ignore-scripts",
            "--json",
            "--pack-destination",
            str(destination),
        ],
        cwd=str(source),
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        raise CandidateError("npm pack failed: {}".format(completed.stderr.strip()))
    result = json.loads(completed.stdout)[0]
    return {
        "filename": result["filename"],
        "integrity": result["integrity"],
        "name": result["name"],
        "version": result["version"],
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    stage = args.output.resolve() / "stage"
    try:
        manifest, details = target_details(args.target)
        binary = args.binary.resolve()
        if not binary.is_file():
            raise CandidateError("npm candidate binary does not exist: {}".format(binary))
        if stage.exists():
            raise CandidateError("refusing to replace npm stage: {}".format(stage))
        args.output.mkdir(parents=True, exist_ok=True)
        package_directory = details["npm_package"].replace("@nostdb/server-", "")
        platform_source = ROOT / "npm" / "packages" / package_directory
        platform_stage = stage / "platform"
        launcher_stage = stage / "launcher"
        shutil.copytree(str(platform_source), str(platform_stage))
        shutil.copytree(
            str(ROOT / "npm"),
            str(launcher_stage),
            ignore=shutil.ignore_patterns(
                "packages", "tests", "scripts", "node_modules"
            ),
        )
        copy_distribution_files(platform_stage)
        copy_distribution_files(launcher_stage)
        executable = platform_stage / "bin" / executable_name(args.target)
        executable.parent.mkdir(parents=True)
        shutil.copy2(binary, executable)
        executable.chmod(
            executable.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH
        )
        platform_result = npm_pack(platform_stage, args.output)
        launcher_result = npm_pack(launcher_stage, args.output)
        if platform_result["name"] != details["npm_package"]:
            raise CandidateError("packed unexpected Server platform package")
        if launcher_result["name"] != "@nostdb/server":
            raise CandidateError("packed unexpected Server launcher package")
        for result in (platform_result, launcher_result):
            if result["version"] != manifest["version"]:
                raise CandidateError("npm candidate version mismatch")
        payload = {
            "launcher": launcher_result,
            "platform": platform_result,
            "published": False,
            "target": args.target,
        }
        print(json.dumps(payload, sort_keys=True))
        return 0
    except (CandidateError, OSError, ValueError, subprocess.SubprocessError) as error:
        print("nostdb-server-npm-candidate: {}".format(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
