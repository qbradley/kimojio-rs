"""Command-adapter suites in both HTTP/2 roles, with bounded JSON evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import sys
import uuid

from cases import flow_cases, qualify, semantic_cases, validate_results
from reference import client, serve
from profiles import PROFILES, WRAPPER_TRAILERS, prepend_warmups, without_warmups
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
    if client_role:
        required = ["{request_file}", "{result_file}"]
    elif any("{request_file}" in arg for arg in command):
        required = ["{request_file}"]
    else:
        required = ["{stream_window}", "{connection_window}"]
    for placeholder in required:
        require(any(placeholder in arg for arg in command), f"missing {placeholder}")
    return spec


def command(spec, request_file, result_file=None, *, config=None):
    config = config or {}
    return [
        arg.replace("{request_file}", str(request_file)).replace("{result_file}", str(result_file))
        .replace("{stream_window}", str(config.get("stream_window", "default")))
        .replace("{connection_window}", str(config.get("connection_window", "default")))
        for arg in spec["command"]
    ]


def client_case(spec, case, directory, *, upload=False, profile="canonical"):
    """Independent server against a command-driven client."""
    directory.mkdir(parents=True)
    observations = []
    warmups = case.concurrency if profile == "wrapper" and upload and case.config.get("stream_window", 65535) < 65535 else 0
    trailers = WRAPPER_TRAILERS if profile == "wrapper" and case.route == "trailers" else None

    def handler(wire, _index):
        observations.append(serve(
            wire.sock, config=case.config if upload else {}, case=case, warmup_count=warmups,
            trailer_fields=trailers,
        ))

    with ScriptedPeer(handler, timeout=65) as peer:
        request_file, result_file = directory / "request.json", directory / "result.json"
        request = case.spec(peer.address, upload=upload)
        write_json(request_file, prepend_warmups(request, warmups) if warmups else request)
        completed = run_command(
            command(spec, request_file, result_file), cwd=ROOT, timeout=65,
        )
        require(completed.returncode == 0, f"client command failed: {completed.stderr!r}")
        result = read_json(result_file)
        target = without_warmups(result, warmups, profile) if warmups else result
        validate_results(case, target, echo=upload, profile=profile, stream_offset=warmups, expected_trailers=trailers)
    require(len(observations) == 1, "independent server did not finish")
    observation = observations[0]
    require(observation["requests"] == case.count + warmups, "wrong request count on wire")
    require(observation["peer_eof"], "client did not close the actual socket")
    def target_credit(credit):
        return {**credit, "streams": [s for s in credit["streams"] if s["stream_id"] > 2 * warmups]}

    qualify(case, target_credit(observation["credit"]))
    if upload:
        qualify(case, target_credit(observation["receive_credit"]))
    if case.padding is not None:
        for stream in observation["credit"]["streams"]:
            require(
                stream["flow"] == case.length + stream["frames"] * (case.padding + 1),
                "padding refund accounting mismatch",
            )
    if case.name == "empty-data-sibling":
        require(observation["empty_data_frames"] == 4096, "wrong empty DATA frame count")
        limited = any(stream["error"] == {"scope": "stream", "code": 11} for stream in result["streams"])
        require(
            observation["resets"] == ({1: 11} if limited else {}),
            "reported resource limit does not match wire RST_STREAM",
        )
    else:
        limited = False
    write_json(directory / "peer.json", observation)
    return {
        "name": case.name, "passed": True, "credit": observation["credit"],
        "outcome": "bounded-stream-rejection" if limited else "complete",
        **({"upload_credit": observation["receive_credit"]} if upload else {}),
        "profile": profile,
        "startup": "bodyless-warmup" if warmups else "unchanged",
        "warmup_requests": warmups,
        "canonical_cold_start": not bool(warmups),
        "trailer_comparison": "per-name-occurrences" if profile == "wrapper" else "exact-list",
    }


def server_case(spec, case, directory, *, upload, profile="canonical"):
    """Independent client against a command-driven server."""
    directory.mkdir(parents=True)
    request_file = directory / "request.json"
    write_json(request_file, {
        "schema": 1, "config": case.config, "timeout_ms": 60000,
    })
    with PeerProcess(command(spec, request_file, config=case.config), cwd=ROOT) as process:
        result = client(case.spec(process.address, upload=upload))
        validate_results(case, result, echo=upload, profile=profile)
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
        "profile": profile, "startup": "independent-client-handshake",
        "trailer_comparison": "per-name-occurrences" if profile == "wrapper" else "exact-list",
    }


def run(*, client_adapter, server_adapter, directory, names=None, evidence="adapter", profile="canonical"):
    prerequisites()
    require(profile in PROFILES, "unknown qualification profile")
    directory.mkdir(parents=True)
    cases = flow_cases() + semantic_cases()
    if names:
        unknown = set(names) - {case.name for case in cases}
        require(not unknown, f"unknown cases: {sorted(unknown)}")
        cases = [case for case in cases if case.name in names]
    require(cases, "no cases selected")
    report = {"schema": 1, "evidence": evidence, "profile": profile, "cases": [], "failures": []}
    for case in cases:
        tasks = []
        if client_adapter:
            tasks.append(("client", lambda: client_case(
                client_adapter, case, directory / "client" / case.name, profile=profile,
            )))
            if case.qualification:
                tasks.append(("client-upload", lambda: client_case(
                    client_adapter, case, directory / "client-upload" / case.name, upload=True, profile=profile,
                )))
        if server_adapter and case.padding is None and case.name != "empty-data-sibling":
            tasks.append(("server-download", lambda: server_case(
                server_adapter, case, directory / "server-download" / case.name, upload=False, profile=profile,
            )))
            if case.qualification:
                tasks.append(("server-upload", lambda: server_case(
                    server_adapter, case, directory / "server-upload" / case.name, upload=True, profile=profile,
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
    parser.add_argument("--profile", choices=PROFILES, default="canonical")
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
        profile=args.profile,
    )


if __name__ == "__main__":
    main()
