"""Deterministic prefix-reader tests for duplex progress assertions."""

import unittest

from duplex_suite import ResponsePrefix
from harness import ProtocolError, Wire
from test_harness import MemorySocket


class DuplexReaderTests(unittest.TestCase):
    def test_chunk_prefixes_cross_frames_at_every_input_split(self):
        raw = (
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n"
            b"2\r\non\r\n4;part=second\r\netwo\r\n0\r\nX-Duplex: done\r\n\r\n"
        )
        for cut in range(len(raw) + 1):
            with self.subTest(cut=cut):
                response = ResponsePrefix(Wire(MemorySocket([raw[:cut], raw[cut:]])))
                self.assertEqual(response.take(3), b"one")
                self.assertEqual(response.take(3), b"two")
                response.finish(((b"x-duplex", b"done"),))

    def test_fixed_and_close_delimited_prefixes(self):
        for head in (
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\n",
            b"HTTP/1.0 200 OK\r\n\r\n",
        ):
            with self.subTest(head=head):
                response = ResponsePrefix(Wire(MemorySocket([head + b"onetwo"])))
                self.assertEqual(response.take(3), b"one")
                self.assertEqual(response.take(3), b"two")
                response.finish()

    def test_truncation_extra_data_and_ambiguous_head_fail(self):
        with self.assertRaises(ProtocolError):
            response = ResponsePrefix(Wire(MemorySocket([
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\none"
            ])))
            response.take(6)
        with self.assertRaises(ProtocolError):
            response = ResponsePrefix(Wire(MemorySocket([
                b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nonetwo!"
            ])))
            response.take(6)
            response.finish()
        with self.assertRaises(ProtocolError):
            ResponsePrefix(Wire(MemorySocket([
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n"
            ])))


if __name__ == "__main__":
    unittest.main()
