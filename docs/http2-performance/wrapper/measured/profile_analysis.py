"""Conservative endpoint attribution and sampled machine-code call-site inventory."""
import collections
import json
import os
from pathlib import Path
import re
import subprocess

HERE = Path(__file__).resolve().parent
ROOT = Path("/workspace/kimojio-rs/target/http2-program/build-http2-wrapper-performance/measured-6e003746")
PROFILES = ROOT / "profiles"
FRAME = re.compile(r"^\s*([0-9a-f]+)\s+(.+)\s+\((.*)\)$")


def normalize(name):
    return re.sub(r"\[[0-9a-f]{8,}\]", "", name)


def roles(name):
    client = ("driver::NativeConnection>::run" in name
              or "driver::Connection>::run" in name
              or ("driver::Connection<" in name and ">>::run" in name)
              or "support::exchange" in name
              or "driver::Client>::send" in name
              or "server::h2::client::H2Client>" in name)
    server = ("driver::serve_connection" in name or "driver::run_handler" in name
              or "support::handler" in name or "server::h2::server::H2Server>" in name)
    return {"client" if client else None, "server" if server else None} - {None}


def symbols(binary):
    raw = subprocess.check_output(["nm", "-n", "-S", "--defined-only", str(binary)], text=True)
    (PROFILES / (binary.name + "-nm.txt")).write_text(raw)
    entries = []
    for line in raw.splitlines():
        parts = line.split()
        if len(parts) == 4 and parts[2] in ("t", "T", "w", "W"):
            entries.append((parts[3], int(parts[0], 16), int(parts[1], 16)))
    names = [name for name, _, _ in entries]
    clean = subprocess.check_output(["c++filt", "-s", "rust"], input="\n".join(names) + "\n", text=True).splitlines()
    result = {name: {"start": start, "size": size, "name": normalize(human)}
              for (name, start, size), human in zip(entries, clean)}
    aliases = collections.defaultdict(set)
    for value in result.values():
        aliases[value["start"], value["size"]].update(roles(value["name"]))
    for value in result.values():
        value["ambiguous_endpoint_alias"] = len(aliases[value["start"], value["size"]]) > 1
    return result


def read_samples(path):
    samples = []
    current = []
    for line in path.read_text().splitlines() + [""]:
        if not line.strip():
            if current:
                samples.append(current)
                current = []
            continue
        if "cpu-clock:u:" in line:
            continue
        match = FRAME.match(line)
        if not match:
            continue
        ip, text, dso = match.groups()
        offset = re.search(r"\+0x([0-9a-f]+)$", text)
        name = text[:offset.start()] if offset else text
        current.append({"ip": int(ip, 16), "symbol": name,
                        "offset": int(offset[1], 16) if offset else 0, "dso": dso})
    return samples


def disassemble(binary, sym):
    path = PROFILES / (binary.name + "-objdump.txt")
    with path.open("w") as output:
        subprocess.run(["objdump", "-d", "--no-show-raw-insn", "-M", "intel", str(binary)],
                       stdout=output, check=True)
    relocations = {}
    by_address = {value["start"]: value["name"] for value in sym.values()}
    for line in subprocess.check_output(["objdump", "-R", str(binary)], text=True).splitlines():
        parts = line.split()
        if len(parts) >= 3 and re.fullmatch("[0-9a-f]+", parts[0]):
            target = parts[2]
            if target.startswith("*ABS*+0x"):
                target = by_address.get(int(target[len("*ABS*+0x"):], 16), target)
            relocations[int(parts[0], 16)] = target
    instructions = []
    for line in path.read_text().splitlines():
        match = re.match(r"^\s*([0-9a-f]+):\s+(.+)$", line)
        if match:
            instructions.append((int(match[1], 16), match[2]))
    calls = []
    for index, (address, instruction) in enumerate(instructions):
        if not instruction.startswith("call"):
            continue
        target = instruction
        indirect = re.search(r"#\s*([0-9a-f]+)", instruction)
        if indirect and int(indirect[1], 16) in relocations:
            target = relocations[int(indirect[1], 16)]
        if any(word in target for word in ("memcpy", "memmove", "memset", "memcmp", "bcmp",
                                           "malloc", "calloc", "realloc", "free", "__rust_alloc", "__rust_dealloc")):
            calls.append({"address": address, "return_address": instructions[index + 1][0], "target": target,
                          "instructions": instructions[max(0, index - 9):index + 2]})
    return calls, instructions


def main():
    report = []
    for fp in (False, True):
        binary = ROOT / ("runtime-frame-pointer" if fp else "runtime")
        sym = symbols(binary)
        calls, instructions = disassemble(binary, sym)
        starts = sorted({value["start"] for value in sym.values()})
        for backend in ("native", "generic"):
            for case in ("empty", "duplex1m"):
                name = backend + "-" + case + ("-fp" if fp else "")
                if fp and backend == "generic" and (PROFILES / (name + "64.data")).exists():
                    name += "64"
                    if (PROFILES / (name + "-f499.data")).exists():
                        name += "-f499"
                data = PROFILES / (name + ".data")
                text = PROFILES / (name + ".raw.txt")
                if not text.exists():
                    with text.open("w") as output, (PROFILES / (name + "-script-errors.txt")).open("w") as errors:
                        subprocess.run(["perf", "script", "--no-inline", "--no-demangle", "-i", str(data),
                                        "-F", "comm,pid,tid,time,event,ip,sym,symoff,dso"],
                                       stdout=output, stderr=errors, check=True,
                                       env=dict(os.environ, DEBUGINFOD_URLS=""))
                samples = read_samples(text)
                leaves = collections.Counter()
                buckets = collections.Counter()
                hottest = collections.defaultdict(collections.Counter)
                frame_hits = collections.defaultdict(set)
                self_hits = collections.defaultdict(set)
                function_hits = collections.defaultdict(set)
                bucket_stacks = {}
                full_stacks = collections.defaultdict(collections.Counter)
                areas = collections.Counter()
                stack_areas = collections.Counter()
                for index, sample in enumerate(samples):
                    endpoint = set()
                    human = []
                    for frame in sample:
                        value = sym.get(frame["symbol"])
                        symbol = value["name"] if value else frame["symbol"]
                        human.append(symbol)
                        if value:
                            file_address = value["start"] + frame["offset"]
                            frame_hits[file_address].add(index)
                            function_hits[value["start"]].add(index)
                            if frame is sample[0]:
                                self_hits[file_address].add(index)
                            role = roles(symbol)
                            # A join type names both children; it is not an endpoint.
                            if len(role) == 1 and not value["ambiguous_endpoint_alias"]:
                                endpoint.update(role)
                    bucket = next(iter(endpoint)) if len(endpoint) == 1 else "shared_or_unattributed"
                    buckets[bucket] += 1
                    leaf = human[0] + (" [" + sample[0]["dso"] + "]" if human[0] == "[unknown]" else "")
                    leaves[leaf] += 1
                    hottest[bucket][leaf] += 1
                    bucket_stacks.setdefault((bucket, leaf), human)
                    full_stacks[bucket][tuple(human)] += 1
                    predicates = {
                        "wait_pointer_hash": lambda n: "hash_one::<&*const kimojio::async_event::WaitData>" in n,
                        "register_wait": lambda n: "IoScopeRegistry>::register_wait" in n,
                        "retire_wait": lambda n: "WaitData>::retire_from_scopes" in n,
                        "wait_future_poll": lambda n: "WaitAsyncEventFuture as" in n and "::poll" in n,
                        "assertion_compare": lambda n: "support::compare" in n,
                    }
                    for area, predicate in predicates.items():
                        if predicate(human[0]):
                            areas[area] += 1
                        if any(predicate(n) for n in human):
                            stack_areas[area] += 1
                    if (any("RawVecInner>::finish_grow" in n for n in human)
                            and any("IoScopeRegistry>::register_wait" in n for n in human)):
                        stack_areas["reverse_membership_vector_growth"] += 1
                inventory = []
                import bisect
                for call in calls:
                    position = bisect.bisect_right(starts, call["address"]) - 1
                    if position < 0:
                        continue
                    start = starts[position]
                    candidates = [v for v in sym.values() if v["start"] == start and call["address"] < start + v["size"]]
                    if not candidates:
                        continue
                    hit = set()
                    own = set()
                    for address in range(call["address"] - 24, call["address"] + 25):
                        hit.update(frame_hits.get(address, ()))
                        own.update(self_hits.get(address, ()))
                    exact = (frame_hits.get(call["return_address"], set())
                             | frame_hits.get(call["return_address"] - 1, set()))
                    if exact or own:
                        inventory.append({**call, "symbol": candidates[0]["name"],
                                          "symbol_start": start, "offset": call["address"] - start,
                                          "near_stack_samples": len(hit), "near_self_samples": len(own),
                                          "exact_return_samples": len(exact),
                                          "function_stack_samples": len(function_hits[start])})
                inventory.sort(key=lambda item: item["exact_return_samples"], reverse=True)
                inline_sites = []
                for position, (address, instruction) in enumerate(instructions):
                    if (re.search(r"\b(?:v?mov(?:aps|ups|dqa|dqu)\w*|rep\s+movs\w*)\b", instruction)
                            or re.search(r"\b(?:pxor|xorps)\b|PTR \[rsp\],0x0", instruction)):
                        hits = len(self_hits.get(address, ()))
                        if hits:
                            inline_sites.append({
                                "address": address, "instruction": instruction, "self_samples": hits,
                                "context": instructions[max(0, position - 2):position + 3],
                            })
                inline_sites.sort(key=lambda item: item["self_samples"], reverse=True)
                row = {
                    "profile": name, "binary": str(binary), "samples": len(samples),
                    "mean_physical_frames": sum(map(len, samples)) / len(samples),
                    "terminal_unknown_samples": sum(s[-1]["symbol"] == "[unknown]" for s in samples),
                    "endpoint_buckets": dict(buckets),
                    "named_area_self_samples": dict(areas),
                    "named_area_inclusive_samples": dict(stack_areas),
                    "hottest_resolved_stacks_by_bucket": {
                        bucket: [{"stack": list(stack), "samples": count}
                                 for stack, count in counts.most_common(3)]
                        for bucket, counts in full_stacks.items()
                    },
                    "top_leaves": leaves.most_common(25),
                    "hottest_by_bucket": {
                        bucket: [{"leaf": leaf, "samples": count, "example_stack": bucket_stacks[bucket, leaf]}
                                 for leaf, count in counts.most_common(3)]
                        for bucket, counts in hottest.items()
                    },
                    "sampled_call_sites": inventory,
                    "sampled_inline_moves_or_zero_instructions": inline_sites[:40],
                }
                report.append(row)
                print(name, len(samples), dict(buckets), flush=True)
    (HERE / "profile-analysis.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
