"""Command-adapter suites in both HTTP/2 roles, with bounded JSON evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import uuid

from cases import flow_cases, qualify, semantic_cases, validate_results
from reference import client, serve
from socket_peer import PeerProcess, ScriptedPeer, prerequisites, require, run_command

ROOT = Path(__file__).resolve().parents[2]
MAX_JSON = 2 * 1024 * 1024


def read_json(path):
    require(path.is_file(), f"missing JSON file: {path}")
    require(path.stat().st_size <= MAX_JSON, "JSON file exceeds size limit")
    return json.loads(path.read_text())


def write_json(path, value):
    text = json.dumps(value, separators=(",", ":")) + "\n"
    require(len(text.encode()) <= MAX_JSON, "JSON report exceeds size limit")
    path.write_text(text)


def adapter(path, *, client_role):
    spec = read_json(path)
    require(spec.get("schema") == 1, "adapter schema must be 1")
    command = spec.get("command")
    require(
        isinstance(command, list) and 0 < len(command) <= 64
        and all(isinstance(arg, str) and len(arg) <= 8192 for arg in command),
        "adapter command must be a bounded nonempty string array",
    )
    for placeholder in (["{request_file}", "{result_file}"] if client_role else ["{request_file}"]):
        require(any(placeholder in arg for arg in command), f"missing {placeholder}")
    return spec


def command(spec, request_file, result_file=None):
    return [
        arg.replace("{request_file}", str(request_file)).replace("{result_file}", str(result_file))
        for arg in spec["command"]
    ]


def client_case(spec, case, directory, *, upload=False):
    """Independent server against a command-driven client."""
    directory.mkdir(parents=True)
    observations = []

    def handler(wire, _index):
        observations.append(serve(wire.sock, config=case.config if upload else {}, case=case))

    with ScriptedPeer(handler, timeout=65) as peer:
        request_file, result_file = directory / "request.json", directory / "result.json"
        write_json(request_file, case.spec(peer.address, upload=upload))
        completed = run_command(
            command(spec, request_file, result_file), cwd=ROOT, timeout=65,
        )
        require(completed.returncode == 0, f"client command failed: {completed.stderr!r}")
        result = read_json(result_file)
        validate_results(case, result)
    require(len(observations) == 1, "independent server did not finish")
    observation = observations[0]
    require(observation["requests"] == case.count, "wrong request count on wire")
    require(observation["peer_eof"], "client did not close the actual socket")
    qualify(case, observation["credit"])
    if upload:
        qualify(case, observation["receive_credit"])
    if case.padding is not None:
        for stream in observation["credit"]["streams"]:
            require(
                stream["flow"] == case.length + stream["frames"] * (case.padding + 1),
                "padding refund accounting mismatch",
            )
    if case.name == "empty-data-sibling":
        require(observation["empty_data_frames"] == 4096, "wrong empty DATA frame count")
    write_json(directory / "peer.json", observation)
    return {
        "name": case.name, "passed": True, "credit": observation["credit"],
        **({"upload_credit": observation["receive_credit"]} if upload else {}),
    }


def server_case(spec, case, directory, *, upload):
    """Independent client against a command-driven server."""
    directory.mkdir(parents=True)
    request_file = directory / "request.json"
    write_json(request_file, {
        "schema": 1, "config": case.config, "timeout_ms": 60000,
    })
    with PeerProcess(command(spec, request_file), cwd=ROOT) as process:
        result = client(case.spec(process.address, upload=upload))
        validate_results(case, result)
        require(process.output_bytes <= 128 * 1024, "server output exceeds limit")
        if upload:
            qualify(case, result["credit"])
        else:
            qualify(case, result["receive_credit"])
        write_json(directory / "result.json", result)
    return {
        "name": f"{'upload' if upload else 'download'}-{case.name}",
        "passed": True,
        "credit": result["credit"] if upload else result["receive_credit"],
    }


def run(*, client_adapter, server_adapter, directory, names=None, evidence="adapter"):
    prerequisites()
    directory.mkdir(parents=True)
    cases = flow_cases() + semantic_cases()
    if names:
        unknown = set(names) - {case.name for case in cases}
        require(not unknown, f"unknown cases: {sorted(unknown)}")
        cases = [case for case in cases if case.name in names]
    require(cases, "no cases selected")
    report = {"schema": 1, "evidence": evidence, "cases": [], "failures": []}
    for case in cases:
        tasks = []
        if client_adapter:
            tasks.append(("client", lambda: client_case(
                client_adapter, case, directory / "client" / case.name,
            )))
            if case.qualification:
                tasks.append(("client-upload", lambda: client_case(
                    client_adapter, case, directory / "client-upload" / case.name, upload=True,
                )))
        if server_adapter and case.padding is None and case.name != "empty-data-sibling":
            tasks.append(("server-download", lambda: server_case(
                server_adapter, case, directory / "server-download" / case.name, upload=False,
            )))
            if case.qualification:
                tasks.append(("server-upload", lambda: server_case(
                    server_adapter, case, directory / "server-upload" / case.name, upload=True,
                )))
        for role, execute in tasks:
            try:
                result = execute()
                result["role"] = role
                # Full per-stream accounting remains in the bounded per-case files.
                for field in ("credit", "upload_credit"):
                    credit = result.get(field)
                    if credit:
                        result[field] = {key: value for key, value in credit.items() if key != "streams"}
                report["cases"].append(result)
                print(f"PASS {role} {case.name}", flush=True)
            except Exception as error:
                report["failures"].append({
                    "role": role, "name": case.name, "error": f"{type(error).__name__}: {error}"[:8192],
                })
                print(f"FAIL {role} {case.name}: {error}", file=sys.stderr, flush=True)
    write_json(directory / "report.json", report)
    require(not report["failures"], f"{len(report['failures'])} cases failed; see {directory / 'report.json'}")
    return report


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--client-adapter", type=Path)
    parser.add_argument("--server-adapter", type=Path)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--case", action="append")
    parser.add_argument("--output", type=Path, default=ROOT / "target" / "http2-interop" / uuid.uuid4().hex)
    args = parser.parse_args()
    directory = args.output.resolve()
    require("target" in directory.parts, "reports must reside under a target directory")
    if args.selftest:
        require(not args.client_adapter and not args.server_adapter, "selftest cannot use native adapters")
        source = str(Path(__file__).with_name("reference.py"))
        client_adapter = {"schema": 1, "command": [sys.executable, source, "client", "{request_file}", "{result_file}"]}
        server_adapter = {"schema": 1, "command": [sys.executable, source, "server", "{request_file}"]}
    else:
        require(args.client_adapter or args.server_adapter, "at least one adapter is required")
        client_adapter = adapter(args.client_adapter, client_role=True) if args.client_adapter else None
        server_adapter = adapter(args.server_adapter, client_role=False) if args.server_adapter else None
    run(
        client_adapter=client_adapter, server_adapter=server_adapter, directory=directory,
        names=args.case, evidence="python-h2-peer-selftest-NOT-core" if args.selftest else "adapter",
    )


if __name__ == "__main__":
    main()
