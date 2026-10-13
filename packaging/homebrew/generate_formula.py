#!/usr/bin/env python3
"""Render a Homebrew formula from release archives and their checksum manifest."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


TARGETS = {
    "MAC_ARM64_SHA256": "aarch64-apple-darwin",
    "MAC_X64_SHA256": "x86_64-apple-darwin",
    "LINUX_ARM64_SHA256": "aarch64-unknown-linux-gnu",
    "LINUX_X64_SHA256": "x86_64-unknown-linux-gnu",
}
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.-]+)?\Z")
SHA256_RE = re.compile(r"[0-9a-f]{64}\Z")


def load_checksums(path: Path, version: str) -> dict[str, str]:
    checksums: dict[str, str] = {}
    for line_number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        parts = line.split()
        if len(parts) != 2 or not SHA256_RE.fullmatch(parts[0]):
            raise ValueError(f"{path}:{line_number}: expected a SHA256SUMS entry")
        digest, filename = parts
        if filename in checksums:
            raise ValueError(f"{path}:{line_number}: duplicate asset {filename}")
        checksums[filename] = digest

    expected = {
        f"termplete-{version}-{target}.tar.gz" for target in TARGETS.values()
    }
    if set(checksums) != expected:
        missing = sorted(expected - set(checksums))
        extra = sorted(set(checksums) - expected)
        details = []
        if missing:
            details.append(f"missing: {', '.join(missing)}")
        if extra:
            details.append(f"unexpected: {', '.join(extra)}")
        raise ValueError("checksum manifest does not match release targets (" + "; ".join(details) + ")")
    return checksums


def render(version: str, checksums: dict[str, str]) -> str:
    template_path = Path(__file__).with_name("termplete.rb.in")
    formula = template_path.read_text(encoding="utf-8")
    replacements = {"VERSION": version}
    replacements.update(
        {
            key: checksums[f"termplete-{version}-{target}.tar.gz"]
            for key, target in TARGETS.items()
        }
    )
    for key, value in replacements.items():
        formula = formula.replace(f"@@{key}@@", value)
    if re.search(r"@@[A-Z0-9_]+@@", formula):
        raise ValueError("formula template contains an unknown placeholder")
    return formula


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True, help="release version without the leading v")
    parser.add_argument("--checksums", required=True, type=Path, help="release SHA256SUMS file")
    parser.add_argument("--output", required=True, type=Path, help="destination formula path")
    args = parser.parse_args()

    if not VERSION_RE.fullmatch(args.version):
        parser.error("version must be a semantic version without a leading v")
    try:
        checksums = load_checksums(args.checksums, args.version)
        formula = render(args.version, checksums)
        args.output.write_text(formula, encoding="utf-8")
    except (OSError, ValueError) as error:
        parser.error(str(error))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
