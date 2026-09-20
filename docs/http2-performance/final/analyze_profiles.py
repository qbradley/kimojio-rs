#!/usr/bin/env python3
"""Classify frozen stacks without assigning folded endpoint helpers to one role."""
import collections
import hashlib
import json
from pathlib import Path
import re
import sys

frozen, output = map(Path, sys.argv[1:3])


def normalize(text):
    return re.sub(r"\[[0-9a-f]{1,16}\]", "", text)


def roles(symbol):
    if any(marker in symbol for marker in ["::Pair<", "::transfer::", "::execute::"]):
        return False, False
    client = "::h2::client::H2Client" in symbol
    server = "::h2::server::H2Server" in symbol
    if "as composition_bench::composition_bench_support::Endpoint>::drive" in symbol:
        client |= "::Client<" in symbol
        server |= "::Server<" in symbol
    return client, server


groups = collections.defaultdict(list)
for line in normalize((frozen / "nm.txt").read_text()).splitlines():
    match = re.match(r"([0-9a-f]+)\s+([0-9a-f]+)\s+\w\s+(.*)", line)
    if match:
        groups[(match[1], match[2])].append(match[3])
mixed_groups = [
    dict(address=address, size=size, symbols=symbols)
    for (address, size), symbols in groups.items()
    if any(roles(symbol)[0] for symbol in symbols)
    and any(roles(symbol)[1] for symbol in symbols)
]
ambiguous = {symbol for group in mixed_groups for symbol in group["symbols"]}
(output / "role-aliases.json").write_text(
    json.dumps(dict(
        binary_sha256=hashlib.sha256((frozen / "composition_bench").read_bytes()).hexdigest(),
        mixed_groups=mixed_groups,
    ), indent=2) + "\n"
)


def owner(frames):
    client = server = False
    for frame in frames:
        symbol = re.sub(r"\+0x[0-9a-f]+$", "", frame)
        if symbol in ambiguous:
            continue
        c, s = roles(symbol)
        client |= c
        server |= s
    return "client" if client and not server else "server" if server and not client else "shared"


client_witness = "<kimojio_fsm_http2::server::h2::client::H2Client>::accept_driver_bytes_ref"
server_witness = "<kimojio_fsm_http2::server::h2::server::H2Server>::accept_driver_event_bytes_ref"
assert owner([client_witness]) == "client"
assert owner([server_witness]) == "server"
assert owner([client_witness, server_witness]) == "shared"
assert owner(["[unknown]"]) == "shared"
for symbol in ambiguous:
    assert owner([symbol]) == "shared"
    assert owner([symbol, client_witness]) == "client"
    assert owner([symbol, server_witness]) == "server"


for name in ["direct-empty", "selected-empty", "direct-empty128", "direct-1m"]:
    samples = []
    for block in normalize((frozen / f"{name}.demangled.txt").read_text()).split("\n\n"):
        frames = []
        for line in block.splitlines()[1:]:
            match = re.match(
                r"\s+[0-9a-f]+\s+(.*)\s+\((?:inlined|/[^)]*|\[unknown\])\)\s*$",
                line,
            )
            if match:
                frames.append(match[1])
        if frames:
            samples.append(frames)
    buckets = collections.Counter(owner(frames) for frames in samples)
    leaves = collections.Counter(re.sub(r"\+0x[0-9a-f]+$", "", s[0]) for s in samples)
    stacks = {}
    for bucket in ["client", "server", "shared"]:
        counts = collections.Counter(
            tuple(re.sub(r"\+0x[0-9a-f]+$", "", frame) for frame in frames[:12])
            for frames in samples if owner(frames) == bucket
        )
        stacks[bucket] = [
            dict(samples=count, frames=frames)
            for frames, count in counts.most_common(8)
        ]
    result = dict(
        samples=len(samples), buckets=dict(buckets),
        leaves=leaves.most_common(35), stacks=stacks,
        leaf_offsets=collections.Counter(s[0] for s in samples).most_common(),
        ignored_ambiguous_role_symbols=len(ambiguous),
    )
    (output / f"{name}-stacks.json").write_text(json.dumps(result, indent=2) + "\n")
    print(name, result["samples"], result["buckets"])
