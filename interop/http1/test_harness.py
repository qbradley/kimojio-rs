"""Self-tests for the harness, independent of the new Rust implementations."""

from __future__ import annotations

import base64
import json
import os
import shutil
import socket
import sys
import time
import unittest

from harness import PeerProcess, ProtocolError, Wire, request, run_command
from client_suite import run_suite as run_client_suite
from duplex_suite import client_case as duplex_client_case, server_case as duplex_server_case
from static_transport_suite import prepare_large_file, run_probes
from scripted import ScriptedPeer
from server_suite import ARTIFACTS, WORKSPACE, create_fixtures, go_client_checks, run_suite


class MemorySocket:
    """Deliver explicit receive boundaries without relying on TCP segmentation."""

    def __init__(self, parts):
        self.parts = list(parts)

    def settimeout(self, _timeout):
        pass

    def recv(self, count):
        while self.parts and not self.parts[0]:
            self.parts.pop(0)
        if not self.parts:
            return b""
        data = self.parts[0][:count]
        self.parts[0] = self.parts[0][count:]
        return data


class FramingTests(unittest.TestCase):
    def parse(self, raw, **kwargs):
        return Wire(MemorySocket([raw]), **kwargs)

    def test_language_neutral_framing_corpus(self):
        corpus = json.loads((WORKSPACE / "interop/http1/framing_cases.json").read_text())
        self.assertEqual(corpus["schema"], 1)
        for case in corpus["cases"]:
            raw = case["wire"].encode(corpus["encoding"])
            for cut in range(len(raw) + 1):
                with self.subTest(case=case["name"], cut=cut):
                    sock = MemorySocket([raw[:cut], raw[cut:]])
                    wire = Wire(sock)
                    if case.get("error"):
                        with self.assertRaises(ProtocolError):
                            wire.response(case["method"])
                    else:
                        response = wire.response(case["method"])
                        self.assertEqual(response.status, case["status"])
                        self.assertEqual(response.body, case["body"].encode("latin-1"))
                        leftover = case["leftover"].encode("latin-1")
                        # Bytes can remain in transport input or in the read-ahead buffer.
                        self.assertEqual(bytes(wire.buffer) + b"".join(sock.parts), leftover)
                        self.assertEqual(response.wire, raw[: len(raw) - len(leftover)])
                        self.assertEqual(
                            response.trailers,
                            tuple((k.encode(), v.encode()) for k, v in case.get("trailers", [])),
                        )
                        if case.get("requires_eof"):
                            self.assertTrue(wire.eof)

    def test_every_split_and_bytewise_full_sequence(self):
        first = b"HTTP/1.1 100 Continue\r\n\r\n"
        second = (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTrailer: X-End\r\n\r\n"
            b'3;name="a;b"\r\none\r\n3\r\ntwo\r\n0\r\nX-End: done\r\n\r\n'
        )
        third = b"HTTP/1.1 204 No Content\r\n\r\n"
        raw = first + second + third
        schedules = [[raw[:cut], raw[cut:]] for cut in range(len(raw) + 1)]
        schedules.append([bytes([byte]) for byte in raw])
        for parts in schedules:
            with self.subTest(parts=len(parts), first_size=len(parts[0])):
                wire = Wire(MemorySocket(parts))
                interim, response = wire.final_response()
                self.assertEqual([item.wire for item in interim], [first])
                self.assertEqual(response.wire, second)
                self.assertEqual(response.body, b"onetwo")
                self.assertEqual(response.trailers, ((b"x-end", b"done"),))
                self.assertEqual(wire.response().wire, third)
                wire.expect_eof()

    def test_content_length_every_truncation_fails(self):
        raw = b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nbody"
        for cut in range(len(raw)):
            with self.subTest(cut=cut), self.assertRaises(ProtocolError):
                self.parse(raw[:cut]).response()
        self.assertEqual(self.parse(raw).response().body, b"body")

    def test_chunked_every_truncation_fails(self):
        raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nab\r\n0\r\n\r\n"
        for cut in range(len(raw)):
            with self.subTest(cut=cut), self.assertRaises(ProtocolError):
                self.parse(raw[:cut]).response()

    def test_bodyless_and_upgrade_leave_exact_bytes(self):
        for method, status, framing in (
            ("HEAD", b"200 OK", b"Content-Length: 999\r\n"),
            ("GET", b"304 Not Modified", b"Content-Length: 999\r\n"),
            ("GET", b"101 Switching Protocols", b"Upgrade: websocket\r\nConnection: Upgrade\r\n"),
            ("CONNECT", b"200 Connection Established", b""),
            (
                "CONNECT",
                b"200 Connection Established",
                b"Content-Length: invalid\r\nTransfer-Encoding: ignored\r\n",
            ),
        ):
            with self.subTest(method=method, status=status):
                head = b"HTTP/1.1 " + status + b"\r\n" + framing + b"\r\n"
                wire = self.parse(head + b"\x81\x02hi")
                response = wire.response(method)
                self.assertEqual(response.body, b"")
                self.assertEqual(response.wire, head)
                self.assertEqual(bytes(wire.buffer), b"\x81\x02hi")

    def test_close_delimited_requires_eof(self):
        wire = self.parse(b"HTTP/1.0 200 OK\r\n\r\nbody")
        self.assertEqual(wire.response().body, b"body")
        self.assertTrue(wire.eof)
        wire.expect_eof()

    def test_duplicate_equal_lengths_and_empty_body(self):
        wire = self.parse(
            b"HTTP/1.1 200 OK\r\nContent-Length: 0, 0\r\nContent-Length: 0\r\n\r\n"
        )
        self.assertEqual(wire.response().body, b"")
        wire.expect_eof()

    def test_malformed_framing_is_not_accepted(self):
        heads = [
            b"Content-Length: -1",
            b"Content-Length: +1",
            b"Content-Length: 1x",
            b"Content-Length: 1, 2",
            b"Content-Length: 1\r\nContent-Length: 2",
            b"Content-Length: 9999999999999999999999999999999",
            b"Transfer-Encoding: chunked\r\nContent-Length: 0",
            b"Transfer-Encoding: chunked, gzip",
            b"Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked",
            b"Transfer-Encoding: identity",
            b"Bad Name: x",
            b"Content-Length : 0",
            b"X: a\r\n folded",
            b"X: a\x00b",
            b"X: a\x7fb",
        ]
        for head in heads:
            with self.subTest(head=head), self.assertRaises(ProtocolError):
                self.parse(b"HTTP/1.1 200 OK\r\n" + head + b"\r\n\r\n0\r\n\r\n").response()
        for status in (b"100 Continue", b"101 Switching Protocols", b"204 No Content"):
            with self.subTest(status=status), self.assertRaises(ProtocolError):
                self.parse(b"HTTP/1.1 " + status + b"\r\nContent-Length: 0\r\n\r\n").response()

    def test_malformed_chunks_are_not_accepted(self):
        chunks = [
            b"-1\r\na\r\n0\r\n\r\n",
            b"+1\r\na\r\n0\r\n\r\n",
            b" 1\r\na\r\n0\r\n\r\n",
            b"0x1\r\na\r\n0\r\n\r\n",
            b"1\r\naX\n0\r\n\r\n",
            b"1;=invalid\r\na\r\n0\r\n\r\n",
            b'1;x="unterminated\r\na\r\n0\r\n\r\n',
            b"0\r\nContent-Length: 0\r\n\r\n",
            b"0\r\nTransfer-Encoding: chunked\r\n\r\n",
            b"0\r\nBad Name: x\r\n\r\n",
        ]
        for chunk in chunks:
            with self.subTest(chunk=chunk), self.assertRaises(ProtocolError):
                self.parse(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n" + chunk
                ).response()

    def test_limits_and_unconsumed_suffix(self):
        with self.assertRaises(ProtocolError):
            self.parse(b"HTTP/1.1 200 OK\r\nX: " + b"a" * 100, max_head=32).response()
        with self.assertRaises(ProtocolError):
            self.parse(
                b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n", max_body=9
            ).response()
        with self.assertRaises(ProtocolError):
            self.parse(b"HTTP/1.0 200 OK\r\n\r\n0123456789", max_body=9).response()
        with self.assertRaises(ProtocolError):
            self.parse(b"HTTP/1.1 204 No Content\r\n\r\n", max_wire=10).response()
        wire = self.parse(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\nsurplus")
        wire.response()
        with self.assertRaises(ProtocolError):
            wire.expect_eof()

    def test_deadline_is_absolute(self):
        wire = self.parse(b"HTTP/1.1 204 No Content\r\n\r\n", timeout=-1)
        with self.assertRaises(TimeoutError):
            wire.response()


class ProcessTests(unittest.TestCase):
    def test_readiness_and_reaping(self):
        command = [
            sys.executable,
            "-u",
            "-c",
            "import time; print('LISTEN 127.0.0.1:12345'); time.sleep(30)",
        ]
        with PeerProcess(command, cwd=WORKSPACE) as child:
            pid = child.process.pid
            self.assertEqual(child.address, ("127.0.0.1", 12345))
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_missing_readiness_times_out_and_reaps(self):
        child = PeerProcess(
            [sys.executable, "-c", "import time; time.sleep(30)"],
            cwd=WORKSPACE,
            startup_timeout=0.1,
            stop_timeout=0.1,
        )
        started = time.monotonic()
        with self.assertRaises(TimeoutError):
            with child:
                self.fail("missing readiness accepted")
        self.assertIsNone(child.process)
        self.assertLess(time.monotonic() - started, 2)

    def test_invalid_address_and_early_exit_fail(self):
        for code in (
            "print('LISTEN 0.0.0.0:8080', flush=True); import time; time.sleep(30)",
            "raise SystemExit(2)",
        ):
            with self.subTest(code=code), self.assertRaises(RuntimeError):
                with PeerProcess([sys.executable, "-c", code], cwd=WORKSPACE):
                    self.fail("invalid readiness accepted")

    def test_uncooperative_child_is_killed(self):
        code = (
            "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); "
            "print('LISTEN 127.0.0.1:12345',flush=True); time.sleep(30)"
        )
        with PeerProcess(
            [sys.executable, "-c", code], cwd=WORKSPACE, stop_timeout=0.1
        ) as child:
            pid = child.process.pid
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_command_exit_output_timeout_and_limit(self):
        result = run_command(
            [sys.executable, "-c", "import sys; print('ok'); sys.exit(7)"], cwd=WORKSPACE
        )
        self.assertEqual((result.returncode, result.stdout), (7, b"ok\n"))
        with self.assertRaises(TimeoutError):
            run_command(
                [sys.executable, "-c", "import time; time.sleep(30)"],
                cwd=WORKSPACE,
                timeout=0.1,
            )
        with self.assertRaises(ProtocolError):
            run_command(
                [sys.executable, "-c", "print('x' * 200000)"],
                cwd=WORKSPACE,
                max_output=1024,
            )

    def test_cli_rejects_nonarray_commands(self):
        for value in ("true", "42", "null", "[]", "{}", '["server", 1]'):
            with self.subTest(value=value):
                result = run_command(
                    [
                        sys.executable, "-B", "interop/http1/server_suite.py",
                        "--server-command", value,
                    ],
                    cwd=WORKSPACE,
                )
                self.assertEqual(result.returncode, 2, result.stderr)
                self.assertNotIn(b"Traceback", result.stderr)

    def test_scripted_peer_propagates_failure(self):
        def fail(wire, _index):
            wire.until(b"\r\n\r\n", 1024)
            raise ProtocolError("intentional worker failure")

        with self.assertRaisesRegex(ProtocolError, "intentional worker"):
            with ScriptedPeer(fail) as peer:
                with socket.create_connection(peer.address, timeout=1) as sock:
                    sock.sendall(request())

    def test_scripted_peer_cleans_up_stalled_accept(self):
        peer = ScriptedPeer(lambda _wire, _index: None, timeout=1)
        with self.assertRaisesRegex(ValueError, "caller failure"):
            with peer:
                raise ValueError("caller failure")
        self.assertFalse(peer.thread.is_alive())


class GoPeerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        go = shutil.which("go")
        if go is None:
            raise RuntimeError("Go is required; this suite does not skip missing peers")
        ARTIFACTS.mkdir(parents=True, exist_ok=True)
        cache = ARTIFACTS / "go-cache"
        scratch = ARTIFACTS / "go-tmp"
        cache.mkdir(exist_ok=True)
        scratch.mkdir(exist_ok=True)
        cls.binary = ARTIFACTS / "http1-peer"
        build = run_command(
            [
                "env",
                f"GOCACHE={cache}",
                f"GOTMPDIR={scratch}",
                f"TMPDIR={scratch}",
                "GOTOOLCHAIN=local",
                "GOPROXY=off",
                "GOSUMDB=off",
                go,
                "build",
                "-o",
                str(cls.binary),
                "interop/http1/peer.go",
            ],
            cwd=WORKSPACE,
            timeout=120,
        )
        if build.returncode:
            raise RuntimeError(build.stderr.decode(errors="replace"))
        cls.root = create_fixtures()
        cls.server = PeerProcess(
            [str(cls.binary), "--bind", "127.0.0.1:0", "--root", str(cls.root)],
            cwd=WORKSPACE,
        )
        cls.server.__enter__()
        cls.addClassCleanup(cls.server.close)
        cls.addClassCleanup(shutil.rmtree, cls.root.parent)

    def test_portable_static_suite(self):
        results = run_suite(self.server.address, "portable")
        self.assertEqual([row for row in results if not row["passed"]], [])
        self.assertEqual(len(results), 27)

    def test_go_client_static_checks(self):
        results = go_client_checks(str(self.binary), self.server.address)
        self.assertEqual([row for row in results if not row["passed"]], [])

    def test_command_json_client_adapter_suite(self):
        adapter = {
            "schema": 1,
            "command": [
                sys.executable, "-B", "interop/http1/go_adapter.py",
                "--peer", str(self.binary),
                "--request", "{request_file}", "--result", "{result_file}",
            ],
            "capabilities": [
                "basic", "reuse", "upload", "chunked-upload", "interim", "trailers", "expect-continue",
            ],
        }
        report = run_client_suite(adapter, self.root.parent / "client-cases")
        self.assertEqual([row for row in report["results"] if not row["passed"]], [])
        self.assertEqual(len(report["results"]), 16)

    def test_independent_go_duplex_reference_cases(self):
        adapter = {
            "command": [
                sys.executable, "-B", "interop/http1/go_adapter.py",
                "--peer", str(self.binary),
                "--request", "{request_file}", "--result", "{result_file}",
            ],
        }
        for expect_continue in (False, True):
            for chunked in (False, True):
                with self.subTest(chunked=chunked, expect_continue=expect_continue):
                    server = duplex_server_case(
                        self.server.address, chunked=chunked, path="/__peer/duplex",
                        expect_continue=expect_continue,
                    )
                    self.assertTrue(server["passed"], server)
                    client = duplex_client_case(
                        adapter, self.root.parent / f"duplex-{chunked}-{expect_continue}",
                        chunked=chunked, expect_continue=expect_continue,
                    )
                    self.assertTrue(client["passed"], client)

    def test_independent_go_reset_recovery_reference(self):
        prepare_large_file(self.root)
        with PeerProcess(
            [
                str(self.binary), "--bind", "127.0.0.1:0", "--root", str(self.root),
                "--max-connections", "1",
            ],
            cwd=WORKSPACE,
        ) as reference:
            report = run_probes(reference.address, reference.process, include_timeout=False)
        self.assertEqual(len(report["results"]), 12)
        self.assertEqual([row for row in report["results"] if not row["passed"]], [])
        self.assertLessEqual(report["final_fds"], report["baseline_fds"] + 2)

    def test_strict_suite_detects_reference_policy_differences(self):
        results = run_suite(self.server.address, "strict")
        failures = {row["case"] for row in results if not row["passed"]}
        # Go accepts three HTTP forms, follows symlinks, and waits on the incomplete prefixes.
        self.assertEqual(
            failures,
            {
                "transfer-encoding-with-length", "folded-field", "bare-lf", "symlink-escape",
                "bare-lf-prefix", "bare-cr-prefix",
            },
        )

    def test_go_chunked_response_and_trailers(self):
        with Wire.connect(self.server.address, recv_size=1) as wire:
            wire.send(request("/__peer/chunked", close=True))
            response = wire.response()
            self.assertEqual(response.body, b"onetwothree")
            self.assertEqual(response.trailers, ((b"x-peer-end", b"done"),))
            wire.expect_eof()

    def test_go_echo_lengths_chunking_and_continue(self):
        for framing, body in (
            (b"Content-Length: 6\r\n", b"onetwo"),
            (b"Transfer-Encoding: chunked\r\n", b"3\r\none\r\n3\r\ntwo\r\n0\r\n\r\n"),
            (
                b"Transfer-Encoding: chunked\r\nTrailer: X-End\r\n",
                b"3\r\none\r\n3\r\ntwo\r\n0\r\nX-End: done\r\n\r\n",
            ),
        ):
            with self.subTest(framing=framing), Wire.connect(self.server.address) as wire:
                head = (
                    b"POST /__peer/echo HTTP/1.1\r\nHost: localhost\r\n"
                    b"Expect: 100-continue\r\nConnection: close\r\n" + framing + b"\r\n"
                )
                wire.send(head, chunk_size=1)
                self.assertEqual(wire.response().status, 100)
                wire.send(body, chunk_size=1)
                self.assertEqual(wire.response().body, b"onetwo")
                wire.expect_eof()

    def test_go_early_final_without_upload(self):
        with Wire.connect(self.server.address) as wire:
            wire.send(
                b"POST /__peer/reject HTTP/1.1\r\nHost: localhost\r\n"
                b"Expect: 100-continue\r\nContent-Length: 999999\r\n\r\n"
            )
            interim, response = wire.final_response()
            self.assertEqual(interim, [])
            self.assertEqual(response.status, 417)
            wire.expect_eof()

    def test_go_client_against_scripted_chunked_peer(self):
        requests = []

        def serve(wire, _index):
            for index in range(2):
                head = wire.until(b"\r\n\r\n", 65536)
                requests.append(head)
                self.assertTrue(head.startswith(b"GET /probe HTTP/1.1\r\n"))
                closing = b"Connection: close\r\n" if index else b""
                wire.send(
                    b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n"
                    + closing
                    + b"\r\n3\r\none\r\n3\r\ntwo\r\n0\r\n\r\n",
                    chunk_size=1,
                )
            wire.expect_eof()

        with ScriptedPeer(serve) as peer:
            result = run_command(
                [
                    str(self.binary),
                    "--mode",
                    "client",
                    "--url",
                    f"http://{peer.address[0]}:{peer.address[1]}/probe",
                    "--count",
                    "2",
                ],
                cwd=WORKSPACE,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            rows = [json.loads(line) for line in result.stdout.splitlines()]
            self.assertEqual([row["index"] for row in rows], [0, 1])
            self.assertEqual([row["status"] for row in rows], [200, 200])
            self.assertEqual([base64.b64decode(row["body_base64"]) for row in rows], [b"onetwo"] * 2)
        self.assertEqual(len(requests), 2)


if __name__ == "__main__":
    unittest.main()
