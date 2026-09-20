"""Test the independent peer itself, including a no-refund negative control."""

import unittest

from h2.events import InformationalResponseReceived, TrailersReceived

from peer import CreditSender, Peer, digest_for, exchange, handshake


class CreditTests(unittest.TestCase):
    def pair(self, window=65535):
        client, server = Peer(client=True, stream_window=window), Peer(client=False)
        handshake(client, server)
        return client, server

    def finish(self, client, server, sender):
        for _ in range(10000):
            progress = sender.drive()
            exchanged = exchange(client, server)
            if not sender.pending:
                return
            self.assertTrue(progress or exchanged, "transfer stalled before completion")
        self.fail("transfer exceeded bounded turn count")

    def test_no_refund_stalls_at_advertised_stream_window(self):
        client, server = self.pair(1024)
        client.request(1)
        exchange(client, server)
        sender = CreditSender(server)
        sender.response(1, 131091)
        self.assertEqual(sender.initial_stream_credit[1], 1024)
        self.assertEqual(sender.drive(), 1)
        exchange(client, server, consume=False)
        self.assertEqual(sender.drive(), 0)
        self.assertEqual(client.received[1].payload, 1024)
        self.assertFalse(client.received[1].ended)
        self.assertNotIn(1, server.window_updates)
        client.consume(1)
        exchange(client, server)
        self.finish(client, server, sender)
        self.assertEqual(client.received[1].payload, 131091)
        self.assertTrue(client.received[1].ended)
        self.assertGreater(sender.total_flow, 2 * sender.initial_connection_credit)
        self.assertGreater(server.window_updates[0], 0)
        self.assertEqual(client.received[1].digest.hexdigest(), digest_for(1, 131091))

    def test_no_refund_stalls_at_connection_window_across_small_streams(self):
        client, server = self.pair()
        sender = CreditSender(server)
        for stream_id in range(1, 17, 2):
            client.request(stream_id)
            exchange(client, server)
            sender.response(stream_id, 16384)
        while sender.drive():
            exchange(client, server, consume=False)
        self.assertEqual(sender.total_flow, sender.initial_connection_credit)
        self.assertEqual(sender.total_flow, 65535)
        self.assertTrue(sender.pending)
        for stream_id in client.received:
            client.consume(stream_id)
        exchange(client, server)
        self.finish(client, server, sender)
        self.assertEqual(sender.total_payload, 8 * 16384)
        self.assertGreater(server.window_updates[0], 0)

    def test_padding_is_refunded_but_not_delivered_as_body(self):
        client, server = self.pair(1024)
        client.request(1)
        exchange(client, server)
        sender = CreditSender(server)
        sender.response(1, 200003, padding=17)
        self.finish(client, server, sender)
        sent = sender.completed[1]
        received = client.received[1]
        self.assertEqual(received.payload, 200003)
        self.assertEqual(received.flow, sent.flow_sent)
        self.assertEqual(received.flow - received.payload, sent.frames * 18)
        self.assertEqual(received.unconsumed, 0)
        self.assertEqual(received.digest.hexdigest(), digest_for(1, 200003))
        self.assertGreater(server.window_updates[0], 0)
        self.assertGreater(server.window_updates[1], 0)

    def test_slow_stream_does_not_stop_sibling(self):
        client, server = self.pair(1024)
        for stream_id in (1, 3):
            client.request(stream_id)
        exchange(client, server)
        sender = CreditSender(server)
        sender.response(1, 4096)
        sender.response(3, 4096)
        for _ in range(16):
            sender.drive()
            exchange(client, server, consume=False)
            if 3 in client.received:
                client.consume(3)
            exchange(client, server, consume=False)
            if 3 in sender.completed:
                break
        self.assertIn(3, sender.completed)
        self.assertNotIn(1, sender.completed)
        self.assertEqual(client.received[1].payload, 1024)
        self.assertEqual(client.received[3].payload, 4096)
        client.consume(1)
        exchange(client, server)
        self.finish(client, server, sender)

    def test_informational_trailers_and_frame_budget(self):
        client, server = self.pair()
        for stream_id in (1, 3):
            client.request(stream_id)
        exchange(client, server)
        sender = CreditSender(server)
        sender.response(1, 17, trailers=True, informational=True)
        sender.response(3, 17)
        self.assertEqual(sender.drive(frame_budget=1), 1)
        self.assertEqual(len(sender.pending), 1)
        events = client.receive(server.take_wire())
        self.assertEqual(
            [event.stream_id for event in events if isinstance(event, InformationalResponseReceived)],
            [1],
        )
        self.assertEqual(
            [event.headers for event in events if isinstance(event, TrailersReceived)],
            [[(b"x-end", b"done")]],
        )
        self.finish(client, server, sender)

    def test_empty_body_and_invalid_consumption(self):
        client, server = self.pair()
        client.request(1)
        exchange(client, server)
        sender = CreditSender(server)
        sender.response(1, 0, trailers=True)
        exchange(client, server)
        self.assertTrue(client.received[1].ended)
        self.assertEqual(client.received[1].flow, 0)
        with self.assertRaises(ValueError):
            client.consume(1, 1)
        with self.assertRaises(ValueError):
            sender.drive(0)


if __name__ == "__main__":
    unittest.main()
