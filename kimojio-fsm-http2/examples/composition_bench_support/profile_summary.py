#!/usr/bin/env python3
"""Classify perf DWARF stacks without assigning shared leaves to one endpoint."""
import collections
import json
import pathlib
import re
import sys

text = re.sub(r"\[[0-9a-f]{16}\]", "", pathlib.Path(sys.argv[1]).read_text())
samples = []
for block in text.split("\n\n"):
    frames = []
    for line in block.splitlines()[1:]:
        match = re.match(r"\s+([0-9a-f]+)\s+(.*)\s+\((?:inlined|/[^)]*|\[unknown\])\)\s*$", line)
        if match:
            frames.append(match.group(2))
    if frames:
        samples.append(frames)


def owner(frames):
    client = server = False
    for frame in frames:
        if "::Pair<" in frame or "::transfer::" in frame or "::execute::" in frame:
            continue
        if "as composition_bench::composition_bench_support::Endpoint>::drive" in frame:
            client |= "::Client<" in frame
            server |= "::Server<" in frame
        client |= "::h2::client::H2Client" in frame
        server |= "::h2::server::H2Server" in frame
        client |= "::H2Endpoint<" in frame and "::H2ClientStream>" in frame
        server |= "::H2Endpoint<" in frame and "::H2ServerStream>" in frame
    return "client" if client and not server else "server" if server and not client else "shared"


buckets = collections.Counter(owner(s) for s in samples)
leaves = collections.Counter(re.sub(r"\+0x[0-9a-f]+$", "", s[0]) for s in samples)
stacks = {}
for bucket in ["client", "server", "shared"]:
    counts = collections.Counter(tuple(re.sub(r"\+0x[0-9a-f]+$", "", f) for f in s[:12])
                                 for s in samples if owner(s) == bucket)
    stacks[bucket] = [dict(samples=count, frames=frames) for frames, count in counts.most_common(8)]
sites = collections.Counter(s[0] for s in samples)
result = dict(samples=len(samples), buckets=dict(buckets),
              leaves=leaves.most_common(35), stacks=stacks, leaf_offsets=sites.most_common())
pathlib.Path(sys.argv[2]).write_text(json.dumps(result, indent=2) + "\n")
