//! Decodes the http2jp/hpack-test-case interop corpus, on demand.
//!
//! Fourteen independent encoders encode identical header sequences using
//! different strategies — Huffman versus literal, indexed versus incremental
//! versus never-indexed, in-band dynamic table size updates, and a non-default
//! 16,384-byte table. Decoding all of them checks our decoder against choices
//! our own encoder never makes.
//!
//! The corpus is not committed. Measured against injected decoder faults it
//! uniquely caught only static table entry corruption, which
//! `hpack_static_table.rs` now covers outright at no storage cost, so carrying
//! 386 KiB of vendored hex earned nothing the cheaper test does not. It stays
//! runnable for one-off validation after decoder changes:
//!
//! ```text
//! git clone https://github.com/http2jp/hpack-test-case.git /tmp/hpack-tc
//! python3 scripts/vendor-hpack-interop.py /tmp/hpack-tc > /tmp/corpus.txt
//! KIMOJIO_HPACK_INTEROP_CORPUS_V1=/tmp/corpus.txt \
//!     cargo test -p kimojio-fsm-http --all-features --test hpack_interop
//! ```
//!
//! Without the variable the test reports that it was skipped and passes.
//!
//! Cases within a story are sequential — the dynamic table carries across them —
//! so each encoder's stories must be decoded in order with a single decoder.

use kimojio_fsm_http::{H2HeaderBlockDecoder, H2RawHeader};

const INTEROP_CORPUS_ENV: &str = "KIMOJIO_HPACK_INTEROP_CORPUS_V1";

/// RFC 7541's default dynamic table capacity, used when a case omits the size.
const DEFAULT_TABLE_SIZE: usize = 4096;

struct Case<'a> {
    headers: Vec<H2RawHeader>,
    wires: Vec<Wire<'a>>,
}

struct Wire<'a> {
    encoder: &'a str,
    table_size: usize,
    hex: &'a str,
}

struct Story<'a> {
    name: &'a str,
    cases: Vec<Case<'a>>,
}

fn decode_hex(input: &str) -> Vec<u8> {
    let input = input.trim().as_bytes();
    assert!(input.len().is_multiple_of(2), "odd-length hex");
    input
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).unwrap();
            u8::from_str_radix(text, 16).unwrap()
        })
        .collect()
}

fn parse_corpus(text: &str) -> Vec<Story<'_>> {
    let mut stories: Vec<Story<'_>> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |reason: &str| -> ! { panic!("corpus line {}: {reason}", number + 1) };
        let (tag, rest) = match line.split_once(' ') {
            Some((tag, rest)) => (tag, rest),
            None => (line, ""),
        };
        match tag {
            "S" => stories.push(Story {
                name: rest,
                cases: Vec::new(),
            }),
            "C" => match stories.last_mut() {
                Some(story) => story.cases.push(Case {
                    headers: Vec::new(),
                    wires: Vec::new(),
                }),
                None => fail("case before any story"),
            },
            "H" => {
                let Some((name, value)) = rest.split_once('\t') else {
                    fail("header without a tab separator")
                };
                match stories.last_mut().and_then(|story| story.cases.last_mut()) {
                    Some(case) => case.headers.push(H2RawHeader::new(name, value)),
                    None => fail("header before any case"),
                }
            }
            "W" => {
                let mut parts = rest.splitn(3, ' ');
                let (Some(encoder), Some(size), Some(hex)) =
                    (parts.next(), parts.next(), parts.next())
                else {
                    fail("wire entry needs an encoder, a table size, and hex")
                };
                let table_size = if size == "-" {
                    DEFAULT_TABLE_SIZE
                } else {
                    size.parse()
                        .unwrap_or_else(|_| fail("unparsable table size"))
                };
                match stories.last_mut().and_then(|story| story.cases.last_mut()) {
                    Some(case) => case.wires.push(Wire {
                        encoder,
                        table_size,
                        hex,
                    }),
                    None => fail("wire before any case"),
                }
            }
            _ => fail("unrecognized record"),
        }
    }
    stories
}

fn encoders<'a>(stories: &[Story<'a>]) -> Vec<&'a str> {
    let mut names: Vec<&str> = stories
        .iter()
        .flat_map(|story| story.cases.iter())
        .flat_map(|case| case.wires.iter())
        .map(|wire| wire.encoder)
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

#[test]
fn every_encoder_in_the_corpus_decodes_to_the_expected_headers() {
    let Ok(path) = std::env::var(INTEROP_CORPUS_ENV) else {
        eprintln!("skipped: {INTEROP_CORPUS_ENV} is not set, see this file's documentation");
        return;
    };
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read the corpus at {path}: {error}"));
    let stories = parse_corpus(&text);
    let names = encoders(&stories);
    assert!(!names.is_empty(), "corpus contains no encoders");

    let mut decoded_blocks = 0usize;
    for encoder in names {
        for story in &stories {
            let mut decoder = H2HeaderBlockDecoder::new();
            let mut configured = DEFAULT_TABLE_SIZE;
            for (index, case) in story.cases.iter().enumerate() {
                let Some(wire) = case.wires.iter().find(|wire| wire.encoder == encoder) else {
                    continue;
                };
                if wire.table_size != configured {
                    decoder.set_max_table_size(wire.table_size);
                    configured = wire.table_size;
                }
                let headers = decoder
                    .decode_with_limit(&decode_hex(wire.hex), usize::MAX)
                    .unwrap_or_else(|error| {
                        panic!("{encoder} {} case {index}: {error:?}", story.name)
                    });
                assert_eq!(
                    headers, case.headers,
                    "{encoder} {} case {index} decoded to the wrong headers",
                    story.name
                );
                decoded_blocks += 1;
            }
        }
    }
    eprintln!("decoded {decoded_blocks} header blocks from {path}");
}
