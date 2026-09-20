"""Independent Go Framer/HPACK scenarios against a command-driven client."""

import argparse
from pathlib import Path
import sys
import uuid

from peer import digest_for
from cases import Case, SUCCESS_CONNECTION_OUTCOMES, validate_results
from socket_peer import PeerProcess, prerequisites, require, run_command
from suite import ROOT, adapter, command, read_json, write_json

SCENARIOS = (
    "connect", "goaway", "continuation", "bad-continuation", "push-before-ack",
    "push-after-ack", "reset-isolation", "content-length", "early-response",
    "reset-discard", "graceful-close", "no-body-data",
)
SERVER_SCENARIOS = ("connect", "request-continuation", "request-hpack-reuse", "request-trailers")


def request(scenario, address):
    two = scenario in ("reset-isolation", "content-length", "early-response", "reset-discard", "no-body-data")
    requests = [{
        "method": "CONNECT" if scenario == "connect" else "POST" if scenario == "early-response" else "GET",
        "path": "/protocol", "body_bytes": 37 if scenario == "connect" else 131087 if scenario == "early-response" else 0,
        "trailers": [],
    }]
    if two:
        requests.append({"method": "GET", "path": "/sibling", "body_bytes": 0, "trailers": []})
    actions = []
    if scenario == "reset-discard":
        actions = [{"action": "reset", "stream_id": 1, "after_bytes": 1024, "code": 8}]
    if scenario == "graceful-close":
        actions = [{"action": "graceful_close", "after_streams": 1}]
    return {
        "schema": 1, "host": address[0], "port": address[1],
        "timeout_ms": 8000,
        "config": {"stream_window": 65535, "connection_window": 65535} if scenario == "reset-discard" else {},
        "request_count": len(requests),
        "concurrency": len(requests), "requests": requests, "actions": actions,
    }


def validate(scenario, report, witness, *, early_policy="peer-reset"):
    require(early_policy in ("peer-reset", "application-cancel"), "unknown early-response policy")
    require(report.get("schema") == 1, "result schema mismatch")
    require(report.get("connection", {}).get("closed") is True, "missing explicit client close")
    require(witness.get("peer_eof") is True, "server did not observe socket close")
    connection_error = {"scope": "connection", "code": 1} if scenario in (
        "bad-continuation", "push-after-ack",
    ) else None
    require(report["connection"].get("error") == connection_error, "wrong connection error scope/code")
    require(
        report["connection"].get("outcome") in (
            {"protocol"} if connection_error else SUCCESS_CONNECTION_OUTCOMES
        ),
        "wrong connection terminal outcome",
    )
    count = 2 if scenario in ("reset-isolation", "content-length", "early-response", "reset-discard", "no-body-data") else 1
    require(witness["requests"] == count, "wire request count mismatch")
    results = report.get("streams", [])
    require(len(results) == count, "wrong result count")
    require({result["stream_id"] for result in results} == set(range(1, 2 * count, 2)), "wrong stream IDs")
    for result in results:
        stream = result["stream_id"]
        status, length, ended, error, outcome = 200, 37, True, None, "complete"
        if connection_error:
            status, length, ended = None, 0, False
            outcome = "connection_failed"
        elif stream == 1 and scenario == "reset-isolation":
            status, length, ended = None, 0, False
            error = {"scope": "stream", "code": 8}
        elif stream == 1 and scenario == "content-length":
            ended, error = False, {"scope": "stream", "code": 1}
        elif stream == 1 and scenario == "reset-discard":
            length, ended, error = 1024, False, {"scope": "stream", "code": 8}
        elif stream == 1 and scenario == "no-body-data":
            status, length, ended, error = 204, 0, False, {"scope": "stream", "code": 1}
        elif stream == 1 and scenario == "early-response":
            status, length = 413, 0
            error = {"scope": "stream", "code": 8 if early_policy == "application-cancel" else 0}
        if error is not None:
            outcome = "reset"
        require(result.get("status") == status, f"stream {stream}: wrong status")
        declared = None if status is None or status == 204 or scenario == "connect" else length
        if stream == 1 and scenario == "content-length":
            declared = 38
        if stream == 1 and scenario == "reset-discard":
            declared = 65535
        require("content_length" in result and result["content_length"] == declared, "wrong declared content length")
        require(result.get("bytes") == length, f"stream {stream}: wrong body length")
        require(result.get("sha256") == digest_for(stream, length), f"stream {stream}: wrong body hash")
        require(result.get("ended") is ended, f"stream {stream}: wrong END_STREAM result")
        require(result.get("error") == error, f"stream {stream}: wrong error scope/code")
        require(result.get("outcome") == outcome, f"stream {stream}: wrong terminal outcome")
        require(result.get("trailers") == [] and result.get("informational") == [], "unexpected metadata")
    if scenario == "push-before-ack":
        require(witness["resets"].get("2") in (7, 8), "client did not refuse the promised stream")
    if scenario.startswith("push-"):
        require(witness["push_disabled"], "client did not disable push")
    if scenario in ("content-length", "no-body-data"):
        require(witness["resets"].get("1") == 1, "length mismatch did not produce stream PROTOCOL_ERROR")
        require(witness["goaway"] == 0, "stream error escalated to GOAWAY")
    if connection_error:
        require(witness["goaway"] == 1, "missing wire GOAWAY(PROTOCOL_ERROR)")
    if scenario == "early-response":
        require(witness["received"].get("1") == 1024, "upload did not stop at actual advertised stream credit")
        require(witness.get("request_ended", {}).get("1") is False, "upload ended instead of remaining blocked")
        require(witness.get("request_ended", {}).get("3") is True, "sibling request did not end")
        require(witness["received"].get("3", 0) == 0, "bodyless sibling sent request DATA")
        require(not witness["window_updates"].get("1"), "unexpected response receive-credit update")
        if early_policy == "peer-reset":
            require(witness.get("early_response_barrier") is True, "response END_STREAM barrier was not acknowledged")
            require(witness.get("server_resets") == {"1": 0}, "missing server RST_STREAM(NO_ERROR)")
            require(not witness["resets"], "client reset before or in response to server NO_ERROR reset")
        else:
            require(witness.get("early_response_barrier") is False, "application-cancel peer unexpectedly sent a barrier")
            require(witness.get("server_resets") == {}, "application-cancel peer unexpectedly reset the upload")
            require(witness["resets"] == {"1": 8}, "missing explicit application CANCEL")
        require(report["connection"]["outcome"] == "graceful", "early response did not close gracefully")
    if scenario == "reset-discard":
        require(witness["resets"].get("1") == 8, "missing client CANCEL")
        require(witness["window_updates"].get("0", 0) >= 32768, "discarded DATA stranded connection credit")
        require(
            witness["window_updates"].get("1", 0) == witness["stream_updates_at_reset"].get("1", 0),
            "discarded DATA reopened stream credit",
        )
    if scenario == "graceful-close":
        require(report["connection"]["outcome"] == "graceful", "graceful close did not finish gracefully")
        require(witness["goaway_count"] >= 1 and witness["goaway"] == 0, "missing graceful GOAWAY before actual close")


def run_case(binary, client_adapter, scenario, directory, *, early_policy="peer-reset"):
    directory.mkdir(parents=True)
    witness_file = directory / "peer.json"
    peer_scenario = (
        "early-response-app-cancel"
        if scenario == "early-response" and early_policy == "application-cancel" else scenario
    )
    with PeerProcess([
        str(binary), "--scenario", peer_scenario, "--report", str(witness_file),
    ], cwd=ROOT) as server:
        request_file, result_file = directory / "request.json", directory / "result.json"
        write_json(request_file, request(scenario, server.address))
        completed = run_command(command(client_adapter, request_file, result_file), cwd=ROOT, timeout=12)
        require(completed.returncode == 0, f"client failed: {completed.stderr!r}")
        code = server.process.wait(timeout=3)
        for thread in server._threads:
            thread.join(timeout=1)
            require(not thread.is_alive(), "server output reader survived exit")
        require(code == 0, f"Go peer failed: {server.diagnostics()}")
        require(server.output_bytes <= 128 * 1024, "server output exceeds bound")
        result, witness = read_json(result_file), read_json(witness_file)
        validate(scenario, result, witness, early_policy=early_policy)
    return {
        "name": scenario, "passed": True,
        **({"early_response_policy": early_policy} if scenario == "early-response" else {}),
    }


def server_case(binary, server_adapter, scenario, directory):
    directory.mkdir(parents=True)
    server_request = directory / "server.json"
    write_json(server_request, {"schema": 1, "config": {}, "timeout_ms": 10000})
    if server_adapter is None and scenario == "connect":
        server_command = [
            str(binary), "--scenario", "connect", "--report", str(directory / "peer.json"),
        ]
    elif server_adapter is None:
        server_command = [
            sys.executable, str(Path(__file__).with_name("reference.py")), "server", str(server_request),
        ]
    else:
        server_command = command(server_adapter, server_request)
    with PeerProcess(server_command, cwd=ROOT) as server:
        count = 2 if scenario == "request-hpack-reuse" else 1
        case = Case(scenario, 37, count, count, method="CONNECT" if scenario == "connect" else "GET")
        spec = case.spec(server.address, upload=scenario == "request-trailers")
        spec["wire_scenario"] = scenario
        if scenario == "connect":
            spec["requests"][0].update(method="CONNECT", body_bytes=37)
        if scenario == "request-trailers":
            spec["requests"][0]["trailers"] = [["x-end", "done"]]
        request_file, result_file = directory / "request.json", directory / "result.json"
        write_json(request_file, spec)
        completed = run_command(
            [str(binary), "--client", str(request_file), "--result", str(result_file)],
            cwd=ROOT, timeout=12,
        )
        require(completed.returncode == 0, f"Go client failed: {completed.stderr!r}")
        validate_results(case, read_json(result_file), echo=scenario == "request-trailers")
        require(server.output_bytes <= 128 * 1024, "server output exceeds bound")
    return {"name": scenario, "role": "server", "passed": True}


def main():
    prerequisites()
    parser = argparse.ArgumentParser()
    parser.add_argument("--go-peer", type=Path, required=True)
    parser.add_argument("--client-adapter", type=Path)
    parser.add_argument("--server-adapter", type=Path)
    parser.add_argument("--selftest", action="store_true")
    parser.add_argument("--early-response-policy", choices=("peer-reset", "application-cancel"), default="peer-reset")
    parser.add_argument("--case", action="append", choices=sorted(set(SCENARIOS + SERVER_SCENARIOS)))
    parser.add_argument("--output", type=Path, default=ROOT / "target" / "http2-protocol" / uuid.uuid4().hex)
    args = parser.parse_args()
    binary = args.go_peer.resolve()
    require(binary.is_file(), "Go peer is missing; build interop/http2/gopeer first")
    directory = args.output.resolve()
    require("target" in directory.parts, "reports must reside under target")
    if args.selftest:
        require(not args.client_adapter and not args.server_adapter, "selftest cannot use native adapter")
        client_adapter = {"command": [
            str(binary), "--client", "{request_file}", "--result", "{result_file}",
            "--early-response-policy", args.early_response_policy,
        ]}
        server_adapter = None
    else:
        require(args.client_adapter or args.server_adapter, "at least one adapter is required")
        client_adapter = adapter(args.client_adapter, client_role=True) if args.client_adapter else None
        server_adapter = adapter(args.server_adapter, client_role=False) if args.server_adapter else None
    directory.mkdir(parents=True)
    report = {
        "schema": 1, "evidence": "Go-Framer-peer-selftest-NOT-core" if args.selftest else "adapter",
        "early_response_policy": args.early_response_policy,
        "cases": [], "failures": [],
    }
    client_cases = [name for name in args.case if name in SCENARIOS] if args.case else SCENARIOS
    server_cases = [name for name in args.case if name in SERVER_SCENARIOS] if args.case else SERVER_SCENARIOS
    for scenario in client_cases if client_adapter else ():
        try:
            result = run_case(
                binary, client_adapter, scenario, directory / "client" / scenario,
                early_policy=args.early_response_policy,
            )
            result["role"] = "client"
            report["cases"].append(result)
            print(f"PASS client {scenario}", flush=True)
        except Exception as error:
            report["failures"].append({"name": scenario, "error": f"{type(error).__name__}: {error}"[:8192]})
            print(f"FAIL {scenario}: {error}", file=sys.stderr, flush=True)
    for scenario in server_cases if args.selftest or server_adapter else ():
        try:
            report["cases"].append(server_case(binary, server_adapter, scenario, directory / "server" / scenario))
            print(f"PASS server {scenario}", flush=True)
        except Exception as error:
            report["failures"].append({"role": "server", "name": scenario, "error": f"{type(error).__name__}: {error}"[:8192]})
            print(f"FAIL server {scenario}: {error}", file=sys.stderr, flush=True)
    write_json(directory / "report.json", report)
    require(report["cases"] or report["failures"], "no cases match the selected adapter roles")
    require(not report["failures"], f"protocol suite failed; see {directory / 'report.json'}")


if __name__ == "__main__":
    main()
