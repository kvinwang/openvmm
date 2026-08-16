#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import argparse
import json
import re
import sys
from pathlib import Path

PREFIX = "VIRT_TDP_CTS "
FIELD = re.compile(r"([a-z_]+)=([^ ]+)")


def manifest_ids(path: Path) -> list[str]:
    result = []
    for line in path.read_text().splitlines():
        if not line or line.startswith("#"):
            continue
        result.append(line.split("\t", 1)[0])
    return result


def fields(line: str) -> dict[str, str]:
    return dict(FIELD.findall(line))


def parse(serial: Path, manifest: Path) -> dict:
    begin = None
    end = None
    cases = {}
    details = []
    for raw in serial.read_text(errors="replace").splitlines():
        offset = raw.find(PREFIX)
        if offset < 0:
            continue
        record = raw[offset + len(PREFIX) :]
        kind, _, payload = record.partition(" ")
        if kind == "BEGIN":
            if begin is not None:
                raise ValueError("duplicate BEGIN record")
            begin = fields(payload)
        elif kind == "RESULT":
            data = fields(payload)
            case_id = data.get("id")
            if not case_id or case_id in cases:
                raise ValueError(f"missing or duplicate case id: {case_id}")
            cases[case_id] = data
        elif kind == "DETAIL":
            details.append(payload)
        elif kind == "END":
            if end is not None:
                raise ValueError("duplicate END record")
            end = fields(payload)

    expected = manifest_ids(manifest)
    if begin is None or begin.get("version") != "1":
        raise ValueError("missing or unsupported BEGIN record")
    if end is None:
        raise ValueError("missing END record")
    missing = [case for case in expected if case not in cases]
    extra = [case for case in cases if case not in expected]
    if missing or extra:
        raise ValueError(f"case set mismatch: missing={missing}, extra={extra}")
    for case_id, data in cases.items():
        required = {"id", "status", "duration_ms", "reason"}
        if set(data) != required:
            raise ValueError(f"case {case_id} has malformed fields")
        if data["status"] not in {"PASS", "FAIL", "SKIP"}:
            raise ValueError(f"case {case_id} has invalid status")
        if not data["duration_ms"].isdigit():
            raise ValueError(f"case {case_id} has invalid duration")
    counts = {
        status: sum(case.get("status") == status for case in cases.values())
        for status in ("PASS", "FAIL", "SKIP")
    }
    if int(end.get("total", -1)) != len(expected):
        raise ValueError("END total does not match manifest")
    for status, count in counts.items():
        if int(end.get(status.lower(), -1)) != count:
            raise ValueError(f"END {status.lower()} count does not match records")
    if sum(counts.values()) != len(expected):
        raise ValueError("result status counts do not cover the manifest")
    return {
        "schema_version": 1,
        "begin": begin,
        "summary": {"total": len(expected), **{k.lower(): v for k, v in counts.items()}},
        "cases": [cases[case] for case in expected],
        "details": details,
        "passed": counts["FAIL"] == 0,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("serial", type=Path)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try:
        result = parse(args.serial, args.manifest)
    except (OSError, ValueError) as error:
        print(f"invalid conformance result: {error}", file=sys.stderr)
        return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output:
        args.output.write_text(encoded)
    else:
        sys.stdout.write(encoded)
    return 0 if result["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
