#!/usr/bin/env python3
"""Inspect dumped 0x1C8 precompiled graphics pipeline binary blobs."""
from pathlib import Path
import sys

d = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(r"C:\Games\GTA VI\versus\bench-logs\1c8_dump")
for name in ["call1_pmeta.bin", "call1_vs_meta.bin", "call1_ps_meta.bin", "call1_gstate.bin"]:
    p = d / name
    if not p.exists():
        continue
    b = p.read_bytes()
    print(f"=== {name} ({len(b)} B) ===")
    for off in range(0, min(len(b), 192), 16):
        chunk = b[off : off + 16]
        u32s = " ".join(
            f"{int.from_bytes(chunk[i : i + 4], 'little'):08x}"
            for i in range(0, len(chunk) - 3, 4)
        )
        asc = "".join(chr(c) if 32 <= c < 127 else "." for c in chunk)
        print(f"  +{off:03x}: {u32s:<36}  {asc}")
