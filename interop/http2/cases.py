"""Executed socket workloads. This is not a list of future capabilities."""

from dataclasses import dataclass, field

from peer import digest_for
from socket_peer import require


@dataclass(frozen=True)
class Case:
    name: str
    length: int
    count: int = 1
    concurrency: int = 1
    config: dict = field(default_factory=dict)
    route: str = "bytes"
    method: str = "GET"
    padding: int | None = None
    qualification: str | None = None
    actions: tuple = ()

    def spec(self, address, *, upload=False):
        route = "echo" if upload else self.route
        return {
            "schema": 1, "host": address[0], "port": address[1],
            "timeout_ms": 60000, "config": self.config,
            "request_count": self.count, "concurrency": self.concurrency,
            "requests": [{
                "method": "POST" if upload else self.method,
                "path": f"/{route}" if route in ("echo", "no-content", "early")
                else f"/{route}/{self.length}",
                "body_bytes": self.length if upload else 0, "trailers": [],
            } for _ in range(self.count)],
            "actions": list(self.actions),
        }


def flow_cases():
    cases = []
    for label, config, single, small, count in (
        ("reduced", {"stream_window": 1024, "connection_window": 65535}, 131087, 512, 320),
        ("standard", {"stream_window": 65535, "connection_window": 65535}, 131087, 32768, 600),
        ("default", {}, 16 * 1024 * 1024 + 17, 32768, 600),
    ):
        cases.append(Case(f"{label}-single", single, config=config, qualification="single"))
        for concurrency in (1, 8):
            cases.append(Case(
                f"{label}-aggregate-{'serial' if concurrency == 1 else 'concurrent'}",
                small, count, concurrency, config, qualification="aggregate",
            ))
    return cases


def semantic_cases():
    return [
        Case("trailers", 131087, config={"stream_window": 1024}, route="trailers"),
        Case("informational", 37, route="informational"),
        Case("head", 999, method="HEAD"),
        Case("no-content", 0, route="no-content"),
        Case("padding", 131087, config={"stream_window": 1024}, padding=17),
        Case(
            "paused-consumer", 4096, 2, 2, {"stream_window": 1024},
            actions=({"action": "pause", "stream_id": 1, "until_stream_ended": 3},),
        ),
        Case(
            "empty-data-sibling", 4096, 2, 2, {"stream_window": 1024},
            actions=({"action": "pause", "stream_id": 1, "until_stream_ended": 3},),
        ),
    ]


def qualify(case, credit):
    initial_connection = credit["initial_connection"]
    initial_streams = [entry["initial"] for entry in credit["streams"]]
    require(initial_streams, "missing measured stream windows")
    if case.qualification == "single":
        require(
            case.length >= 2 * max(initial_connection, *initial_streams) + 17,
            "unqualified workload: body must exceed twice BOTH actual initial windows",
        )
        require(credit["connection_window_update"] > 0, "no connection refunds observed")
        require(credit["streams"][0]["window_update"] > 0, "no stream refunds observed")
    elif case.qualification == "aggregate":
        require(
            all(case.length < window for window in initial_streams),
            "unqualified workload: small stream is not smaller than actual stream window",
        )
        require(
            case.length * case.count > 2 * initial_connection,
            "unqualified workload: aggregate must exceed twice actual connection window",
        )
        require(credit["connection_window_update"] > 0, "no aggregate connection refunds")
    if case.name.startswith("default-"):
        require(
            max(initial_connection, *initial_streams) <= 8 * 1024 * 1024,
            "default window exceeds the qualified 8 MiB limit",
        )


def validate_results(case, report, *, echo=False):
    require(report.get("schema") == 1, "result schema must be 1")
    connection = report.get("connection", {})
    require(connection.get("closed") is True, "client did not explicitly close its socket")
    require(connection.get("error") is None, f"connection error: {connection}")
    streams = report.get("streams")
    require(isinstance(streams, list) and len(streams) == case.count, "wrong result count")
    indexed = {entry["stream_id"]: entry for entry in streams}
    require(set(indexed) == set(range(1, 2 * case.count, 2)), "missing or duplicate stream ID")
    for stream, result in indexed.items():
        limited = (
            case.name == "empty-data-sibling" and stream == 1
            and result.get("error") == {"scope": "stream", "code": 11}
        )
        length = 0 if case.method == "HEAD" or case.route == "no-content" or limited else case.length
        require(result.get("status") == (204 if case.route == "no-content" else 200), "wrong status")
        require("content_length" in result, "missing declared content length")
        declared = result["content_length"]
        if echo:
            require(declared is None or declared == length, "echo declared length mismatch")
        else:
            expected_length = None if case.route == "no-content" or case.method == "CONNECT" else case.length
            require(declared == expected_length, f"stream {stream}: declared content length mismatch")
        require(result.get("bytes") == length, f"stream {stream}: body length mismatch")
        require(result.get("sha256") == digest_for(stream, length), f"stream {stream}: body hash mismatch")
        require(result.get("ended") is (not limited), f"stream {stream}: wrong END_STREAM result")
        require(
            result.get("error") == ({"scope": "stream", "code": 11} if limited else None),
            f"stream {stream}: unexpected error",
        )
        require(result.get("trailers") == ([["x-end", "done"]] if case.route == "trailers" else []), "wrong trailers")
        require(result.get("informational") == ([103] if case.route == "informational" else []), "wrong informational sequence")
