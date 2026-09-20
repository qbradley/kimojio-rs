"""Record both unchanged normal-allocator binaries, then resolve actual call sites."""
import collections
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess

HERE = Path(__file__).resolve().parent
freeze = json.loads((HERE / "freeze.json").read_text())
spec = importlib.util.spec_from_file_location("previous_profile", HERE.parent / "measured/profile_analysis.py")
analysis = importlib.util.module_from_spec(spec)
spec.loader.exec_module(analysis)
private = Path(freeze["binaries"]["runtime"]["path"]).parent / "profiles"
metadata = []
reports = []
for version, entry in (("baseline", freeze["baseline_binaries"]["runtime"]),
                       ("candidate", freeze["binaries"]["runtime"])):
    binary = Path(entry["path"])
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == entry["sha256"]
    folder = private / version
    folder.mkdir(parents=True, exist_ok=False)
    data = folder / "generic-duplex.data"
    command = [
        "perf", "record", "-N", "-e", "cpu-clock:u", "-F", "999",
        "--call-graph", "dwarf,32768", "-o", str(data), "--",
        "taskset", "-c", "2", str(binary), "--backend", "generic", "--case", "duplex",
        "--bytes", "1048576", "--concurrency", "8", "--cohorts", "1000",
        "--warmup", "8", "--timeout-seconds", "90",
    ]
    record = {"version": version, "command": command, "binary": entry}
    result = subprocess.run(command, text=True, capture_output=True, timeout=110)
    record.update(returncode=result.returncode, stdout=result.stdout, stderr=result.stderr)
    record["data_sha256"] = hashlib.sha256(data.read_bytes()).hexdigest()
    metadata.append(record)
    (HERE / "profiles.json").write_text(json.dumps(metadata, indent=2) + "\n")
    assert result.returncode == 0
    workload = json.loads(result.stdout)
    assert workload["valid"] and workload["drivers_closed_successfully"] == 2
    assert workload["client_retirements"] == workload["server_retirements"] == 8064
    assert workload["early_response_witnesses_including_warmup"] == 8064
    assert workload["checked_payload_bytes_including_warmup"] == 8064 * 2 * 1048576
    analysis.PROFILES = folder
    symbols = analysis.symbols(binary)
    calls, instructions = analysis.disassemble(binary, symbols)
    raw = folder / "samples.txt"
    with raw.open("w") as output, (folder / "script-errors.txt").open("w") as errors:
        subprocess.run(["perf", "script", "--no-inline", "--no-demangle", "-i", str(data),
                        "-F", "comm,pid,tid,time,event,ip,sym,symoff,dso"],
                       stdout=output, stderr=errors, check=True,
                       env=dict(os.environ, DEBUGINFOD_URLS=""))
    samples = analysis.read_samples(raw)
    frame_hits = collections.defaultdict(set)
    self_hits = collections.defaultdict(set)
    parent_hits = collections.defaultdict(set)
    leaves = collections.Counter()
    buckets = collections.Counter()
    stacks = collections.defaultdict(collections.Counter)
    for index, sample in enumerate(samples):
        endpoint = set()
        human = []
        for position, frame in enumerate(sample):
            symbol = symbols.get(frame["symbol"])
            human.append(symbol["name"] if symbol else frame["symbol"])
            if symbol:
                address = symbol["start"] + frame["offset"]
                frame_hits[address].add(index)
                parent_hits[symbol["start"]].add(index)
                if position == 0:
                    self_hits[address].add(index)
                roles = analysis.roles(symbol["name"])
                if len(roles) == 1 and not symbol["ambiguous_endpoint_alias"]:
                    endpoint.update(roles)
        bucket = next(iter(endpoint)) if len(endpoint) == 1 else "shared_or_unattributed"
        buckets[bucket] += 1
        leaves[human[0]] += 1
        stacks[bucket][tuple(human)] += 1
    inventory = []
    for call in calls:
        owners = [s for s in symbols.values() if s["start"] <= call["address"] < s["start"] + s["size"]]
        if not owners:
            continue
        owner = owners[0]
        exact = frame_hits.get(call["return_address"], set()) | frame_hits.get(call["return_address"] - 1, set())
        nearby = set().union(*(self_hits.get(a, set()) for a in range(call["address"] - 24, call["address"] + 25)))
        interesting = any(word in owner["name"] for word in ("try_read_impl", "Connection<kimojio_http2::body::Data>"))
        if exact or nearby or (interesting and "memcpy" in call["target"]):
            source = subprocess.check_output(
                ["addr2line", "-f", "-C", "-i", "-e", str(binary), hex(call["address"])], text=True)
            inventory.append({**call, "symbol": owner["name"], "offset": call["address"] - owner["start"],
                              "exact_return_samples": len(exact), "near_self_samples": len(nearby),
                              "parent_samples": len(parent_hits[owner["start"]]), "source": source})
    inventory.sort(key=lambda row: row["exact_return_samples"], reverse=True)
    reports.append({"version": version, "samples": len(samples),
                    "mean_frames": sum(map(len, samples)) / len(samples),
                    "endpoint_buckets": dict(buckets), "top_leaves": leaves.most_common(20),
                    "top_stacks": {b: [{"samples": count, "stack": list(stack)}
                                      for stack, count in c.most_common(3)] for b, c in stacks.items()},
                    "copy_and_allocation_sites": inventory})
    print(version, len(samples), dict(buckets), flush=True)
(HERE / "profile-analysis.json").write_text(json.dumps(reports, indent=2) + "\n")
