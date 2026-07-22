# Fuzz targets

Coverage-guided fuzz tests for the protocol decoders of Kimojio. This crate is
outside of the parent workspace. Therefore the sanitizer flags and the nightly
flags that the fuzz profile needs do not change a usual build of the runtime.

```sh
cd fuzz
cargo +nightly fuzz build                     # build all targets
cargo +nightly fuzz run <target>              # run one target until interrupted
cargo +nightly fuzz run <target> -- -max_total_time=900 -workers=6 -jobs=6
```

The fuzzer writes each failure case to `fuzz/artifacts/<target>/`. To run a
failure case again, use
`cargo +nightly fuzz run <target> fuzz/artifacts/<target>/<case>`.

Add a target together with the code that it tests. Each target documents the
interface that it tests and the meaning of a failure in that target.

## Targets

| Target | Interface | Meaning of a failure |
| --- | --- | --- |
| `hpack_decode` | `H2HeaderBlockDecoder` over a sequence of blocks | The dynamic table keeps state between the blocks. An index from one block is applied to a table that a previous block resized or removed entries from. |
| `hpack_round_trip` | `H2HeaderBlockEncoder` into `H2HeaderBlockDecoder` | The encoder made a block that its own decoder rejects, or the encoded form of a malformed name or value has different bytes than the input. |
| `h2_server_accept` | `H2Server::accept_event_ref` | This is the widest target. One call reaches the frame parser, HPACK, the settings negotiation, the flow-control counters, and the stream state machine. |
| `http1_connection` | `Http1ConnectionDecoder::next_event` | A head, a chunk size, or a trailer block causes a different framing decision when it arrives in more than one read than when it arrives complete. |
| `http1_chunked` | `Http1ChunkedBody::next_event` | A peer reads a chunk size differently, or the decoder reports more consumed bytes than it received. Each of these conditions moves the buffer offset of the caller to a position outside the message. |
| `varlink_reply` | The frame format of a systemd-resolved Varlink reply and the generated `ResolveHostnameReply` decoder | A malformed reply from the service passes the checks of the NUL terminator, the UTF-8 encoding, the size, the depth, the field types, the address length, or the address family. A reply can also reach a path that panics in the decoder or in the tokenizer below it. |
| `json_tokenizer` | `kimojio_json::Tokenizer` over arbitrary bytes | The tokenizer panics, moves its cursor outside the input, supplies a slice that does not borrow from the input, reports an escaped string that does not fit in a buffer of its own raw length, or gives a different result on a second pass over accepted bytes. |

## Design notes

Keep these two properties in a new or a changed target.

The fuzzer supplies the input in **slices of a length that the fuzzer selects**,
and not as one buffer. For an incremental decoder, the positions of the splits
are as much a part of the input as the bytes are. A defect in the continuation
of a decode is the desynchronization that causes a decoder to read one message
as two messages.

`h2_server_accept` **puts the client preface before the input.** Without the
preface, the fuzzer uses its time to find a fixed string of 24 bytes and almost
never reaches the frame handling.

Note the limit of `hpack_round_trip`. The encoder and the decoder use the same
static table and the same Huffman table. A corrupted shared table therefore
makes the two sides agree on the same incorrect result.
`kimojio-fsm-http/tests/hpack_static_table.rs` tests those tables independently
for this reason.

## Corpora and artifacts

`fuzz/corpus/` and `fuzz/artifacts/` are ignored files. A corpus is inexpensive
to generate again and expensive to review, thus the repository does not contain
one. A failure case becomes a regression test with the code that it found, for
example in `kimojio-fsm-http/tests/`.
