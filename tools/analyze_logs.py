#!/usr/bin/env python3
"""Operation Zero Warnings — Reusable Versus runtime console log analyzer.

Parses Versus stdout/stderr logs, strips ANSI escapes, groups WARN/ERROR
entries by root cause, ranks shader translation failures, and compares runs
against a persistent JSON diagnostic database.
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

ANSI_RE = re.compile(r"\x1b\[[0-9;]*[A-Za-z]")
LOG_RE = re.compile(
    r"^(?P<ts>\S+)\s+(?P<level>WARN|ERROR|INFO)\s+(?P<target>\S+):\s+(?P<msg>.*)$"
)
SHADER_FAIL_RE = re.compile(
    r'shader does not translate.*?stage="(?P<stage>[^"]+)"\s+hash=(?P<hash>[0-9a-f]+)\s+error=(?P<err>.+)$'
)
HEX_OFFSET_RE = re.compile(r"\bat 0x[0-9a-fA-F]+:")
HEX_PTR_RE = re.compile(r"0x[0-9a-fA-F]{6,}")
NUM_KV_RE = re.compile(r"\b(calls|frame|index|count|rejected|created|parsed|size|ms|mb)=\S+")
PRECOMPILED_RE = re.compile(
    r"precompiled pipeline stage telemetry\s+slot=(?P<slot>\S+)\s+trigger=\"(?P<trigger>[^\"]+)\"\s+"
    r"accepted=(?P<accepted>\d+)\s+parsed=(?P<parsed>\d+)\s+rejected=(?P<rejected>\d+)\s+"
    r"host_created=(?P<host_created>\d+)\s+host_failed=(?P<host_failed>\d+)\s+"
    r"executed=(?P<executed>\d+)\s+dropped=(?P<dropped>\d+)"
)


def normalize_shader_error(err: str) -> str:
    err = HEX_OFFSET_RE.sub("at 0x...:", err)
    err = re.sub(r"^converter\s+\S+\s+v\d+:\s*", "", err)
    return err.strip()


def normalize_warning(target: str, msg: str) -> str:
    # Strip structured key=value fields that vary per call
    base = msg.split(" slot=")[0].split(" calls=")[0].split(" hash=")[0]
    base = HEX_PTR_RE.sub("0x...", base)
    base = NUM_KV_RE.sub(r"\1=...", base)
    return f"{target}: {base.strip()}"


def analyze_log(path: Path) -> dict:
    warn_groups: Counter[str] = Counter()
    error_groups: Counter[str] = Counter()
    shader_failures: dict[str, set[str]] = defaultdict(set)
    shader_stages: Counter[str] = Counter()
    missing_slots: Counter[str] = Counter()
    precompiled: dict[str, dict] = {}
    total_warns = 0
    total_errors = 0

    raw_bytes = path.read_bytes()
    enc = "utf-16" if raw_bytes.startswith((b"\xff\xfe", b"\xfe\xff")) else "utf-8"
    if True:
        for raw in raw_bytes.decode(enc, errors="replace").splitlines():
            line = ANSI_RE.sub("", raw).strip()
            m = LOG_RE.match(line)
            if not m:
                continue
            level = m.group("level")
            target = m.group("target")
            msg = m.group("msg")

            pm = PRECOMPILED_RE.search(msg)
            if pm:
                precompiled[pm.group("slot")] = {
                    k: int(pm.group(k))
                    for k in (
                        "accepted",
                        "parsed",
                        "rejected",
                        "host_created",
                        "host_failed",
                        "executed",
                        "dropped",
                    )
                }
                continue

            if level == "WARN":
                total_warns += 1
                sm = SHADER_FAIL_RE.search(msg)
                if sm:
                    cat = normalize_shader_error(sm.group("err"))
                    shader_failures[cat].add(sm.group("hash"))
                    shader_stages[sm.group("stage")] += 1
                    warn_groups[f"shader_fail: {cat}"] += 1
                else:
                    if "a pipeline creation method that is not implemented" in msg:
                        slot_m = re.search(r"slot=(0x[0-9a-fA-F]+)", msg)
                        if slot_m:
                            missing_slots[slot_m.group(1)] += 1
                    warn_groups[normalize_warning(target, msg)] += 1
            elif level == "ERROR":
                total_errors += 1
                error_groups[normalize_warning(target, msg)] += 1

    unique_failed_shaders = set().union(*shader_failures.values()) if shader_failures else set()
    return {
        "log": str(path),
        "total_warns": total_warns,
        "total_errors": total_errors,
        "unique_warn_categories": len(warn_groups),
        "unique_error_categories": len(error_groups),
        "unique_failed_shaders": len(unique_failed_shaders),
        "shader_stages": dict(shader_stages.most_common()),
        "shader_failure_categories": [
            {"category": cat, "unique_shaders": len(hashes), "sample_hashes": sorted(hashes)[:3]}
            for cat, hashes in sorted(shader_failures.items(), key=lambda kv: -len(kv[1]))
        ],
        "top_warnings": [
            {"category": cat, "count": cnt} for cat, cnt in warn_groups.most_common(25)
        ],
        "top_errors": [
            {"category": cat, "count": cnt} for cat, cnt in error_groups.most_common(15)
        ],
        "missing_pipeline_slots": dict(missing_slots),
        "precompiled_telemetry": precompiled,
    }


def main() -> int:
    ap = argparse.ArgumentParser(description="Analyze Versus runtime logs.")
    ap.add_argument("log", type=Path, help="Path to stderr/stdout log file")
    ap.add_argument("--json-out", type=Path, help="Write JSON report to this path")
    ap.add_argument("--compare", type=Path, help="Compare against a previous JSON report")
    args = ap.parse_args()

    report = analyze_log(args.log)
    if args.json_out:
        args.json_out.parent.mkdir(parents=True, exist_ok=True)
        args.json_out.write_text(json.dumps(report, indent=2), encoding="utf-8")

    print(f"=== Versus Console Analysis: {args.log.name} ===")
    print(
        f"Total WARN: {report['total_warns']} ({report['unique_warn_categories']} categories) | "
        f"Total ERROR: {report['total_errors']} ({report['unique_error_categories']} categories) | "
        f"Unique Failed Shaders: {report['unique_failed_shaders']}"
    )
    if report["precompiled_telemetry"]:
        print("\n--- Precompiled Pipeline Telemetry ---")
        for slot, stats in report["precompiled_telemetry"].items():
            print(f"  {slot}: {stats}")

    print("\n--- Top Shader Translation Failure Categories (by unique shaders) ---")
    for item in report["shader_failure_categories"][:15]:
        print(f"  {item['unique_shaders']:4d}  {item['category']}  (e.g. {', '.join(item['sample_hashes'])})")

    print("\n--- Top Non-Shader Warnings ---")
    non_shader = [w for w in report["top_warnings"] if not w["category"].startswith("shader_fail:")]
    for item in non_shader[:10]:
        print(f"  {item['count']:4d}  {item['category']}")

    if report["top_errors"]:
        print("\n--- Errors ---")
        for item in report["top_errors"][:10]:
            print(f"  {item['count']:4d}  {item['category']}")

    if args.compare and args.compare.exists():
        prev = json.loads(args.compare.read_text(encoding="utf-8"))
        print(f"\n=== Delta vs {args.compare.name} ===")
        print(
            f"  Unique Failed Shaders: {prev['unique_failed_shaders']} -> {report['unique_failed_shaders']} "
            f"({report['unique_failed_shaders'] - prev['unique_failed_shaders']:+d})"
        )
        print(
            f"  Total WARN: {prev['total_warns']} -> {report['total_warns']} "
            f"({report['total_warns'] - prev['total_warns']:+d})"
        )
        prev_cats = {c["category"]: c["unique_shaders"] for c in prev["shader_failure_categories"]}
        curr_cats = {c["category"]: c["unique_shaders"] for c in report["shader_failure_categories"]}
        for cat in sorted(set(prev_cats) | set(curr_cats)):
            d = curr_cats.get(cat, 0) - prev_cats.get(cat, 0)
            if d != 0:
                print(f"  {d:+4d}  {cat} ({prev_cats.get(cat, 0)} -> {curr_cats.get(cat, 0)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
