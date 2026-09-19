"""Summarize per-trial results without pooling native quantiles or hiding failures."""

import argparse
import collections
import gzip
import hashlib
import json
from pathlib import Path
import statistics


def read_report(path):
    if path.suffix == ".gz":
        with gzip.open(path, "rt") as source:
            return json.load(source)
    return json.loads(path.read_text())


def spread(values):
    present = [value for value in values if value is not None]
    return {"median": statistics.median(present), "min": min(present), "max": max(present)} if present else None


def context_deltas(row):
    first = row["server_before"]["sampled_thread_context_switches"]
    peaks = {}
    for sample in [row["server_before"], *row["server_samples"], row["server_after"]]:
        for tid, counters in sample["sampled_thread_context_switches"].items():
            for name, value in counters.items():
                key = tid, name
                peaks[key] = max(peaks.get(key, 0), value)
    return {
        name: sum(value - first.get(tid, {}).get(name, 0)
                  for (tid, counter), value in peaks.items() if counter == name)
        for name in ("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")
    }


def summarize(report, source):
    groups = collections.defaultdict(list)
    failures = []
    total_errors = 0
    for index, row in enumerate(report["results"]):
        total_errors += len(row.get("errors", [])) + len(row.get("client", {}).get("errors") or [])
        if not row["valid"]:
            failures.append({"index": index, "row": row})
            continue
        case = row["case"]
        label = row["target"]["name"] if row["comparison"] == "servers" else row["client_kind"] + "-client"
        key = (row["comparison"], case["kind"], case["size"],
               case.get("concurrency", case.get("fanout")), case.get("fresh", False),
               case.get("framing", "websocket"), label)
        groups[key].append((index, row))
    summaries = []
    for key, rows in sorted(groups.items()):
        trials = []
        for index, row in rows:
            client = row["client"]
            count = client["successes"]
            elapsed = client["elapsed_seconds"]
            cpu = client.get("measurement_cpu_total_seconds")
            if cpu is None:
                cpu = client["cpu_user_seconds"] + client["cpu_system_seconds"]
            server_cpu = sum(row["server_cpu"].values())
            contexts = context_deltas(row)
            trials.append({
                "row": index, "repetition": row["repetition"], "successes": count,
                "attempts": client["attempts"], "elapsed_seconds": elapsed,
                "operations_per_second": client["successes_per_second"],
                "p50_ms": client["p50_ms"], "p95_ms": client["p95_ms"], "p99_ms": client["p99_ms"],
                "client_measurement_cpu_seconds": cpu, "client_cpu_core_equivalents": cpu / elapsed,
                "client_cpu_us_per_operation": cpu * 1e6 / count,
                "client_whole_user_seconds": client["process_cpu_user_seconds"],
                "client_whole_system_seconds": client["process_cpu_system_seconds"],
                "client_max_rss_mib": client["max_rss_kib"] / 1024,
                "client_voluntary_switches": client["voluntary_context_switches"],
                "client_involuntary_switches": client["involuntary_context_switches"],
                "client_max_sampled_fds": client.get("max_sampled_fds"),
                "client_final_sampled_fds": client.get("final_sampled_fds"),
                "client_self_reported_final_fds": client.get("final_fds"),
                "server_user_seconds": row["server_cpu"]["cpu_user_seconds"],
                "server_system_seconds": row["server_cpu"]["cpu_system_seconds"],
                "server_cpu_core_equivalents": server_cpu / row["server_cpu_window_seconds"],
                "server_cpu_us_per_operation": server_cpu * 1e6 / count,
                "server_max_sampled_rss_mib": row["server_max_sampled_rss_kib"] / 1024,
                "server_retained_rss_mib": row["retained_rss_samples"][-1]["rss_kib"] / 1024,
                "server_retained_rss_delta_mib": (
                    row["retained_rss_samples"][-1]["rss_kib"] - row["server_before"]["rss_kib"]) / 1024,
                "server_fds_before": row["server_before"]["fds"],
                "server_fds_after": row["server_after"]["fds"],
                "server_fds_retained": row["retained_rss_samples"][-1]["fds"],
                "server_sampled_voluntary_switches": contexts["voluntary_ctxt_switches"],
                "server_sampled_involuntary_switches": contexts["nonvoluntary_ctxt_switches"],
                "new_connections": client["new_connections"],
                "limit_retirements": client.get("limit_retirements"),
                "fresh_closes": client.get("fresh_closes"),
            })
        summaries.append({
            "comparison": key[0], "kind": key[1], "size": key[2], "concurrency_or_fanout": key[3],
            "fresh": key[4], "framing": key[5], "implementation": key[6],
            "trial_count": len(trials), "trials": trials,
            "metrics": {name: spread([trial[name] for trial in trials])
                        for name in trials[0] if name not in ("row", "repetition")},
        })
    return {
        "schema": 1, "source_report": str(source),
        "source_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
        "rows": len(report["results"]), "valid_rows": sum(row["valid"] for row in report["results"]),
        "error_count": total_errors, "failures": failures,
        "configuration": report["configuration"], "platform": report["platform"],
        "tool": report["tool"], "server_manifest": report["manifest"],
        "native_client_publication": report.get("native_client_publication"),
        "groups": summaries,
        "quantile_scope": "Median/min/max of per-trial quantiles; no pooling of native quantiles or inferred bins.",
        "cpu_scope": "Client measurement total uses its own elapsed window; server CPU uses observer invocation window. Whole-process counters remain separate.",
        "context_scope": "Server counters are sampled per-thread lower bounds; client counters are process rusage.",
    }


def markdown(summary):
    lines = (["**Invalid rows are retained in the JSON. Do not compare incomplete trial groups.**", ""]
             if summary["failures"] else []) + [
        "| Comparison | Case | Implementation | ops/s median [min–max] | p99 ms median [min–max] | client CPU cores | server CPU cores | server RSS MiB |",
        "|---|---|---|---:|---:|---:|---:|---:|",
    ]
    def interval(value, places):
        return f"{value['median']:.{places}f} [{value['min']:.{places}f}–{value['max']:.{places}f}]"
    for group in summary["groups"]:
        m = group["metrics"]
        case = f"{group['kind']} {group['size']}B {'f' if group['kind']=='ws' else 'c'}{group['concurrency_or_fanout']}"
        if group["fresh"]:
            case += " fresh"
        if group["framing"] == "trailers":
            case += " chunked"
        elif group["framing"] == "chunked-echo":
            case += " chunked POST echo"
        lines.append(
            f"| {group['comparison']} | {case} | {group['implementation']} | "
            f"{interval(m['operations_per_second'],1)} | {interval(m['p99_ms'],3)} | "
            f"{m['client_cpu_core_equivalents']['median']:.2f} | "
            f"{m['server_cpu_core_equivalents']['median']:.2f} | "
            f"{m['server_max_sampled_rss_mib']['median']:.2f} |"
        )
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("report", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--table", type=Path)
    args = parser.parse_args()
    result = summarize(read_report(args.report), args.report)
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    if args.table:
        args.table.write_text(markdown(result))
    print(f"{result['valid_rows']}/{result['rows']} valid, errors={result['error_count']}, groups={len(result['groups'])}")
    return 0 if not result["failures"] and not result["error_count"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
