#!/usr/bin/env python3
"""Validate an Embewi partition table CSV (layout invariants + Agent size gate).

usage: check-partitions.py <partitions.csv> --flash-size 16M [--agent-bin agent.bin]
                           [--min-agent-margin-percent 25]

Checks: no overlap, 4 KiB (data) / 64 KiB (app) alignment, inside the flash, nothing
below 0x9000, the five Agent partitions exactly as in S15 (nvs, otadata, phy_init,
ota_0, ota_1), and -- when the Workload partitions exist -- all three present, subtype
`undefined`, equal slots >= 256 KiB, `wl_meta` >= two erase units. With --agent-bin the
real .bin written to ota_x must fit ota_0/ota_1 (a clear error otherwise) and a growth
margin is reported (and enforced when --min-agent-margin-percent is given).
"""
import argparse
import os
import sys

ERASE = 0x1000
APP_ALIGN = 0x10000
FIRST = 0x9000
MIN_WORKLOAD_SLOT = 256 * 1024

# The Agent partitions, frozen since S15 (name -> (type, subtype, offset, size)).
AGENT_REFERENCE = {
    "nvs": ("data", "nvs", 0x9000, 0x6000),
    "otadata": ("data", "ota", 0xF000, 0x2000),
    "phy_init": ("data", "phy", 0x11000, 0x1000),
    "ota_0": ("app", "ota_0", 0x20000, 0x180000),
    "ota_1": ("app", "ota_1", 0x1A0000, 0x180000),
}
WORKLOAD = ("wl_meta", "workload_a", "workload_b")


def num(text):
    text = text.strip()
    mult = 1
    if text[-1:].upper() == "K":
        mult, text = 1024, text[:-1]
    elif text[-1:].upper() == "M":
        mult, text = 1024 * 1024, text[:-1]
    return int(text, 0) * mult


def parse(path):
    rows = []
    for raw in open(path):
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        f = [x.strip() for x in line.split(",")]
        if len(f) < 5:
            raise SystemExit(f"{path}: malformed line: {raw.strip()}")
        rows.append({"name": f[0], "type": f[1], "sub": f[2], "off": num(f[3]), "size": num(f[4])})
    return rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("csv")
    ap.add_argument("--flash-size", required=True)
    ap.add_argument("--agent-bin")
    ap.add_argument("--min-agent-margin-percent", type=float)
    args = ap.parse_args()
    flash = num(args.flash_size)
    rows = parse(args.csv)
    errors = []

    for i, p in enumerate(rows):
        align = APP_ALIGN if p["type"] == "app" else ERASE
        if p["off"] % align or p["size"] % ERASE:
            errors.append(f"{p['name']}: offset {p['off']:#x}/size {p['size']:#x} not aligned to {align:#x}/{ERASE:#x}")
        if p["off"] < FIRST:
            errors.append(f"{p['name']}: starts below {FIRST:#x}")
        if p["off"] + p["size"] > flash:
            errors.append(f"{p['name']}: ends at {p['off'] + p['size']:#x}, beyond the {flash:#x}-byte flash")
        for q in rows[i + 1:]:
            if p["off"] < q["off"] + q["size"] and q["off"] < p["off"] + p["size"]:
                errors.append(f"{p['name']} overlaps {q['name']}")

    by = {p["name"]: p for p in rows}
    for name, (typ, sub, off, size) in AGENT_REFERENCE.items():
        p = by.get(name)
        if p is None or (p["type"], p["sub"], p["off"], p["size"]) != (typ, sub, off, size):
            errors.append(f"Agent partition {name} differs from the S15 reference {typ}/{sub} {off:#x}/{size:#x}")

    present = [n for n in WORKLOAD if n in by]
    if present and len(present) != 3:
        errors.append(f"Workload partitions are all-or-nothing, found only {present}")
    if len(present) == 3:
        for n in WORKLOAD:
            if by[n]["type"] != "data" or by[n]["sub"] != "undefined":
                errors.append(f"{n}: must be data/undefined (the Agent's table parser panics on unknown subtypes)")
        if by["wl_meta"]["size"] < 2 * ERASE:
            errors.append("wl_meta: needs two erase units (two OTM2 copies)")
        if by["workload_a"]["size"] != by["workload_b"]["size"]:
            errors.append("workload_a and workload_b must have the same size")
        if by["workload_a"]["size"] < MIN_WORKLOAD_SLOT:
            errors.append(f"workload slots below the {MIN_WORKLOAD_SLOT} B minimum")

    if args.agent_bin:
        size = os.path.getsize(args.agent_bin)
        for slot in ("ota_0", "ota_1"):
            cap = by[slot]["size"]
            if size > cap:
                errors.append(
                    f"Agent image too large for {slot}: {size} B > {cap} B (over by {size - cap} B)")
        cap = min(by["ota_0"]["size"], by["ota_1"]["size"])
        margin = (cap - size) * 100.0 / size
        print(f"Agent .bin {size} B, slot {cap} B, growth margin {cap - size} B ({margin:.1f} %)")
        if args.min_agent_margin_percent is not None and margin < args.min_agent_margin_percent:
            errors.append(f"Agent growth margin {margin:.1f} % is below {args.min_agent_margin_percent} %")

    used_end = max(p["off"] + p["size"] for p in rows)
    print(f"{args.csv}: {len(rows)} partitions, last ends at {used_end:#x}, flash {flash:#x}, free tail {flash - used_end:#x} B")
    if errors:
        print("FAIL:")
        for e in errors:
            print("  -", e)
        return 1
    print("OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
