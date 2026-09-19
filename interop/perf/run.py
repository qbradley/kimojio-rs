#!/usr/bin/env python3
"""Build, smoke, and (only on explicit request) measure command-supplied peers."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import shutil
import subprocess
import sys
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[2]
ARTIFACTS = ROOT / "target" / "perf"
sys.path.insert(0, str(ROOT / "interop" / "http1"))
from harness import PeerProcess, run_command


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def command_output(command):
    return subprocess.check_output(command, cwd=ROOT, text=True, timeout=15).strip()


def go_environment():
    folders = {name: ARTIFACTS / folder for name, folder in
               (("GOCACHE", "go-cache"), ("GOTMPDIR", "go-tmp"), ("TMPDIR", "go-tmp"), ("GOMODCACHE", "go-mod"))}
    for path in folders.values():
        path.mkdir(parents=True, exist_ok=True)
    return {**{name: str(path) for name, path in folders.items()}, "GOTOOLCHAIN": "local"}


def build():
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    output = ARTIFACTS / "perf-tool"
    environment = {**os.environ, **go_environment()}
    for command in (
        ["go", "mod", "download"], ["go", "test", "./..."], ["go", "vet", "./..."],
        ["go", "build", "-trimpath", "-buildvcs=false", "-pgo=off", "-o", str(output), "."],
    ):
        subprocess.run(command, cwd=ROOT / "interop" / "perf", env=environment, check=True, timeout=120)
    sha = digest(output)
    frozen = ARTIFACTS / f"perf-tool-{sha}"
    if not frozen.exists():
        shutil.copyfile(output, frozen)
        frozen.chmod(0o555)
    assert digest(frozen) == sha
    identity = {
        "binary": str(frozen), "sha256": sha, "checkout_commit": command_output(["git", "rev-parse", "HEAD"]),
        "source_status_at_build": command_output(["git", "status", "--short", "--", "interop/perf"]),
        "source_sha256": {str(p.relative_to(ROOT)): digest(p) for p in sorted((ROOT / "interop" / "perf").glob("*.go"))},
        "go_mod_sha256": digest(ROOT / "interop" / "perf" / "go.mod"),
        "go_sum_sha256": digest(ROOT / "interop" / "perf" / "go.sum"),
        "compiler": command_output(["go", "version"]), "build_flags": ["-trimpath", "-buildvcs=false", "-pgo=off"],
        "dependency": "github.com/gorilla/websocket v1.5.3",
    }
    (ARTIFACTS / "tool-publication.json").write_text(json.dumps(identity, indent=2) + "\n")
    (ARTIFACTS / "reference-manifest.json").write_text(json.dumps(reference_manifest(identity), indent=2) + "\n")
    return identity


def topology():
    allowed = sorted(os.sched_getaffinity(0))
    return {cpu: {
        "socket": int(Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/physical_package_id").read_text()),
        "core": int(Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/core_id").read_text()),
        "siblings": Path(f"/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list").read_text().strip(),
    } for cpu in allowed}


def validate_cpus(server_cpu, client_cpus):
    cpus = topology()
    if server_cpu not in cpus or not client_cpus or any(cpu not in cpus for cpu in client_cpus):
        raise ValueError("requested CPUs are outside current affinity")
    keys = [(cpus[cpu]["socket"], cpus[cpu]["core"]) for cpu in [server_cpu, *client_cpus]]
    if len(set(keys)) != len(keys):
        raise ValueError("server and each client CPU must use distinct physical cores, not SMT siblings")
    return cpus


def snapshot(pid):
    base = Path(f"/proc/{pid}")
    raw = (base / "stat").read_text()
    fields = raw[raw.rindex(")") + 2:].split()
    status = {}
    for line in (base / "status").read_text().splitlines():
        if ":" in line:
            name, value = line.split(":", 1)
            status[name] = value.strip()
    switches = {}
    for task in (base / "task").iterdir():
        try:
            values = {}
            for line in (task / "status").read_text().splitlines():
                if line.startswith(("voluntary_ctxt_switches:", "nonvoluntary_ctxt_switches:")):
                    name, value = line.split(":")
                    values[name] = int(value)
            switches[task.name] = values
        except FileNotFoundError:
            continue
    return {
        "monotonic": time.monotonic(),
        "cpu_user_seconds": int(fields[11]) / os.sysconf("SC_CLK_TCK"),
        "cpu_system_seconds": int(fields[12]) / os.sysconf("SC_CLK_TCK"),
        "rss_kib": int(status.get("VmRSS", "0 kB").split()[0]),
        "rss_high_water_kib": int(status.get("VmHWM", "0 kB").split()[0]),
        "fds": len(list((base / "fd").iterdir())), "threads": int(status["Threads"]),
        "affinity": status["Cpus_allowed_list"], "sampled_thread_context_switches": switches,
    }


class Samples:
    def __init__(self, pid):
        self.pid = pid
        self.rows = []
        self.errors = []
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self.collect, daemon=True)

    def collect(self):
        while not self.stop.is_set():
            try:
                self.rows.append(snapshot(self.pid))
            except OSError as error:
                self.errors.append(repr(error))
                return
            self.stop.wait(.05)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.stop.set()
        self.thread.join(timeout=2)
        if self.thread.is_alive():
            raise RuntimeError("process sampler survived cleanup")


def cases(smoke=False):
    result = []
    for kind, sizes in (("static", (128, 65536)), ("fixture", (128, 65536))):
        for size in sizes:
            for concurrency in (1, 16):
                result.append({"kind": kind, "size": size, "concurrency": concurrency,
                               "path": f"/{size}.bin" if kind == "static" else f"/bytes/{size}",
                               "framing": "fixed", "fresh": False})
        if kind == "static":
            result.append({"kind": kind, "size": 128, "concurrency": 16,
                           "path": "/128.bin", "framing": "fixed", "fresh": True})
    result.append({"kind": "fixture", "size": 13, "concurrency": 1,
                   "path": "/trailers", "framing": "trailers", "fresh": False})
    for size in (128, 4096, 65536):
        for fanout in (1, 4):
            result.append({"kind": "ws", "size": size, "fanout": fanout, "path": "/chat"})
    return result


def client_cases():
    result = [{"kind": "fixture", "size": size, "concurrency": concurrency,
               "path": f"/bytes/{size}", "framing": "fixed", "fresh": False}
              for size in (128, 65536) for concurrency in (1, 16)]
    result.append({"kind": "fixture", "size": 128, "concurrency": 16,
                   "path": "/bytes/128", "framing": "fixed", "fresh": True})
    return result


def chunked_cases():
    return [{"kind": "fixture", "size": size, "concurrency": concurrency,
             "path": "/echo", "framing": "chunked-echo", "fresh": True}
            for size in (65536, 1048576) for concurrency in (1, 16)]


def reference_manifest(identity):
    return {"schema": 1, "targets": [
        {"name": f"go-{kind}", "kind": kind, "binary": identity["binary"], "sha256": identity["sha256"],
         "source": identity, "profile": "Go default optimization, -trimpath",
         "socket_options": {"server_tcp_nodelay": kind == "fixture", "requested_send_buffer_bytes": 65536 if kind == "ws" else 0,
                            "zero_send_buffer_meaning": "retain OS default",
                            "keepalive": "30s idle/1s interval/30 probes" if kind == "fixture" else False},
         "limits": {"max_clients": 32 if kind == "fixture" else 64, "max_message_bytes": 1048576, "queued_messages": 32,
                    "per_client_payload_bytes": 4194304, "aggregate_unique_payload_bytes": 33554432,
                    "max_requests_per_connection": 0 if kind == "ws" else 1000},
         "comparability": "Go WS byte caps exclude metadata/reservations; Go static can use sendfile.",
         "challenge_command": [identity["binary"], "ws-serve", "--bind", "{bind}",
                               "--max-message-bytes", "65536", "--max-queued-messages", "2",
                               "--max-client-bytes", "262144", "--send-buffer-bytes", "4096",
                               "--close-timeout", "5s"] if kind == "ws" else None,
         "command": [identity["binary"], "ws-serve" if kind == "ws" else "http-serve", "--bind", "{bind}",
                     "--max-clients", "32" if kind == "fixture" else "64",
                     "--max-requests-per-connection", "0" if kind == "ws" else "1000",
                     f"--nodelay={str(kind == 'fixture').lower()}", f"--keepalive={str(kind == 'fixture').lower()}",
                     *(["--send-buffer-bytes", "65536"] if kind == "ws" else ["--mode", kind, "--root", "{root}"])]}
        for kind in ("static", "fixture", "ws")]}


def load_manifest(path, identity):
    manifest = json.loads(path.read_text()) if path else reference_manifest(identity)
    if manifest.get("schema") != 1 or not manifest.get("targets"):
        raise ValueError("manifest schema/targets missing")
    for target in manifest["targets"]:
        for required in ("name", "kind", "command", "source", "profile", "socket_options", "limits", "binary", "sha256"):
            if required not in target:
                raise ValueError(f"target missing {required}")
        if target["kind"] not in ("static", "fixture", "ws"):
            raise ValueError("unknown target kind")
        if not isinstance(target["command"], list) or not target["command"] or not all(isinstance(v, str) for v in target["command"]):
            raise ValueError("command must be a nonempty string array")
        if Path(target["command"][0]).resolve() != Path(target["binary"]).resolve():
            raise ValueError("command executable must match the hash-identified binary")
        if digest(target["binary"]) != target["sha256"]:
            raise ValueError(f"target binary hash mismatch: {target['name']}")
    return manifest


def run_one(target, case, directory, tool, args, row, client_kind="go"):
    actual = [item.replace("{bind}", "127.0.0.1:0").replace("{root}", str(directory / "root"))
              for item in target["command"]]
    row["server_command"] = ["taskset", "-c", str(args.server_cpu), "env", "GOMAXPROCS=1", *actual]
    with PeerProcess(row["server_command"], cwd=ROOT) as server:
        host, port = server.address
        mode = "ws-load" if case["kind"] == "ws" else "http-load"
        uri = f"{'ws' if mode == 'ws-load' else 'http'}://{host}:{port}{case['path']}"
        base = ["taskset", "-c", ",".join(map(str, args.client_cpus)), "env",
                f"GOMAXPROCS={len(args.client_cpus)}", tool, mode, "--url", uri, "--size", str(case["size"]),
                "--nodelay=true"]
        if mode == "ws-load":
            base += ["--fanout", str(case["fanout"])]
        else:
            base += ["--concurrency", str(case["concurrency"]), "--framing", case["framing"],
                     f"--fresh={str(case['fresh']).lower()}"]
        def client_command(duration):
            if client_kind == "go":
                return [*base, "--duration", f"{duration}s"]
            return [
                sys.executable, "-B", str(ROOT / "interop" / "perf" / "native_client.py"),
                "--publication", str(args.native_client_publication), "--url", uri,
                "--connections", str(case["concurrency"]), "--size", str(case["size"]),
                "--mode", "fresh" if case["fresh"] else "keepalive",
                "--client-cpus", ",".join(map(str, args.client_cpus)),
                "--duration-ms", str(round(duration * 1000)),
            ]
        if args.warmup:
            warm_command = client_command(args.warmup)
            warm = run_command(warm_command, cwd=ROOT, timeout=args.warmup + 20)
            row["warmup"] = {"command": warm_command, "exit": warm.returncode,
                             "stderr": warm.stderr.decode(errors="replace"), "result": json.loads(warm.stdout)}
            if warm.returncode or not row["warmup"]["result"]["valid"]:
                raise RuntimeError(f"warmup failed: {row['warmup']}")
        row["server_before"] = snapshot(server.process.pid)
        row["client_command"] = client_command(args.duration)
        with Samples(server.process.pid) as sampled:
            client = run_command(row["client_command"], cwd=ROOT, timeout=args.duration + 30)
        row["server_samples"] = sampled.rows
        row["errors"] += sampled.errors
        row["client_exit"] = client.returncode
        row["client_stderr"] = client.stderr.decode(errors="replace")
        row["client_stdout"] = client.stdout.decode(errors="replace")
        row["client"] = json.loads(client.stdout)
        elapsed = row["client"]["elapsed_seconds"]
        measured_cpu = row["client"].get("measurement_cpu_total_seconds")
        if measured_cpu is None:
            measured_cpu = row["client"]["cpu_user_seconds"] + row["client"]["cpu_system_seconds"]
        fraction = measured_cpu / elapsed / len(args.client_cpus) if elapsed > 0 else None
        row["client_cpu_fraction_of_allocated_cores"] = fraction
        row["client_saturation_warning"] = fraction is not None and fraction >= .85
        row["server_log"] = server.diagnostics()
        row["server_exit_after_load"] = server.process.poll()
        row["server_after"] = snapshot(server.process.pid)
        row["retained_rss_samples"] = []
        for delay in (.2, .3):
            time.sleep(delay)
            row["retained_rss_samples"].append(snapshot(server.process.pid))
        row["server_log"] = server.diagnostics()
        row["valid"] = client.returncode == 0 and row["client"]["valid"] and not row["errors"]
        row["server_cpu"] = {
            name: row["server_after"][name] - row["server_before"][name]
            for name in ("cpu_user_seconds", "cpu_system_seconds")
        }
        row["server_cpu_window_seconds"] = row["server_after"]["monotonic"] - row["server_before"]["monotonic"]
        row["server_max_sampled_rss_kib"] = max(r["rss_kib"] for r in [row["server_before"], row["server_after"], *sampled.rows])
        row["server_context_switches_scope"] = "Per-TID sampled counters; threads that exit between samples can be missed. Client rusage contains whole-process totals."
        if digest(target["binary"]) != target["sha256"]:
            raise RuntimeError("server executable changed during run")
    return row


def run(args):
    identity = json.loads((ARTIFACTS / "tool-publication.json").read_text())
    if digest(identity["binary"]) != identity["sha256"]:
        raise ValueError("load generator changed after publication")
    topo = validate_cpus(args.server_cpu, [*args.client_cpus, args.monitor_cpu])
    os.sched_setaffinity(0, {args.monitor_cpu})
    manifest = load_manifest(args.manifest, identity)
    directory = ARTIFACTS / f"{args.action}-{uuid.uuid4().hex}"
    (directory / "root").mkdir(parents=True)
    for size in (128, 65536, 1048576):
        (directory / "root" / f"{size}.bin").write_bytes(b"x" * size)
    report = {"schema": 1, "purpose": "correctness_smoke_not_performance" if args.action == "smoke" else "measurement",
              "directory": str(directory), "tool": identity, "manifest": manifest,
              "platform": {"uname": list(platform.uname()), "python": sys.version,
                           "rustc": command_output(["rustc", "--version"]), "go": command_output(["go", "version"]),
                           "cpu_model": next(line for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")),
                           "topology": topo, "initial_loadavg": os.getloadavg()},
              "configuration": {key: str(value) if isinstance(value, Path) else value for key, value in vars(args).items()},
              "schedule": [], "results": []}
    if args.suite in ("clients", "all"):
        if args.native_client_publication is None:
            raise ValueError("client comparisons require --native-client-publication")
        native_identity = json.loads(args.native_client_publication.read_text())
        if digest(native_identity["binary"]["path"]) != native_identity["binary"]["sha256"]:
            raise ValueError("native client hash mismatch")
        report["native_client_publication"] = native_identity
        common = copy.deepcopy(next(target for target in manifest["targets"] if target["name"] == args.client_server_name))
        common["name"] += "-for-clients"
        common["command"] += ["--max-requests-per-connection", "0"]
        common["limits"]["max_requests_per_connection"] = 0
        report["client_comparison_server"] = common
    rng = random.Random(args.seed)
    for repetition in range(args.repetitions):
        selected = []
        if args.suite in ("servers", "all"):
            selected += [("servers", case) for case in cases(args.action == "smoke")]
        if args.suite == "chunked":
            selected += [("servers", case) for case in chunked_cases()]
        if args.suite in ("clients", "all"):
            selected += [("clients", case) for case in client_cases()]
        rng.shuffle(selected)
        for comparison, case in selected:
            candidates = (
                [(target, "go") for target in manifest["targets"] if target["kind"] == case["kind"]]
                if comparison == "servers" else [(common, "go"), (common, "native")]
            )
            rng.shuffle(candidates)
            for target, client_kind in candidates:
                report["schedule"].append({"repetition": repetition, "comparison": comparison, "case": case,
                                           "target": target["name"], "client_kind": client_kind})
                row = {"target": target, "case": case, "errors": [], "comparison": comparison, "client_kind": client_kind}
                try:
                    row = run_one(target, case, directory, identity["binary"], args, row, client_kind)
                except (OSError, RuntimeError, ValueError, AssertionError) as error:
                    row["valid"] = False
                    row["errors"].append(repr(error))
                    row["error_notes"] = getattr(error, "__notes__", [])
                row["repetition"] = repetition
                report["results"].append(row)
                (directory / "results.json").write_text(json.dumps(report, indent=2) + "\n")
                print(f"{comparison}/{target['name']}/{client_kind} {case} valid={row['valid']}", flush=True)
    report["valid"] = bool(report["results"]) and all(row["valid"] for row in report["results"])
    (directory / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    if args.archive:
        archive = ROOT / "docs" / "http1-fsm-evidence" / "performance"
        archive.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(directory / "results.json", archive / f"{directory.name}.json")
        print(f"ARCHIVE {archive / (directory.name + '.json')}")
    print(f"REPORT {directory / 'results.json'}")
    return 0 if report["valid"] else 1


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("action", choices=("build", "smoke", "measure"))
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--suite", choices=("servers", "clients", "all", "chunked"), default="servers")
    parser.add_argument("--native-client-publication", type=Path)
    parser.add_argument("--client-server-name", default="go-fixture")
    parser.add_argument("--server-cpu", type=int, default=0)
    parser.add_argument("--client-cpus", type=lambda s: [int(v) for v in s.split(",")], default=[2, 4])
    parser.add_argument("--monitor-cpu", type=int, default=6)
    parser.add_argument("--duration", type=float, default=None)
    parser.add_argument("--warmup", type=float, default=None)
    parser.add_argument("--repetitions", type=int, default=None)
    parser.add_argument("--seed", type=int, default=953)
    parser.add_argument("--archive", action="store_true", help="persist selected full reports under docs/http1-fsm-evidence/performance")
    args = parser.parse_args()
    if args.action == "build":
        print(json.dumps(build(), indent=2))
        return 0
    args.duration = args.duration if args.duration is not None else (.05 if args.action == "smoke" else 2)
    args.warmup = args.warmup if args.warmup is not None else (0 if args.action == "smoke" else .5)
    args.repetitions = args.repetitions if args.repetitions is not None else (1 if args.action == "smoke" else 3)
    if not 0 < args.duration <= 30 or not 0 <= args.warmup <= 5 or not 1 <= args.repetitions <= 5:
        parser.error("duration/warmup/repetition limits exceeded")
    if args.action == "smoke" and (args.duration > .2 or args.warmup > .1 or args.repetitions != 1):
        parser.error("smoke must remain short; use measure only after parent scheduling approval")
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
