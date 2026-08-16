#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import argparse
import json
import re
import sys
from pathlib import Path

PREFIX = "VIRT_TDP_STRESS "
FIELD = re.compile(r"([a-z_]+)=([^ ]+)")
EXPECTED = [
    "docker.load", "docker.info", "cpu.parallel", "memory.bandwidth",
    "process.churn", "filesystem.sequential", "postgres.pgbench",
    "postgres.persistence", "redis.benchmark", "redis.persistence",
    "network.raw_tx", "mixed.database_cpu_memory",
]

def parse(path: Path) -> dict:
    begin = None; end = None; cases = {}; metrics = {}
    for raw in path.read_text(errors="replace").splitlines():
        offset = raw.find(PREFIX)
        if offset < 0: continue
        record = raw[offset + len(PREFIX):]
        kind, _, payload = record.partition(" ")
        fields = dict(FIELD.findall(payload))
        if kind == "BEGIN":
            if begin is not None: raise ValueError("duplicate BEGIN")
            begin = fields
        elif kind == "RESULT":
            name = fields.get("name")
            if not name or name in cases: raise ValueError(f"missing or duplicate case {name}")
            if fields.get("status") not in {"PASS", "FAIL"}: raise ValueError(f"invalid status for {name}")
            if not fields.get("duration_ms", "").isdigit(): raise ValueError(f"invalid duration for {name}")
            cases[name] = fields
        elif kind == "METRIC":
            name = fields.get("name")
            if name: metrics[name] = payload.partition(" text=")[2]
        elif kind == "END":
            if end is not None: raise ValueError("duplicate END")
            end = fields
    if begin is None or begin.get("version") != "1": raise ValueError("missing BEGIN")
    if end is None: raise ValueError("missing END")
    missing = [name for name in EXPECTED if name not in cases]
    extra = [name for name in cases if name not in EXPECTED]
    if missing or extra: raise ValueError(f"case mismatch missing={missing} extra={extra}")
    passed = sum(x["status"] == "PASS" for x in cases.values())
    failed = len(cases) - passed
    if (int(end.get("total", -1)), int(end.get("pass", -1)), int(end.get("fail", -1))) != (len(EXPECTED), passed, failed):
        raise ValueError("END summary mismatch")
    return {"schema_version": 1, "begin": begin, "summary": {"total": len(EXPECTED), "pass": passed, "fail": failed},
            "cases": [{**cases[name], "metric": metrics.get(name, "")} for name in EXPECTED], "passed": failed == 0}

def main() -> int:
    parser = argparse.ArgumentParser(); parser.add_argument("serial", type=Path); parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    try: result = parse(args.serial)
    except (OSError, ValueError) as error:
        print(f"invalid stress result: {error}", file=sys.stderr); return 2
    encoded = json.dumps(result, indent=2, sort_keys=True) + "\n"
    if args.output: args.output.write_text(encoded)
    else: sys.stdout.write(encoded)
    return 0 if result["passed"] else 1
if __name__ == "__main__": raise SystemExit(main())
