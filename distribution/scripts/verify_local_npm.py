#!/usr/bin/env python3
"""Verify an isolated offline global Server plus CLI npm installation."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

from common import CandidateError, host_target, release_manifest


def run(arguments, *, cwd=None, env=None) -> subprocess.CompletedProcess:
    completed = subprocess.run(
        [str(value) for value in arguments],
        cwd=str(cwd) if cwd else None,
        env=env,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        raise CandidateError(
            "command failed ({}): {}".format(
                completed.returncode, completed.stderr.strip()
            )
        )
    return completed


def stage(script: Path, target: str, binary: Path, output: Path, env: dict) -> dict:
    completed = run(
        [
            sys.executable,
            "-B",
            script,
            "--target",
            target,
            "--binary",
            binary,
            "--output",
            output,
        ],
        env=env,
    )
    return json.loads(completed.stdout)


def command_path(prefix: Path, command: str) -> Path:
    if os.name == "nt":
        return prefix / (command + ".cmd")
    return prefix / "bin" / command


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli-root", type=Path, required=True)
    parser.add_argument("--cli-binary", type=Path, required=True)
    parser.add_argument("--server-binary", type=Path, required=True)
    args = parser.parse_args()
    server_root = Path(__file__).resolve().parents[2]
    target = host_target()
    version = release_manifest()["version"]
    try:
        cli_root = args.cli_root.resolve()
        cli_script = cli_root / "distribution" / "scripts" / "stage_npm_candidate.py"
        if not cli_script.is_file():
            raise CandidateError("CLI npm candidate script is missing")
        with tempfile.TemporaryDirectory(prefix="nostdb-server-npm-install-") as temporary:
            root = Path(temporary)
            cache = root / "cache"
            environment = dict(os.environ)
            environment["npm_config_cache"] = str(cache)
            cli_output = root / "cli"
            server_output = root / "server"
            cli = stage(
                cli_script,
                target,
                args.cli_binary.resolve(),
                cli_output,
                environment,
            )
            server = stage(
                server_root / "distribution" / "scripts" / "stage_npm_candidate.py",
                target,
                args.server_binary.resolve(),
                server_output,
                environment,
            )
            archives = [
                cli_output / cli["platform"]["filename"],
                cli_output / cli["launcher"]["filename"],
                server_output / server["platform"]["filename"],
                server_output / server["launcher"]["filename"],
            ]
            prefix = root / "prefix"
            npm = "npm.cmd" if os.name == "nt" else "npm"
            run(
                [
                    npm,
                    "install",
                    "--global",
                    "--ignore-scripts",
                    "--offline",
                    "--cache",
                    cache,
                    "--prefix",
                    prefix,
                    *archives,
                ],
                env=environment,
            )
            expected = {
                "nostdb": "nostdb {}".format(version),
                "nostd": "nostd {}".format(version),
            }
            for command, version_line in expected.items():
                executable = command_path(prefix, command)
                if not executable.is_file():
                    raise CandidateError("global command shim is missing: {}".format(command))
                completed = run([executable, "--version"], env=environment)
                if completed.stdout.strip() != version_line:
                    raise CandidateError(
                        "{} version mismatch: {}".format(
                            command, completed.stdout.strip()
                        )
                    )
            tree = json.loads(
                run(
                    [
                        npm,
                        "list",
                        "--global",
                        "--json",
                        "--all",
                        "--prefix",
                        prefix,
                    ],
                    env=environment,
                ).stdout
            )
            installed = tree.get("dependencies", {})
            for package in (
                "@nostdb/cli",
                cli["platform"]["name"],
                "@nostdb/server",
                server["platform"]["name"],
            ):
                if installed.get(package, {}).get("version") != version:
                    raise CandidateError("global package version mismatch: {}".format(package))
            server_dependencies = installed["@nostdb/server"].get("dependencies", {})
            if server_dependencies.get("@nostdb/cli", {}).get("version") != version:
                raise CandidateError("Server did not deduplicate the exact CLI dependency")
            if (
                server_dependencies.get(server["platform"]["name"], {}).get("version")
                != version
            ):
                raise CandidateError("Server did not select the exact native dependency")
            cli_dependencies = installed["@nostdb/cli"].get("dependencies", {})
            if cli_dependencies.get(cli["platform"]["name"], {}).get("version") != version:
                raise CandidateError("CLI did not select the exact native dependency")
        print(
            json.dumps(
                {
                    "commands": ["nostdb", "nostd"],
                    "installed_packages": 4,
                    "passed": True,
                    "published": False,
                    "target": target,
                    "version": version,
                },
                sort_keys=True,
            )
        )
        return 0
    except (CandidateError, OSError, ValueError, subprocess.SubprocessError) as error:
        print("nostdb-server-npm-local: {}".format(error), file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
