#!/usr/bin/env python3
"""Generates the http2jp/hpack-test-case interop corpus for on-demand validation.

The corpus holds the same request sequences encoded by many independent HPACK
encoders. Decoding them exercises representation choices our own encoder never
makes, which is the point: an encoder and decoder written together agree on
each other's blind spots.

The output is deliberately not committed. Measured against injected decoder
faults, the corpus uniquely caught only static table entry corruption, which
kimojio-fsm-http/tests/hpack_static_table.rs now covers outright at no storage
cost. Generate it when decoder internals change and you want the broader check:

    git clone https://github.com/http2jp/hpack-test-case.git /tmp/hpack-tc
    python3 scripts/vendor-hpack-interop.py /tmp/hpack-tc > /tmp/corpus.txt
    KIMOJIO_HPACK_INTEROP_CORPUS_V1=/tmp/corpus.txt \\
        cargo test -p kimojio-fsm-http --all-features --test hpack_interop

The upstream corpus is 66 MiB, most of it long packet captures that repeat
representations the short stories already cover. This emits every encoder but
only the short stories, and stores the expected headers once per case rather
than once per encoder, which fits 1,190 blocks into 386 KiB.
"""

import json
import os
import subprocess
import sys

STORIES = [f"story_{index:02d}" for index in range(10)]
# The corpus repository stores the decoded stories alongside the encoded ones.
NOT_AN_ENCODER = {"raw-data", "util"}


def encoders(root):
    return sorted(
        name
        for name in os.listdir(root)
        if name not in NOT_AN_ENCODER
        and not name.startswith(".")
        and os.path.isdir(os.path.join(root, name))
    )


def load(root, encoder, story):
    path = os.path.join(root, encoder, f"{story}.json")
    if not os.path.exists(path):
        return None
    with open(path, encoding="utf-8") as handle:
        return json.load(handle)["cases"]


def fields(case):
    return [item for header in case["headers"] for item in header.items()]


def main():
    if len(sys.argv) != 2:
        sys.exit(f"usage: {sys.argv[0]} <hpack-test-case checkout>")
    root = sys.argv[1]
    names = encoders(root)
    commit = subprocess.run(
        ["git", "-C", root, "rev-parse", "HEAD"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()

    out = sys.stdout
    out.write("# HPACK interop corpus v1\n")
    out.write("# source: https://github.com/http2jp/hpack-test-case\n")
    out.write(f"# commit: {commit}\n")
    out.write("# regenerate: scripts/vendor-hpack-interop.py\n")
    out.write(f"# encoders: {' '.join(names)}\n")

    for story in STORIES:
        expected = None
        loaded = {}
        for encoder in names:
            cases = load(root, encoder, story)
            if cases is None:
                continue
            loaded[encoder] = cases
            observed = [fields(case) for case in cases]
            if expected is None:
                expected = observed
            elif observed != expected:
                sys.exit(f"{story}: {encoder} disagrees on the decoded headers")
        if expected is None:
            continue

        out.write(f"S {story}\n")
        for index, case_fields in enumerate(expected):
            out.write("C\n")
            for name, value in case_fields:
                if any(character in name + value for character in "\t\r\n"):
                    sys.exit(f"{story} case {index}: field contains a separator")
                out.write(f"H {name}\t{value}\n")
            for encoder, cases in loaded.items():
                case = cases[index]
                # Absent means the encoder used RFC 7541's 4,096-byte default.
                size = case.get("header_table_size")
                out.write(f"W {encoder} {size if size is not None else '-'} {case['wire']}\n")


if __name__ == "__main__":
    main()
