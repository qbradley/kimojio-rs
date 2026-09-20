"""python-h2 socket fixture for harness qualification, not native-core evidence."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import socket

from h2.events import DataReceived, RequestReceived, StreamEnded, StreamReset

from peer import CreditSender
from socket_peer import Channel, configured_peer, credit_report, prerequisites, receive_credit_report, require


def client(spec):
    require(spec.get("schema") == 1, "request schema must be 1")
    require(all(action["action"] == "pause" for action in spec["actions"]), "unsupported reference action")
    paused = {action["stream_id"]: action["until_stream_ended"] for action in spec["actions"]}
    require(spec["request_count"] == len(spec["requests"]), "request count mismatch")
    require(0 < spec["request_count"] <= 1024, "invalid request count")
    require(0 < spec["timeout_ms"] <= 60000, "invalid timeout")
    require(0 < spec["concurrency"] <= 128, "invalid concurrency")
    require(all(0 <= req["body_bytes"] <= 32 * 1024 * 1024 for req in spec["requests"]), "invalid upload size")
    peer = configured_peer(client=True, config=spec["config"])
    sock = socket.create_connection((spec["host"], spec["port"]), timeout=5)
    channel = Channel(sock, peer, timeout=spec["timeout_ms"] / 1000, consume=not paused)
    sender = CreditSender(peer)
    issued = 0
    try:
        channel.handshake()
        baseline = channel.inbound.updates[0]
        receive_baseline = channel.outbound.updates[0]
        receive_initial = peer.connection.inbound_flow_control_window
        for _ in range(1_000_000):
            ended = sum(result["ended"] for result in channel.results.values())
            while issued < len(spec["requests"]) and issued - ended < min(
                spec["concurrency"], peer.connection.remote_settings.max_concurrent_streams,
            ):
                if any(
                    stream < issued * 2 + 1
                    and (stream not in peer.received or peer.received[stream].payload == 0)
                    for stream in paused
                ):
                    break
                request = spec["requests"][issued]
                stream = issued * 2 + 1
                headers = [
                    (b":method", request["method"].encode()),
                    (b":scheme", b"http"), (b":authority", b"localhost"),
                    (b":path", request["path"].encode()),
                ]
                length = request["body_bytes"]
                if length:
                    headers.append((b"content-length", str(length).encode()))
                peer.connection.send_headers(
                    stream, headers, end_stream=length == 0 and not request["trailers"],
                )
                sender.start(stream, length, trailers=bool(request["trailers"]))
                issued += 1
            blocked = {
                send.stream_id for send in sender.pending
                if spec["requests"][(send.stream_id - 1) // 2]["path"] == "/echo"
                and send.sent > 0
                and (
                    send.stream_id not in peer.received
                    or peer.received[send.stream_id].payload == 0
                )
            }
            sender.drive(frame_budget=8, blocked_streams=blocked)
            if ended == len(spec["requests"]) and not sender.pending:
                channel.flush()
                break
            events = channel.step()
            if spec["actions"]:
                for event in events:
                    if isinstance(event, DataReceived) and event.stream_id not in paused:
                        peer.consume(event.stream_id, event.flow_controlled_length)
                for stream, sibling in list(paused.items()):
                    if channel.results.get(sibling, {}).get("ended"):
                        require(stream in peer.received, "paused stream received no body")
                        require(not peer.received[stream].ended, "paused stream ended before its sibling")
                        if stream in peer.received:
                            peer.consume(stream)
                        del paused[stream]
            require(not channel.eof, "server closed before all streams ended")
            require(not peer.resets and peer.termination is None, "unexpected reset or GOAWAY")
        else:
            raise AssertionError("client turn limit exceeded")
        credit = credit_report(channel, sender, baseline)
        receive_credit = receive_credit_report(channel, receive_initial, receive_baseline)
    finally:
        channel.close()
    report = channel.report()
    report["credit"] = credit
    report["receive_credit"] = receive_credit
    return report


def serve(sock, *, config, timeout=60, case=None, refund=True, peer=None):
    peer = peer if peer is not None else configured_peer(client=False, config=config)
    channel = Channel(sock, peer, timeout=timeout, consume=False)
    sender = CreditSender(peer)
    requests = {}
    responded = set()
    sibling_progress = False
    receive_initial = peer.connection.inbound_flow_control_window
    receive_baseline = receive_initial - 65535
    try:
        channel.handshake()
        baseline = channel.inbound.updates[0]
        for _ in range(1_000_000):
            sender.drive(frame_budget=8)
            channel.queue()
            if sender.releases:
                fragments, sender.releases = sender.releases, []

                def settled(fragments=fragments):
                    for fragment in fragments:
                        sender.settle(fragment, consume=refund)
                    fragments.clear()

                channel.after_write(settled)
            if case and case.actions and 3 in sender.completed and not sibling_progress:
                pending = {send.stream_id: send for send in sender.pending}
                if 1 not in sender.cancelled:
                    require(1 in pending, "paused stream completed before its sibling")
                    require(pending[1].sent == 1024, "paused consumer returned premature stream credit")
                sibling_progress = True
            events = channel.step()
            for event in events:
                if isinstance(event, StreamReset):
                    require(
                        case is not None and case.name == "empty-data-sibling"
                        and event.stream_id == 1 and int(event.error_code) == 11,
                        "unexpected stream reset",
                    )
                    sender.cancel(1)
                if isinstance(event, RequestReceived):
                    require(len(requests) < 1024, "request count limit exceeded")
                    requests[event.stream_id] = dict(event.headers)
                if isinstance(event, RequestReceived):
                    stream = event.stream_id
                    if stream in responded:
                        continue
                    headers = requests[stream]
                    path = headers[b":path"].decode()
                    if path == "/echo":
                        declared = headers.get(b"content-length")
                        sender.echo_response(stream, int(declared) if declared is not None else None)
                        responded.add(stream)
                        continue
                    elif path == "/no-content":
                        peer.connection.send_headers(stream, [(b":status", b"204")], end_stream=True)
                        sender.start(stream, 0)
                        responded.add(stream)
                        continue
                    else:
                        length = int(path.rsplit("/", 1)[1])
                    require(0 <= length <= 32 * 1024 * 1024, "body size limit exceeded")
                    if headers[b":method"] == b"HEAD":
                        peer.connection.send_headers(
                            stream, [(b":status", b"200"), (b"content-length", str(length).encode())],
                            end_stream=True,
                        )
                        sender.start(stream, 0)
                    else:
                        sender.response(
                            stream, length,
                            padding=case.padding if case else None,
                            trailers=path.startswith("/trailers/"),
                            informational=path.startswith("/informational/"),
                        )
                        if case and case.name == "empty-data-sibling" and stream == 1:
                            for _ in range(4096):
                                peer.connection.send_data(stream, b"")
                    responded.add(stream)
                if isinstance(event, DataReceived):
                    if event.stream_id in sender.streaming:
                        require(
                            event.data == bytes([event.stream_id % 251]) * len(event.data),
                            "upload fragment mismatch",
                        )
                        sender.feed(event.stream_id, event.data, event.flow_controlled_length)
                    elif refund:
                        peer.consume(event.stream_id, event.flow_controlled_length)
                if isinstance(event, StreamEnded) and event.stream_id in sender.streaming:
                    sender.end(event.stream_id)
            # The receive batch must not retain fragments after write settlement.
            events.clear()
            event = None
            if channel.eof:
                require(not sender.pending, "client closed with incomplete responses")
                require(sender.owned_items == 0, "client closed before echo writes settled")
                require(len(responded) == len(requests), "client abandoned request")
                break
        else:
            raise AssertionError("server turn limit exceeded")
        credit = credit_report(channel, sender, baseline)
        receive_credit = receive_credit_report(channel, receive_initial, receive_baseline)
        require(
            not peer.resets or case is not None and case.name == "empty-data-sibling" and peer.resets == {1: 11},
            "unexpected stream reset",
        )
        require(
            peer.termination is None or peer.termination.error_code == 0,
            "unexpected connection error",
        )
        if case and case.actions:
            require(sibling_progress, "sibling did not finish while stream 1 was paused")
        return {
            "credit": credit, "receive_credit": receive_credit, "requests": len(requests),
            "peer_eof": channel.eof,
            "sibling_progress": sibling_progress,
            "empty_data_frames": channel.outbound.types[0] - sum(
                send.frames for send in (*sender.completed.values(), *sender.cancelled.values())
            ),
            "resets": peer.resets,
            "echo_retention": {
                "bytes": sender.peak_owned_bytes, "items": sender.peak_owned_items,
                "byte_limit": sender.storage_limit, "item_limit": 4096,
            },
            "received": {
                str(stream): {"bytes": received.payload, "sha256": received.digest.hexdigest(), "ended": received.ended}
                for stream, received in peer.received.items()
            },
        }
    finally:
        channel.close()


def main():
    prerequisites()
    parser = argparse.ArgumentParser()
    parser.add_argument("role", choices=("client", "server"))
    parser.add_argument("request_file", type=Path)
    parser.add_argument("result_file", type=Path, nargs="?")
    args = parser.parse_args()
    require(args.request_file.stat().st_size <= 2 * 1024 * 1024, "request JSON exceeds limit")
    spec = json.loads(args.request_file.read_text())
    if args.role == "client":
        require(args.result_file is not None, "client requires result file")
        args.result_file.write_text(json.dumps(client(spec)) + "\n")
    else:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(1)
            listener.settimeout(spec["timeout_ms"] / 1000)
            print(f"LISTEN 127.0.0.1:{listener.getsockname()[1]}", flush=True)
            sock, _ = listener.accept()
            with sock:
                serve(sock, config=spec["config"], timeout=spec["timeout_ms"] / 1000)


if __name__ == "__main__":
    main()
