#!/usr/bin/env python3
"""Render selected rows from the complete machine-readable trial summary."""
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
summary = json.loads((root / "evidence/summary.json").read_text())
by_key = {(r["mode"], r["case"], r["concurrency"], r["fragment"], r["phase"]): r
          for r in summary}
selected = [("empty", c, 65536, "steady") for c in [1, 8, 64, 128]]
selected += [("empty", 1, f, "steady") for f in [17, 1024]]
selected += [(case, 1, f, "steady") for case in ["duplex", "32k", "1m"]
             for f in [17, 1024, 65536]]
selected += [("duplex", 128, 65536, "steady"), ("32k", 128, 65536, "steady"),
             ("paused", 128, 17, "steady"), ("paused", 128, 65536, "steady")]
selected += [("empty", 1, f, "construct") for f in [17, 1024, 65536]]
lines = [
    "# Timing results",
    "",
    "The table uses microseconds per completed exchange.",
    "Each cell shows the median and the minimum–maximum range of five trials.",
    "The percentages compare route medians with the direct median.",
    "They are observations, not confidence intervals or universal overhead bounds.",
    "",
    "| Case | C | Fragment | Phase | Direct µs | Selected µs | Auto µs | Selected Δ | Auto Δ |",
    "|---|---:|---:|---|---:|---:|---:|---:|---:|",
]
for case, c, f, phase in selected:
    rows = [by_key[(mode, case, c, f, phase)] for mode in ["direct", "selected", "auto"]]
    if any(row["blocked"] for row in rows):
        continue
    assert all(row["trials"] == 5 for row in rows)
    values = [f'{row["median_ns"]/1000:.3f} ({row["min_ns"]/1000:.3f}–{row["max_ns"]/1000:.3f})'
              for row in rows]
    deltas = [f'{100*(row["median_ns"]/rows[0]["median_ns"]-1):+.1f}%' for row in rows[1:]]
    lines.append("| " + " | ".join([case, str(c), str(f), phase] + values + deltas) + " |")
lines += ["", "## Blocked direct-core cells", "",
          "The selected and auto routes fail the same cells.",
          "No elapsed time from these failures enters the table.", "",
          "| Case | Concurrency | Fragment |", "|---|---:|---:|"]
for row in summary:
    if row["mode"] == "direct" and row["blocked"]:
        lines.append(f'| {row["case"]} | {row["concurrency"]} | {row["fragment"]} |')
(root / "RESULTS.md").write_text("\n".join(lines) + "\n")
