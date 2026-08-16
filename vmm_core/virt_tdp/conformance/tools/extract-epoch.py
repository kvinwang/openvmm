#!/usr/bin/env python3
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.

import argparse
from pathlib import Path

PREFIX = "VIRT_TDP_CTS "


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("serial", type=Path)
    parser.add_argument("index", type=int)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    if args.index < 1:
        parser.error("index must be positive")
    epochs: list[list[str]] = []
    current: list[str] | None = None
    for line in args.serial.read_text(errors="replace").splitlines(keepends=True):
        if f"{PREFIX}BEGIN " in line:
            if current is not None:
                raise SystemExit("nested BEGIN record")
            current = [line]
        elif current is not None:
            current.append(line)
            if f"{PREFIX}END " in line:
                epochs.append(current)
                current = None
    if len(epochs) < args.index:
        raise SystemExit(f"epoch {args.index} is incomplete or absent")
    args.output.write_text("".join(epochs[args.index - 1]))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
