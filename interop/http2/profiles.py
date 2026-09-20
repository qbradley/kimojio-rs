"""Explicit qualification profiles; canonical wire-fidelity checks remain the default."""

from socket_peer import require

PROFILES = ("canonical", "wrapper")
WRAPPER_TRAILERS = [("x-list", "one"), ("x-other", "marker"), ("x-list", "two")]


def trailer_values(fields):
    require(isinstance(fields, (list, tuple)), "trailers must be a field sequence")
    values = {}
    for pair in fields:
        require(isinstance(pair, (list, tuple)) and len(pair) == 2, "invalid trailer pair")
        name, value = pair
        require(isinstance(name, str) and isinstance(value, str) and name, "invalid trailer field")
        values.setdefault(name.lower(), []).append(value)
    return values


def trailers_equal(actual, expected, profile):
    require(profile in PROFILES, "unknown qualification profile")
    if profile == "canonical":
        return actual == expected
    return trailer_values(actual) == trailer_values(expected)


def prepend_warmups(spec, count):
    require(count == spec["concurrency"] and count > 0, "warmups must fill application concurrency")
    return {
        **spec,
        "requests": [
            {"method": "GET", "path": "/bytes/0", "body_bytes": 0, "trailers": []}
            for _ in range(count)
        ] + spec["requests"],
        "request_count": spec["request_count"] + count,
    }


def without_warmups(report, count, profile):
    from cases import Case, validate_results

    warm = [s for s in report.get("streams", []) if s["stream_id"] <= 2 * count]
    validate_results(Case("warmup", 0, count, count), {**report, "streams": warm}, profile=profile)
    return {**report, "streams": [s for s in report["streams"] if s["stream_id"] > 2 * count]}
