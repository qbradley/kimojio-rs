import collections
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess

ROOT = Path(os.environ.get("PROFILE_ARTIFACT_ROOT", Path(__file__).resolve().parent))
BINARY = Path(os.environ.get(
    "PROFILE_BINARY", "/workspace/kimojio-rs/target/wrapper-lab/frozen/final/keepalive_bench"
))
SYMBOL_RE = re.compile(r"^\s*[0-9a-f]+ ([^ +]+)(?:\+0x([0-9a-f]+))?", re.MULTILINE)

raw = {}
symbols = set()
for mode in ("native", "coalesced"):
    blocks = (ROOT / f"{mode}-stacks.txt").read_text().split("\n\n")
    raw[mode] = [SYMBOL_RE.findall(block) for block in blocks if SYMBOL_RE.search(block)]
    symbols.update(symbol for block in raw[mode] for symbol, _ in block)
symbols = sorted(symbols)
names = dict(zip(symbols, subprocess.check_output(
    ["c++filt", "-s", "rust"], input="\n".join(symbols), text=True
).splitlines()))

result = {
    "binary": str(BINARY),
    "sha256": hashlib.sha256(BINARY.read_bytes()).hexdigest(),
    "revision": os.environ.get("PROFILE_REVISION", "da081a89349ef1cc5619496e74556a898dcb98c6"),
    "profiles": {},
}
for mode, blocks in raw.items():
    buckets = collections.Counter()
    leaves = collections.defaultdict(collections.Counter)
    allocations = collections.Counter()
    for frames in blocks:
        decoded = [names.get(symbol, symbol) for symbol, _ in frames]
        client = any("driver::NativeConnection" in name or "driver::Connection<" in name for name in decoded)
        server = any("driver" in name and "run_pair" in name for name in decoded)
        owner = "client" if client and not server else "server" if server and not client else "shared"
        buckets[owner] += 1
        leaves[owner][decoded[0]] += 1
        if frames[0][0] == "malloc":
            if any("WaitData" in name for name in decoded):
                kind = "WaitData"
            elif any("SleepFuture" in name for name in decoded):
                kind = "SleepFuture"
            else:
                kind = next((name for name in decoded[1:] if "alloc::" not in name and "alloc[" not in name), "unresolved")
            allocations[kind] += 1
    result["profiles"][mode] = {
        "samples": len(blocks),
        "buckets": dict(buckets),
        "top_leaves": {owner: counter.most_common(10) for owner, counter in leaves.items()},
        "malloc_stacks": allocations.most_common(),
    }

raw_nm = {}
for line in (ROOT / "symbols.txt").read_text().splitlines():
    parts = line.split()
    if len(parts) == 4:
        raw_nm[int(parts[0], 16)] = parts[3]

selected = {}
for line in (ROOT / "symbols-demangled.txt").read_text().splitlines():
    parts = line.split(maxsplit=3)
    if len(parts) != 4:
        continue
    address, size, _, name = parts
    label = None
    if "::Core<" in name and "::next::<" in name:
        label = "core-next"
    elif "PollFn" in name and "next_input" in name and "NativeIo" in name:
        label = "input-poll"
    elif "next_input" in name and "NativeIo" in name and "drop_glue" not in name:
        label = "input-async"
    elif "::Slot<" in name and "::poll" in name:
        label = "slot-read" if "read_once" in name else "slot-write"
    elif "::event::" in name and "NativeIo" in name and ">::map" not in name:
        label = "event-client" if "NativeConnection" in name else "event-server"
    if label:
        selected[label] = (int(address, 16), int(size, 16), name)

result["sites"] = {}
for label, (address, size, name) in selected.items():
    symbol = raw_nm[address]
    assembly = subprocess.check_output([
        "objdump", "-d", "-C", f"--start-address={address}", f"--stop-address={address + size}", str(BINARY)
    ], text=True)
    (ROOT / f"{label}.asm").write_text(assembly)
    lines = assembly.splitlines()
    offsets = {}
    for mode, blocks in raw.items():
        offsets[mode] = []
        for frames in blocks:
            found = next((int(offset, 16) for frame, offset in frames
                          if frame.split(".llvm.")[0] == symbol.split(".llvm.")[0] and offset), None)
            if found is not None:
                offsets[mode].append(found)
    sites = []
    for i, line in enumerate(lines):
        if "call" not in line:
            continue
        copy_name = next((token for token in ("memcpy", "memmove", "memset") if token in line), None)
        register = re.search(r"call\s+\*%(r\w+)", line)
        if copy_name is None and register:
            destination = re.compile(r",%" + register[1] + r"(?:\s|$)")
            for earlier in reversed(lines[max(0, i - 24):i]):
                if destination.search(earlier):
                    copy_name = next((token for token in ("memcpy", "memmove", "memset") if token in earlier), None)
                    break
        if copy_name is None:
            continue
        call_address = int(line.strip().split(":")[0], 16)
        prior = "\n".join(lines[max(0, i - 8):i])
        writes = [line for line in prior.splitlines() if re.search(r",%(?:edx|rdx)(?:\s|$)", line)]
        sizes = re.findall(r"mov\s+\$0x([0-9a-f]+),%edx", writes[-1]) if writes else []
        offset = call_address - address
        sites.append({
            "offset": hex(offset),
            "immediate_edx": int(sizes[-1], 16) if sizes else None,
            "instruction": line.strip(),
            "copy_name": copy_name,
            "nearby_samples": {mode: sum(abs(value - offset) <= 0x18 for value in values) for mode, values in offsets.items()},
        })
    result["sites"][label] = {
        "address": hex(address), "size": hex(size), "name": name,
        "parent_samples": {mode: len(values) for mode, values in offsets.items()},
        "direct_calls": sites,
        "vector_instruction_count": len(re.findall(r"\b(?:movaps|movups|movdqa|movdqu)\b", assembly)),
        "rep_movs_instruction_count": len(re.findall(r"\brep\s+movs", assembly)),
    }
(ROOT / "analysis.json").write_text(json.dumps(result, indent=2) + "\n")
for mode, profile in result["profiles"].items():
    print(mode, "samples", profile["samples"], "buckets", profile["buckets"])
    for owner, leaves in profile["top_leaves"].items():
        print(owner, [pair for pair in leaves if pair[0] != "[unknown]"][:3])
    print("malloc", profile["malloc_stacks"][:6])
for label, site in result["sites"].items():
    print(label, site["address"], site["size"], "parent samples", site["parent_samples"],
          "vector sites", site["vector_instruction_count"])
    for call in site["direct_calls"]:
        print(call["offset"], call["immediate_edx"], call["nearby_samples"])
