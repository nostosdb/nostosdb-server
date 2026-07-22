#!/usr/bin/env python3
"""Shared dependency-free Server release-candidate helpers."""

import json
import platform
from pathlib import Path
from typing import Dict, Tuple


ROOT = Path(__file__).resolve().parents[2]
MANIFEST_PATH = ROOT / "distribution" / "release-manifest.json"


class CandidateError(RuntimeError):
    """An invalid or incomplete Server release candidate."""


def read_json(path: Path) -> dict:
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise CandidateError("cannot read JSON {}: {}".format(path, error)) from error


def release_manifest() -> dict:
    manifest = read_json(MANIFEST_PATH)
    if manifest.get("schema_version") != 1:
        raise CandidateError("unsupported release manifest schema")
    if manifest.get("binary") != "nostosd":
        raise CandidateError("release manifest must name nostosd")
    return manifest


def target_details(target: str) -> Tuple[dict, dict]:
    manifest = release_manifest()
    try:
        details = manifest["targets"][target]
    except KeyError as error:
        raise CandidateError("unsupported release target: {}".format(target)) from error
    return manifest, details


def host_target() -> str:
    systems: Dict[str, str] = {
        "Darwin": "apple-darwin",
        "Linux": "unknown-linux-gnu",
        "Windows": "pc-windows-msvc",
    }
    machines = {
        "aarch64": "aarch64",
        "arm64": "aarch64",
        "amd64": "x86_64",
        "x86_64": "x86_64",
    }
    try:
        return "{}-{}".format(
            machines[platform.machine().lower()], systems[platform.system()]
        )
    except KeyError as error:
        raise CandidateError(
            "unsupported native host: {} {}".format(
                platform.system(), platform.machine()
            )
        ) from error


def executable_name(target: str) -> str:
    return "nostosd.exe" if "windows" in target else "nostosd"
