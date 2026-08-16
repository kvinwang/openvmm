#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import argparse
import re
import sys
from pathlib import Path

ID = re.compile(r"[a-z][a-z0-9]*(?:[._-][a-z0-9]+)*")
POSITIVE = re.compile(r"[1-9][0-9]*")
REQUIRES = {"none", "block", "network"}


def validate(manifest: Path, cases: Path) -> list[str]:
    errors: list[str] = []
    seen: set[str] = set()
    records = 0
    for number, line in enumerate(manifest.read_text().splitlines(), 1):
        if not line or line.startswith("#"):
            continue
        records += 1
        fields = line.split("\t")
        if len(fields) != 6:
            errors.append(f"line {number}: expected 6 tab-separated fields")
            continue
        case_id, timeout, minimum_cpus, requires, runner, description = fields
        if not ID.fullmatch(case_id):
            errors.append(f"line {number}: invalid id {case_id!r}")
        if case_id in seen:
            errors.append(f"line {number}: duplicate id {case_id!r}")
        seen.add(case_id)
        if not POSITIVE.fullmatch(timeout):
            errors.append(f"line {number}: timeout must be positive")
        if not POSITIVE.fullmatch(minimum_cpus):
            errors.append(f"line {number}: minimum_cpus must be positive")
        if requires not in REQUIRES:
            errors.append(f"line {number}: invalid requirement {requires!r}")
        if not ID.fullmatch(runner) or not (cases / f"{runner}.sh").is_file():
            errors.append(f"line {number}: missing or invalid runner {runner!r}")
        if not description.strip():
            errors.append(f"line {number}: empty description")
    if not records:
        errors.append("manifest has no cases")
    return errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("manifest", type=Path)
    parser.add_argument("cases", type=Path)
    args = parser.parse_args()
    try:
        errors = validate(args.manifest, args.cases)
    except OSError as error:
        print(f"manifest validation failed: {error}", file=sys.stderr)
        return 2
    for error in errors:
        print(error, file=sys.stderr)
    return bool(errors)


if __name__ == "__main__":
    raise SystemExit(main())
