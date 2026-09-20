import unittest

from h2.settings import SettingCodes
from hyperframe.frame import DataFrame, SettingsFrame

from peer import CreditSender, Peer
from socket_peer import PREFACE
from wrapper_startup_probe import RecordingSocket, trailer_values


class WrapperStartupTests(unittest.TestCase):
    def test_recorder_separates_inflight_data_from_settings_ack(self):
        recorder = RecordingSocket(None)
        data = DataFrame(1, data=b"x" * 2048).serialize()
        ack = SettingsFrame(flags=["ACK"]).serialize()
        recorder.feed("in", PREFACE[:7])
        recorder.feed("in", PREFACE[7:] + data[:12])
        recorder.feed("in", data[12:] + ack + data)
        self.assertEqual(recorder.before_ack[1], 2048)
        self.assertEqual(recorder.flow[1], 4096)
        self.assertEqual(recorder.settings_acks, 1)
        self.assertEqual(len(recorder.events), 3)

    def test_public_settings_update_accepts_inflight_credit_and_tracks_debt(self):
        client, server = Peer(client=True), Peer(client=False)
        server.connection.update_settings({SettingCodes.INITIAL_WINDOW_SIZE: 1024})
        client.connection.send_headers(1, [
            (b":method", b"POST"), (b":scheme", b"http"),
            (b":authority", b"localhost"), (b":path", b"/echo"),
        ], end_stream=False)
        sender = CreditSender(client)
        sender.start(1, 131087)
        for _ in range(4):
            sender.drive(frame_budget=8)
        server.receive(client.take_wire(), consume=False)
        self.assertEqual(server.received[1].payload, 65535)
        client.receive(server.take_wire(), consume=False)
        server.receive(client.take_wire(), consume=False)
        self.assertEqual(client.connection.local_flow_control_window(1), 1024 - 65535)
        sender.drive(frame_budget=8)
        self.assertEqual(client.take_wire(), b"")
        server.consume(1)
        client.receive(server.take_wire(), consume=False)
        self.assertEqual(client.connection.local_flow_control_window(1), 1024)
        sender.drive(frame_budget=8)
        server.receive(client.take_wire(), consume=False)
        self.assertEqual(server.received[1].payload, 65535 + 1024)

    def test_trailers_preserve_same_name_values_not_cross_name_positions(self):
        original = [("x-list", "one"), ("x-other", "value"), ("x-list", "two")]
        regrouped = [("x-other", "value"), ("X-List", "one"), ("x-list", "two")]
        self.assertEqual(trailer_values(original), trailer_values(regrouped))
        self.assertNotEqual(trailer_values(original), trailer_values(regrouped[::-1]))
        self.assertNotEqual(trailer_values(original), trailer_values(regrouped[:-1]))


if __name__ == "__main__":
    unittest.main()
