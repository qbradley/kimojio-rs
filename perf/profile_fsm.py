"""Record short userspace CPU profiles against immutable release binaries."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "interop" / "http1"))
from harness import PeerProcess


def digest(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def successful(value: dict) -> bool:
    if value.get("valid") is not True or "errors" not in value:
        return False
    errors = value["errors"]
    no_errors = errors is None or errors == [] or (type(errors) is int and errors == 0)
    count = value.get("successes", value.get("count"))
    return no_errors and type(count) is int and count > 0


def result(command: list[str], timeout: float) -> dict:
    completed = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, timeout=timeout)
    if completed.returncode != 0:
        raise RuntimeError(f"load failed: {command}\n{completed.stdout}\n{completed.stderr}")
    value = json.loads(completed.stdout)
    if not successful(value):
        raise RuntimeError(f"load did not complete correctly: {value.get('errors')}")
    return value


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--go-tool", type=Path, required=True)
    parser.add_argument("--native-dir", type=Path, required=True)
    parser.add_argument("--native-client", type=Path, required=True)
    parser.add_argument("--server-revision", required=True)
    parser.add_argument("--client-revision", required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--case", choices=["static", "wrapper", "websocket", "client"], action="append")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    go = str(args.go_tool.resolve(strict=True))
    native_client = args.native_client.resolve(strict=True)
    native_dir = args.native_dir.resolve(strict=True)
    fixture = args.output / "fixture"
    fixture.mkdir()
    (fixture / "body.bin").write_bytes(b"x" * 65536)
    profiles = []
    for case in args.case or ["static", "wrapper", "websocket", "client"]:
        directory = args.output / case
        directory.mkdir()
        if case == "client":
            subject = native_client
            server = [go, "http-serve", "--bind", "127.0.0.1:0", "--mode", "fixture",
                      "--max-clients", "32", "--nodelay=true", "--keepalive=true"]
            server_command = ["taskset", "-c", "2", "env", "GOMAXPROCS=1", *server]
            revision = args.client_revision
        else:
            filename = {"static": "http1-static", "wrapper": "http1-wrapper-server",
                        "websocket": "websocket-chat"}[case]
            subject = native_dir / filename
            server = [str(subject), "--bind", "127.0.0.1:0"]
            if case == "static":
                server += ["--root", str(fixture.resolve())]
            if case == "websocket":
                server += ["--run-for-ms", "20000"]
            server_command = ["taskset", "-c", "0", *server]
            revision = args.server_revision
        before = digest(subject)
        with PeerProcess(server_command, cwd=ROOT) as peer:
            host, port = peer.address
            if case == "client":
                load_command = ["taskset", "-c", "0", str(subject), "--url",
                                f"http://{host}:{port}/bytes/65536", "--size", "65536",
                                "--connections", "16", "--warmup-ms", "500", "--duration-ms", "8000"]
                warmup = None
            else:
                prefix = ["taskset", "-c", "2,4,6", "env", "GOMAXPROCS=3", go]
                if case == "websocket":
                    workload = ["ws-load", "--url", f"ws://{host}:{port}/chat",
                                "--size", "4096", "--fanout", "4"]
                else:
                    path = "/body.bin" if case == "static" else "/bytes/65536"
                    workload = ["http-load", "--url", f"http://{host}:{port}{path}",
                                "--size", "65536", "--concurrency", "16"]
                warmup = result([*prefix, *workload, "--duration", "500ms"], 10)
                load_command = [*prefix, *workload, "--duration", "8s"]
            load = subprocess.Popen(load_command, cwd=ROOT, stdout=subprocess.PIPE,
                                    stderr=subprocess.PIPE, text=True)
            try:
                time.sleep(1.25)
                if load.poll() is not None:
                    out, err = load.communicate()
                    raise RuntimeError(f"load exited before sampling: {out}\n{err}")
                pid = load.pid if case == "client" else peer.process.pid
                command = ["perf", "record", "-q", "-e", "cpu-clock:u", "-F", "499",
                           "--call-graph", "dwarf,16384", "-p", str(pid),
                           "-o", str(directory / "perf.data"), "--", "sleep", "3"]
                recorded = subprocess.run(command, capture_output=True, text=True, timeout=15)
                (directory / "perf.stderr").write_text(recorded.stderr)
                if recorded.returncode != 0:
                    raise RuntimeError(f"perf failed: {recorded.stderr}")
                out, err = load.communicate(timeout=15)
                (directory / "load.stdout").write_text(out)
                (directory / "load.stderr").write_text(err)
                workload_result = json.loads(out)
                if load.returncode != 0 or not successful(workload_result):
                    raise RuntimeError(f"profile workload failed: {out}\n{err}")
                if digest(subject) != before:
                    raise RuntimeError("profile binary changed during sampling")
                metadata = {
                    "case": case,
                    "binary": str(subject),
                    "sha256": before,
                    "revision": revision,
                    "build": (
                        "CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_TARGET_DIR=target/profile "
                        "cargo build --release -p kimojio-http1 --example bench_client --offline"
                        if case == "client" else
                        "CARGO_PROFILE_RELEASE_DEBUG=2 CARGO_TARGET_DIR=target/profile "
                        "cargo build --release -p http1-static -p kimojio-http1 "
                        "-p websocket-chat --bins --examples --offline"
                    ),
                    "event": "cpu-clock:u",
                    "scope": "Userspace CPU samples after warmup; load statistics are instrumented, not comparative results",
                    "server_command": server_command,
                    "load_command": load_command,
                    "load_tool_sha256": digest(args.go_tool),
                    "perf_command": command,
                    "warmup": warmup,
                    "workload_result": workload_result,
                }
                (directory / "profile.json").write_text(json.dumps(metadata, indent=2) + "\n")
                profiles.append(metadata)
                print(f"{case}: recorded {directory / 'perf.data'}", flush=True)
            finally:
                if load.poll() is None:
                    load.terminate()
                try:
                    load.communicate(timeout=3)
                except subprocess.TimeoutExpired:
                    load.kill()
                    load.communicate()
    (args.output / "profiles.json").write_text(json.dumps(profiles, indent=2) + "\n")


if __name__ == "__main__":
    main()
