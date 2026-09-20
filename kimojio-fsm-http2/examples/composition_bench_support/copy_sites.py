#!/usr/bin/env python3
"""Join static memcpy sites to symbol-relative perf samples, never runtime IPs."""
import collections
import hashlib
import json
import pathlib
import re
import sys

frozen = pathlib.Path(sys.argv[1])
destination = pathlib.Path(sys.argv[2])
binary_sha256 = hashlib.sha256((frozen / "composition_bench").read_bytes()).hexdigest()
profiles = [name for name in ["direct-empty", "selected-empty", "direct-empty128", "direct-1m"]
            if (frozen / f"{name}.demangled.txt").exists()]
assert profiles, "no recorded profile stacks"
samples = {}
for name in profiles:
    by_symbol = collections.defaultdict(list)
    text = re.sub(r"\[[0-9a-f]{1,16}\]", "", (frozen / f"{name}.demangled.txt").read_text())
    for block in text.split("\n\n"):
        seen = set()
        for line in block.splitlines()[1:]:
            match = re.match(r"\s+[0-9a-f]+\s+(.*)\+0x([0-9a-f]+) \((/[^)]+)\)$", line)
            if match and match[3].endswith("/composition_bench"):
                key = (match[1], int(match[2], 16))
                if key not in seen:
                    by_symbol[key[0]].append(key[1])
                    seen.add(key)
    samples[name] = by_symbol

sites = []
symbol = ""
start = 0
previous = []
for line in (frozen / "objdump.txt").read_text().splitlines():
    header = re.match(r"([0-9a-f]+) <(.*)>:", line)
    if header:
        start, symbol = int(header[1], 16), header[2]
        previous = []
        continue
    instruction = re.match(r"\s+([0-9a-f]+):\s+", line)
    if not instruction:
        continue
    address = int(instruction[1], 16)
    if re.search(r"\bcall\b.*\bmem(cpy|move|set)@", line):
        offset = address - start
        hits = {name: sum(abs(value - offset) <= 0x18 for value in by_symbol[symbol])
                for name, by_symbol in samples.items()}
        parents = {name: len(by_symbol[symbol]) for name, by_symbol in samples.items()}
        if any(parents.values()):
            sites.append(dict(symbol=symbol, address=hex(address), offset=hex(offset),
                              instruction=line.strip(), preceding=previous[-9:],
                              hits=hits, parent_samples=parents))
    previous.append(line.strip())
sites.sort(key=lambda s: sum(s["hits"].values()), reverse=True)
if binary_sha256 == "205f19afeaa93d7d6161a3f32dd3359098bf2805d2b0bca08c5cd8a7c077c527":
    symbol = "<kimojio_fsm_http2::engine::Connection<&[u8]>>::next::<composition_bench::composition_bench_support::Ports>"
    offset = 0xed3
    sites.append(dict(symbol=symbol, address=hex(0x49f10 + offset), offset=hex(offset),
                      instruction="movups/movaps block: new Option<WriteOp> moved into Ports::write storage",
                      preceding=[], hits={name: sum(abs(value - offset) <= 0x18 for value in by_symbol[symbol])
                                          for name, by_symbol in samples.items()},
                      parent_samples={name: len(by_symbol[symbol]) for name, by_symbol in samples.items()}))
else:
    print("New binary: inspect inline vector moves separately; no historical address reused.",
          file=sys.stderr)
destination.write_text(json.dumps(sites, indent=2) + "\n")
for site in sites[:20]:
    print(site["offset"], site["hits"], site["parent_samples"], site["symbol"])
    print("\n".join(site["preceding"][-4:]))
