# HPACK and Header Representation

`kimojio-fsm-http` owns the production HPACK implementation used by the FSM
and stack HTTP/2 paths. It preserves ordered header occurrences as bytes,
keeps sensitivity attached to each occurrence, and maintains one inbound and
one outbound compression history per physical connection. The third-party
`hpack` crate is a test-only secondary oracle, not a production dependency.

## Codec and representation

The decoder accepts every RFC 7541 representation: indexed fields,
incremental literals, literals without indexing, never-indexed literals, and
leading dynamic-table size updates. Dynamic entries are newest first, account
for `name length + value length + 32`, and evict oldest entries to remain under
the effective capacity. The local capacity ceiling is 1,048,576 octets; setting
a capacity does not allocate storage proportional to that capacity.

The encoder applies this policy to each occurrence:

1. Sensitive occurrences use never-indexed literals and are never inserted.
2. Ordinary exact static or newest dynamic matches use indexed forms.
3. Name-only literals prefer the newest dynamic name, then the static table.
4. An ordinary miss that fits is inserted; an oversized miss is not indexed.
5. A name or value uses Huffman encoding only when it is strictly shorter.

`H2HeaderField` is the canonical owned carrier. Its name and value are byte
vectors, and its `sensitive` bit belongs to that occurrence. `H2RawHeaderRef`
is the borrowed sensitivity-bearing carrier. `H2RawHeader` remains the
source-compatible nonsensitive pair; call `with_sensitive` to convert it to a
canonical field. The compatibility decoder rejects a received sensitive
occurrence rather than returning a lossy `H2RawHeader`.

Received never-indexed literals produce sensitive canonical fields. Cloning or
adapting those fields must retain the bit so re-encoding remains never indexed.
Order and duplicate occurrences are significant and are not reconstructed from
a map.

## Connection ownership and wire order

An `H2Server`, `H2Client`, or stack HTTP/2 `ConnectionState` owns exactly one
decoder for peer-to-local traffic and one encoder for local-to-peer traffic.
Histories, settings changes, terminal state, and diagnostics are directional.
Standalone `H2HeaderBlockEncoder` and `H2HeaderBlockDecoder` values are also
stateful and must be reused for one direction in wire order.

Layered adapters that own the codec pair construct the FSM with
`for_external_hpack_adapter` and deliver decoded fields through
`accept_external_header_fields`. This prevents a second active history for the
same connection.

Outbound FSM helpers return an `H2OutboundCommit`. The owner reads the FIFO
front from `next_outbound_block`, verifies that its receipt matches, assigns
those bytes to one ordered transport queue, and then calls
`acknowledge_outbound_block`. A reversible `prepare_outbound_header_block`
transaction changes no compression state until `commit`. Pre-handoff failures
roll back the encoder; failure after handoff is terminal because peer-visible
history cannot be reordered.

### Checked outbound handoff

More than one complete transaction may be queued. Always compare the FIFO
front's receipt with the helper's receipt **before** assigning bytes:

```rust
use kimojio_fsm_http::{H2OutboundCommit, H2Server, ServerError};

fn handoff(
    server: &mut H2Server,
    commit: H2OutboundCommit,
    assign: impl FnOnce(&[u8]) -> Result<(), ServerError>,
) -> Result<(), ServerError> {
    let block = server
        .next_outbound_block()
        .ok_or(ServerError::InvalidFrame)?;
    if block.commit() != commit {
        return Err(ServerError::InvalidFrame);
    }
    assign(block.bytes())?;
    server.acknowledge_outbound_block(commit)
}
```

A mismatch leaves the FIFO untouched; drain or hand off the older receipt
first. An assignment error must not acknowledge the block. Once assignment
succeeds, acknowledge that exact receipt before handing off the next block.

## Byte and text APIs

`H2ByteStreamEvent` and `H2ByteClientEvent` expose request, response, and trailer
fields as canonical `H2HeaderField` occurrences. Their borrowed variants avoid
copying DATA payloads while retaining owned header occurrences. The borrowed
`project_h2_header_fields` and `project_h2_header_fields_for_role` functions are
atomic: non-UTF-8 names or values and malformed pseudo-fields return
`H2HeaderProjectionError` while the caller still owns the unchanged canonical
section.

By contrast, event `try_into_text` consumes `self` and its error does not return
the canonical event. Borrow the event's fields for projection, or clone the
event before `try_into_text`, when canonical forwarding authority must survive
a failed text inspection. Pseudo-fields become typed request or response roles
and cannot later be forwarded as the original regular-field occurrence.
Forwarding code must keep the canonical fields instead of forwarding a
projected text view.

## Limits and errors

`H2Limits` separates encoded header-block bytes, decoded field-section size,
and dynamic-table capacity. Defaults are 64 KiB for both encoded and decoded
header limits and 4,096 octets for the table; the table is always capped by the
1 MiB local ceiling. Decoded size uses `name length + value length + 32` per
occurrence. Equality is accepted and the first octet over a limit fails.

`H2HpackError` provides stable, content-free categories for invalid indexes,
integers, strings, table updates, Huffman input, decoded and encoded limits,
accounting/state overflow, poisoned state, and allocation failure.
Compression-syntax failure poisons the inbound history and later calls return
`DecoderPoisoned` without inspecting input. A local decoded-list limit still
finishes compression processing so table state remains synchronized; if a
later syntax failure exists, that compression error takes precedence.

No field is delivered until the complete block has passed HPACK processing and
HTTP field validation. HTTP/2 values reject NUL, CR, LF, and leading or
trailing SP/HTAB. Encoded assembly overflow and allocation failure are terminal
resource outcomes rather than peer syntax errors. Encoder failure before
handoff returns no partial output and restores table, pending update,
diagnostic, and output state.

Stack `ConnectionState::new_with_encoded_header_block_limit` configures the
encoded ceiling independently from `Settings::max_header_list_size`. Runtime
stack connections use `HttpConfig::max_header_bytes` for encoded assembly and
the local HTTP/2 setting for decoded field-section accounting.

## Diagnostics

`H2HpackDiagnosticsSnapshot` exposes saturating, content-free lifetime counters
for blocks, wire and field octets, representation classes, Huffman and plain
strings, table updates, insertions and evictions, compression errors, and local
limit failures. It never contains names or values.

`effectiveness()` returns the exact reduced `encoded wire octets /
uncompressed field octets` fraction. It returns `None` when no field octets
exist or either required counter saturated. Inspect inbound and outbound
snapshots separately with the connection's
`inbound_hpack_diagnostics` and `outbound_hpack_diagnostics` methods.

## HTTP and gRPC adapters

Stack HTTP exposes canonical `h2::HeaderField` values. The
`request_from_header_fields`, `request_header_fields`,
`response_from_header_fields`, and `response_header_fields` helpers preserve
the canonical section. `Trailers::{from_h2_fields,as_h2_fields,into_h2_fields}`
does the same for trailers. `HeaderMap` is a convenience projection and cannot
preserve global occurrence order; a changed or lossy projection is rejected
for forwarding. Snapshot checks compare `HeaderValue::is_sensitive` explicitly
because ordinary `HeaderMap` equality does not.

FSM and stack gRPC `Metadata` retain canonical field occurrences. Use
`as_h2_fields` or `into_h2_fields` for forwarding and `append` or `append_bin`
for duplicate metadata. The `try_as_http_headers` and
`try_into_http_headers` methods are fallible inspection projections. Status
trailer conversion likewise uses canonical field APIs, preserving legal bytes,
duplicates, order, and sensitivity while filtering reserved gRPC transport
fields.

## Acceptance and compatibility

HPACK Acceptance Manifest v4 is the sole current deterministic denominator.
Its authoritative runners cover integer, Huffman, representation, table,
resource, limit, connection, diagnostics, adapter-path, and closure rows. Each
row has one owner and one completion, and phase gates enforce reverse coverage
for linked requirements. Final closure completion is accepted only when the
compatibility runner supplies independently observed constituent-runner and
matrix results and the repository document/inventory checks pass. Manifest v1
through v3 remain immutable historical evidence.

Run the exact compatibility matrix with:

```sh
scripts/check-hpack-compatibility.sh
```

The fixed target denominator is
[`scripts/hpack-compatibility-targets.tsv`](../scripts/hpack-compatibility-targets.tsv).
The runner compares all 67 declared targets with Cargo metadata and checks
exactly these packages:

- `kimojio-fsm-http`
- `kimojio-fsm-grpc`
- `kimojio-fsm-static-file-server`
- `kimojio-stack-http`
- `kimojio-stack-grpc`
- `examples`

For every target it reports `check` and executable `test` actions plus
`rustdoc` for documentation-enabled targets, under default and all-feature
configurations: 324 target/action tuples on `x86_64-unknown-linux-gnu`.
The three `examples` binaries whose manifests require `tls` or
`virtual-clock` are explicitly `NOT_APPLICABLE` for their nine default
check/rustdoc/test tuples; all are selected and pass with all features. The
remaining 315 tuples must pass. Missing, duplicate, unexpected, silently
skipped, or incorrectly classified tuples fail the runner.

## SC-006 documentation checklist

Each row maps a released documentation surface to byte handling, sensitivity,
connection ownership, limits/errors, diagnostics, and compatibility.

| Surface | Byte handling | Sensitivity | Ownership | Limits/errors | Diagnostics | Compatibility |
| --- | --- | --- | --- | --- | --- | --- |
| This technical reference | [Byte and text APIs](#byte-and-text-apis) | [Codec and representation](#codec-and-representation) | [Connection ownership and wire order](#connection-ownership-and-wire-order) | [Limits and errors](#limits-and-errors) | [Diagnostics](#diagnostics) | [Acceptance and compatibility](#acceptance-and-compatibility) |
| [FSM HTTP Server Guide](fsm-http-server.md#hpack-headers-limits-and-diagnostics) | Canonical byte events | Per-occurrence never-indexing | FSM connection pair and handoff | Encoded/decoded/table limits and terminal errors | Directional snapshots | Validation commands and migration behavior |
| [Stackful HTTP and gRPC Guide](stack-http-grpc.md#hpack-ownership-limits-and-diagnostics) | Canonical HTTP/gRPC fields | Carrier sensitivity is retained | One stack connection pair | HTTP/gRPC projection and size failures | Stack connection snapshots | Default/all-feature adapter coverage |
| [`kimojio-fsm-http/TODO.md`](../kimojio-fsm-http/TODO.md) inventory | Completion pointer names byte preservation | Completion pointer names never-indexing | Completion pointer names directional ownership | Completion pointer names bounded failures | Completion pointer names content-free counters | Completion pointer links this matrix |
| [README technical-reference link](../README.md#stackful-http-and-grpc) | Links this reference | Links this reference | Links this reference | Links this reference | Links this reference | Stable released index |
| [README FSM-guide link](../README.md#stackful-http-and-grpc) | Links the FSM guide | Links the FSM guide | Links the FSM guide | Links the FSM guide | Links the FSM guide | Stable released index |
| [README stack-guide link](../README.md#stackful-http-and-grpc) | Links the stack guide | Links the stack guide | Links the stack guide | Links the stack guide | Links the stack guide | Stable released index |

## Limitations

Text and map projections are convenience views, not canonical forwarding
formats. Diagnostics deliberately omit header content. External reference-peer
matrices, generalized fuzzing, protocol microbenchmarks, and unrelated HTTP/2
extensions remain separate work.
