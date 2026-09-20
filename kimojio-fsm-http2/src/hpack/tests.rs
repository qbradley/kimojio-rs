use super::*;

#[test]
fn rfc_integer_examples() {
    let mut encoded = Vec::new();
    push_integer(&mut encoded, 10, 5, 0);
    assert_eq!(encoded, [10]);
    encoded.clear();
    push_integer(&mut encoded, 1337, 5, 0);
    assert_eq!(encoded, [31, 154, 10]);
    let mut cursor = 0;
    assert_eq!(decode_integer(&encoded, &mut cursor, 5), Ok(1337));
}

#[test]
fn rfc_huffman_example() {
    let mut encoded = Vec::new();
    encode_huffman(b"www.example.com", &mut encoded);
    assert_eq!(
        encoded,
        [
            0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90, 0xf4, 0xff
        ]
    );
    assert_eq!(
        decode_huffman(&encoded, &mut AllocationGate::default()),
        Ok(b"www.example.com".to_vec())
    );
}

#[test]
fn nibble_huffman_decoder_matches_bitwise_reference() {
    fn decode_with(
        bytes: &[u8],
        walk: impl FnOnce(&[u8], &mut dyn FnMut(u8)) -> Result<usize, Error>,
    ) -> Result<(usize, Vec<u8>), Error> {
        let mut output = Vec::new();
        let decoded_len = walk(bytes, &mut |symbol| output.push(symbol))?;
        assert_eq!(decoded_len, output.len());
        Ok((decoded_len, output))
    }

    let optimized = |bytes: &[u8]| decode_with(bytes, |input, emit| walk_huffman(input, emit));
    let reference =
        |bytes: &[u8]| decode_with(bytes, |input, emit| walk_huffman_reference(input, emit));

    assert_eq!(optimized(&[]), reference(&[]));
    for first in 0..=u8::MAX {
        assert_eq!(optimized(&[first]), reference(&[first]));
    }
    for input in 0..=u16::MAX {
        let bytes = input.to_be_bytes();
        assert_eq!(
            optimized(&bytes),
            reference(&bytes),
            "mismatch for {bytes:02x?}"
        );
    }
}

#[test]
fn nibble_huffman_table_matches_each_bitwise_transition() {
    let tree = huffman_tree();
    for state in 0..tree.len() {
        for input in 0..HUFFMAN_NIBBLE_COUNT {
            let mut node = state;
            let mut symbol = NO_HUFFMAN_SYMBOL;
            let mut valid = true;
            for shift in (0..HUFFMAN_NIBBLE_BITS).rev() {
                let bit = (input >> shift) & 1;
                let Some(child) = tree[node].children[bit] else {
                    valid = false;
                    break;
                };
                node = child;
                if let Some(decoded) = tree[node].symbol {
                    if decoded == HUFFMAN_EOS_SYMBOL || symbol != NO_HUFFMAN_SYMBOL {
                        valid = false;
                        break;
                    }
                    symbol = decoded;
                    node = 0;
                }
            }
            let expected = if valid {
                HuffmanTransition {
                    next: node as u16,
                    symbol,
                }
            } else {
                EMPTY_HUFFMAN_TRANSITION
            };
            assert_eq!(
                HUFFMAN_DECODE_TABLE.transitions[state][input], expected,
                "mismatch for state {state}, nibble {input:#x}"
            );
        }

        let mut eos_prefix = 0;
        let mut valid_padding_state = false;
        for _ in 0..MAX_HUFFMAN_PADDING_BITS {
            eos_prefix = tree[eos_prefix].children[1].unwrap();
            valid_padding_state |= eos_prefix == state;
        }
        assert_eq!(
            HUFFMAN_DECODE_TABLE.valid_padding_state[state], valid_padding_state,
            "padding classification mismatch for state {state}"
        );
    }
}

#[test]
fn empty_huffman_value_decodes_without_poisoning_or_table_mutation() {
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.decode(&[0x11, 0x80], usize::MAX),
        Ok(vec![HeaderField::sensitive(":authority", "")])
    );
    assert_eq!(decoder.table.entries.len(), 0);
    assert_eq!(
        decoder.decode(&[0x82], usize::MAX),
        Ok(vec![HeaderField::new(":method", "GET")])
    );
}

#[test]
fn rfc_request_examples_with_huffman() {
    let mut decoder = Decoder::new();
    let first = [
        0x82, 0x86, 0x84, 0x41, 0x8c, 0xf1, 0xe3, 0xc2, 0xe5, 0xf2, 0x3a, 0x6b, 0xa0, 0xab, 0x90,
        0xf4, 0xff,
    ];
    let fields = decoder.decode(&first, usize::MAX).unwrap();
    assert_eq!(
        fields,
        [
            HeaderField::new(":method", "GET"),
            HeaderField::new(":scheme", "http"),
            HeaderField::new(":path", "/"),
            HeaderField::new(":authority", "www.example.com"),
        ]
    );

    let second = [
        0x82, 0x86, 0x84, 0xbe, 0x58, 0x86, 0xa8, 0xeb, 0x10, 0x64, 0x9c, 0xbf,
    ];
    let fields = decoder.decode(&second, usize::MAX).unwrap();
    assert_eq!(fields[3], HeaderField::new(":authority", "www.example.com"));
    assert_eq!(fields[4], HeaderField::new("cache-control", "no-cache"));
}

#[test]
fn encoder_reuses_dynamic_entries_and_preserves_sensitivity() {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let fields = [
        HeaderField::new("x-repeat", "repeat-value"),
        HeaderField::sensitive("authorization", "secret"),
    ];
    let first = encoder.encode(&fields).unwrap();
    let decoded = decoder.decode(&first, usize::MAX).unwrap();
    assert_eq!(decoded, fields);
    let second = encoder.encode(&fields).unwrap();
    assert!(second.len() < first.len());
    assert_eq!(decoder.decode(&second, usize::MAX).unwrap(), fields);
    assert_eq!(encoder.diagnostics().never_indexed_fields, 2);
}

#[test]
fn encoder_revalidates_exact_indexes_after_mutation_and_pending_resize() {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    encoder.set_max_table_size(80);
    decoder.set_max_allowed_table_size(80);

    let a = HeaderField::new("x-a", "1");
    let b = HeaderField::new("x-b", "2");
    let c = HeaderField::new("x-c", "3");
    let setup = [a.clone(), c];
    let setup_block = encoder.encode(&setup).unwrap();
    assert_eq!(decoder.decode(&setup_block, usize::MAX), Ok(setup.to_vec()));

    // A's first index is safe to cache. Inserting B evicts A, so the
    // second A must be looked up again and emitted as a literal.
    let mixed = [a.clone(), b, a.clone()];
    let mixed_block = encoder.encode(&mixed).unwrap();
    assert_eq!(decoder.decode(&mixed_block, usize::MAX), Ok(mixed.to_vec()));

    // The pending minimum clears both histories before A is encoded.
    // Preflight must not retain the exact index from the old table.
    encoder.set_max_table_size(0);
    encoder.set_max_table_size(80);
    decoder.set_max_allowed_table_size(0);
    decoder.set_max_allowed_table_size(80);
    let resized_block = encoder.encode(std::slice::from_ref(&a)).unwrap();
    assert_eq!(
        resized_block,
        [0x20, 0x3f, 0x31, 0x40, 0x03, b'x', b'-', b'a', 0x01, b'1']
    );
    assert_eq!(
        decoder.decode(&resized_block, usize::MAX),
        Ok(vec![a.clone()])
    );
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[test]
fn table_updates_must_lead_and_obey_limit() {
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(64);
    assert_eq!(decoder.decode(&[0x3f, 0x21], usize::MAX), Ok(Vec::new()));
    assert_eq!(
        decoder.decode(&[0x82, 0x20], usize::MAX),
        Err(Error::TableSizeUpdateAfterField)
    );
    assert_eq!(
        decoder.decode(&[0x3f, 0x62], usize::MAX),
        Err(Error::DecoderPoisoned)
    );

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(64);
    assert_eq!(
        decoder.decode(&[0x3f, 0x62], usize::MAX),
        Err(Error::InvalidMaxDynamicSize)
    );
}

#[test]
fn oversized_incremental_field_reuses_dynamic_name_before_clearing() {
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(64);
    assert_eq!(
        decoder
            .decode(&[0x3f, 0x21, 0x40, 0x01, b'x', 0x01, b'a'], usize::MAX)
            .unwrap(),
        [HeaderField::new("x", "a")]
    );

    let mut oversized = vec![0x7e, 40];
    oversized.extend(std::iter::repeat_n(b'v', 40));
    assert_eq!(
        decoder.decode(&oversized, usize::MAX).unwrap(),
        [HeaderField::new(b"x", vec![b'v'; 40])]
    );
    assert!(decoder.table.snapshot().is_empty());
}

#[test]
fn oversized_lists_advance_compression_state() {
    let mut encoder = Encoder::new();
    let first = encoder
        .encode(&[HeaderField::new("x-dynamic", "a-value")])
        .unwrap();
    let second = encoder
        .encode(&[HeaderField::new("x-dynamic", "a-value")])
        .unwrap();
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.decode(&first, 1),
        Err(Error::HeaderListTooLarge { actual: 48 })
    );
    assert_eq!(
        decoder.decode(&second, usize::MAX).unwrap(),
        [HeaderField::new("x-dynamic", "a-value")]
    );
}

#[test]
fn rejects_eos_and_invalid_padding() {
    assert_eq!(
        decode_huffman(&[0xff, 0xff, 0xff, 0xff], &mut AllocationGate::default()),
        Err(Error::InvalidHuffman)
    );
    assert_eq!(
        decode_huffman(&[0x00], &mut AllocationGate::default()),
        Err(Error::InvalidHuffman)
    );
}

#[test]
fn huffman_round_trips_every_octet() {
    let input = (0..=u8::MAX).collect::<Vec<_>>();
    let mut encoded = Vec::new();
    encode_huffman(&input, &mut encoded);
    assert_eq!(
        decode_huffman(&encoded, &mut AllocationGate::default()),
        Ok(input)
    );
}

#[test]
fn decodes_every_representation_and_preserves_never_indexed() {
    let block = [
        0x20, // table size update to zero
        0x82, // indexed :method GET
        0x40, 0x01, b'a', 0x01, b'b', // incremental
        0x00, 0x01, b'c', 0x01, b'd', // without indexing
        0x10, 0x01, b'e', 0x01, b'f', // never indexed
    ];
    let mut decoder = Decoder::new();
    let fields = decoder.decode(&block, usize::MAX).unwrap();
    assert_eq!(
        fields,
        [
            HeaderField::new(":method", "GET"),
            HeaderField::new("a", "b"),
            HeaderField::new("c", "d"),
            HeaderField::sensitive("e", "f"),
        ]
    );
    let diagnostics = decoder.diagnostics();
    assert_eq!(diagnostics.table_size_updates, 1);
    assert_eq!(diagnostics.indexed_fields, 1);
    assert_eq!(diagnostics.incremental_fields, 1);
    assert_eq!(diagnostics.without_indexing_fields, 1);
    assert_eq!(diagnostics.never_indexed_fields, 1);
}

#[test]
fn huffman_is_selected_only_when_shorter() {
    let mut encoder = Encoder::new();
    let encoded = encoder
        .encode(&[
            HeaderField::sensitive("x", "www.example.com"),
            HeaderField::sensitive("x", "\0"),
        ])
        .unwrap();
    assert_eq!(encoder.diagnostics().huffman_strings, 1);
    assert_eq!(encoder.diagnostics().plain_strings, 3);
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.decode(&encoded, usize::MAX).unwrap(),
        [
            HeaderField::sensitive("x", "www.example.com"),
            HeaderField::sensitive("x", "\0"),
        ]
    );
}

#[test]
fn accepts_non_minimal_and_rejects_overflowing_integers() {
    let mut cursor = 0;
    assert_eq!(decode_integer(&[0x1f, 0x80, 0x00], &mut cursor, 5), Ok(31));
    let mut cursor = 0;
    assert_eq!(
        decode_integer(
            &[
                0x1f, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02
            ],
            &mut cursor,
            5,
        ),
        Err(Error::IntegerOverflow)
    );
    let mut decoder = Decoder::new();
    assert_eq!(
        decoder.decode(
            &[
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f
            ],
            usize::MAX,
        ),
        Err(Error::IntegerOverflow)
    );
}

#[test]
fn accepts_non_minimal_integer_forms_for_every_prefix_width() {
    for prefix_bits in [4, 5, 6, 7] {
        let prefix_max = (1usize << prefix_bits) - 1;
        for offset in 0..12 {
            let expected = prefix_max + offset * 127;
            let mut encoded = Vec::new();
            push_integer(&mut encoded, expected, prefix_bits, 0);
            let final_octet = encoded.last_mut().unwrap();
            *final_octet |= 0x80;
            encoded.push(0);
            let mut cursor = 0;
            assert_eq!(
                decode_integer(&encoded, &mut cursor, prefix_bits),
                Ok(expected)
            );
            assert_eq!(cursor, encoded.len());
        }
    }
}

#[test]
fn integer_zero_continuations_cover_platform_maximum_and_truncations() {
    let maximum_groups = usize::BITS.div_ceil(7) as usize;
    for groups in 1..=maximum_groups {
        let mut encoded = vec![0x1f];
        encoded.extend(std::iter::repeat_n(0x80, groups));
        encoded.push(0);
        let mut cursor = 0;
        assert_eq!(decode_integer(&encoded, &mut cursor, 5), Ok(31));
        assert_eq!(cursor, encoded.len());

        encoded.pop();
        let mut cursor = 0;
        assert_eq!(
            decode_integer(&encoded, &mut cursor, 5),
            Err(Error::TruncatedInteger)
        );
    }

    let mut overflowing = vec![0x1f];
    overflowing.extend(std::iter::repeat_n(0x80, maximum_groups + 1));
    let mut cursor = 0;
    assert_eq!(
        decode_integer(&overflowing, &mut cursor, 5),
        Err(Error::IntegerOverflow)
    );
}

#[test]
fn decoded_limit_accounting_overflow_is_a_local_limit_outcome() {
    let mut total = usize::MAX - 1;
    let mut oversized = false;
    assert!(account_lengths(
        1,
        0,
        usize::MAX,
        &mut total,
        &mut oversized
    ));
    assert!(oversized);
    assert_eq!(total, usize::MAX);
}

#[test]
fn reduced_decoder_limit_requires_a_leading_minimum_update() {
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(0);
    assert_eq!(
        decoder.decode(&[], usize::MAX),
        Err(Error::InvalidTableSizeUpdate)
    );
    assert_eq!(
        decoder.decode(&[0x20], usize::MAX),
        Err(Error::DecoderPoisoned)
    );

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(0);
    assert_eq!(decoder.decode(&[0x20], usize::MAX), Ok(Vec::new()));

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(0);
    decoder.set_max_allowed_table_size(128);
    assert_eq!(
        decoder.decode(&[0x3f, 0x61], usize::MAX),
        Err(Error::InvalidTableSizeUpdate)
    );
    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(0);
    decoder.set_max_allowed_table_size(128);
    assert_eq!(
        decoder.decode(&[0x20, 0x3f, 0x61], usize::MAX),
        Ok(Vec::new())
    );
}

#[test]
fn table_eviction_removes_lookup_indexes() {
    let mut encoder = Encoder::new();
    encoder.set_max_table_size(64);
    let first = encoder
        .encode(&[HeaderField::new("first", "first-value")])
        .unwrap();
    let second = encoder
        .encode(&[HeaderField::new("second", "second-value")])
        .unwrap();
    let third = encoder
        .encode(&[HeaderField::new("first", "first-value")])
        .unwrap();
    assert!(second.len() > 1);
    assert!(third.len() > 1);

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(64);
    assert_eq!(
        decoder.decode(&first, usize::MAX).unwrap(),
        [HeaderField::new("first", "first-value")]
    );
    assert_eq!(
        decoder.decode(&second, usize::MAX).unwrap(),
        [HeaderField::new("second", "second-value")]
    );
    assert_eq!(
        decoder.decode(&third, usize::MAX).unwrap(),
        [HeaderField::new("first", "first-value")]
    );
    assert_eq!(
        encoder.table.snapshot(),
        [(b"first".to_vec(), b"first-value".to_vec())]
    );
    assert_eq!(
        decoder.table.snapshot(),
        [(b"first".to_vec(), b"first-value".to_vec())]
    );
}

#[test]
fn empty_blocks_have_exact_zero_raw_effects() {
    let mut encoder = Encoder::new();
    let mut decoder = Decoder::new();
    let block = encoder.encode(&[]).unwrap();
    assert!(block.is_empty());
    assert_eq!(decoder.decode(&block, usize::MAX), Ok(Vec::new()));
    assert_eq!(encoder.table.snapshot(), []);
    assert_eq!(decoder.table.snapshot(), []);
    assert_eq!(encoder.diagnostics().encoded_blocks, 1);
    assert_eq!(decoder.diagnostics().decoded_blocks, 1);
    assert_eq!(encoder.diagnostics().field_bytes, 0);
    assert_eq!(decoder.diagnostics().field_bytes, 0);
}

#[test]
fn encoder_persistent_preflight_is_bounded_by_effective_capacity() {
    let mut encoder = Encoder::new();
    encoder.set_max_table_size(64);
    let field = HeaderField::new("x", "value");
    encoder.encode(std::slice::from_ref(&field)).unwrap();
    let repeated = vec![field; 4096];
    encoder.encode(&repeated).unwrap();

    assert!(encoder.table.entries.capacity() <= 4);
    assert!(encoder.table.newest_name.capacity() <= 7);
    assert!(encoder.table.newest_exact.capacity() <= 7);
}

#[test]
fn output_preflight_covers_worst_case_dynamic_name_index() {
    assert_eq!(maximum_literal_len(b"", b"").unwrap(), 3);
    assert_eq!(
        maximum_field_output_len(b"", b"", false, MAX_TABLE_SIZE).unwrap(),
        5
    );
}

#[test]
fn crossed_limit_incremental_literal_allocates_only_retained_state() {
    let block = [0x40, 0x01, b'x', 0x01, b'v'];
    let mut decoder = Decoder::new();
    decoder.set_allocation_failure_after(Some(4));

    assert_eq!(
        decoder.decode(&block, 0),
        Err(Error::HeaderListTooLarge { actual: 34 })
    );
    assert_eq!(decoder.table.snapshot(), [(b"x".to_vec(), b"v".to_vec())]);
}

#[test]
fn reused_indexed_output_needs_no_decoder_allocation() {
    let mut decoder = Decoder::new();
    let mut fields = Vec::new();
    decoder
        .decode_with_visitor_into(&[0x82], usize::MAX, &mut IgnoreHeaderFields, &mut fields)
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, b":method");
    assert_eq!(fields[0].value, b"GET");

    decoder.set_allocation_failure_after(Some(0));
    decoder
        .decode_with_visitor_into(&[0x82], usize::MAX, &mut IgnoreHeaderFields, &mut fields)
        .unwrap();
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].name, b":method");
    assert_eq!(fields[0].value, b"GET");
}

#[test]
fn output_reservation_failure_precedes_dynamic_table_mutation() {
    let mut succeeded = false;
    for successful_allocations in 0..32 {
        let mut decoder = Decoder::new();
        let mut fields = Vec::new();
        decoder.set_allocation_failure_after(Some(successful_allocations));

        match decoder.decode_with_visitor_into(
            &[0x40, 0x01, b'x', 0x01, b'y'],
            usize::MAX,
            &mut IgnoreHeaderFields,
            &mut fields,
        ) {
            Err(Error::AllocationFailed) => {
                assert_eq!(decoder.table.entries.len(), 0);
            }
            Ok(()) => {
                assert_eq!(decoder.table.entries.len(), 1);
                assert_eq!(fields.len(), 1);
                succeeded = true;
                break;
            }
            Err(error) => panic!("unexpected decoder error: {error:?}"),
        }
    }
    assert!(succeeded, "allocation-failure sweep never reached success");
}

#[test]
fn entry_metadata_fits_hpack_per_entry_overhead() {
    assert!(std::mem::size_of::<Entry>() <= 32);
    assert!(std::mem::size_of::<Option<Entry>>() <= 32);
}

#[test]
fn chunked_table_preserves_order_across_chunk_boundaries() {
    let mut table = DynamicTable::new(65_536);
    let mut allocations = AllocationGate::default();
    for index in 0..70 {
        table
            .insert(
                format!("x-{index:02}").as_bytes(),
                b"value",
                &mut allocations,
            )
            .unwrap();
    }
    assert_eq!(table.entries.len(), 70);
    assert_eq!(
        table.get(62).unwrap(),
        (b"x-69".as_slice(), b"value".as_slice())
    );
    assert_eq!(
        table.get(131).unwrap(),
        (b"x-00".as_slice(), b"value".as_slice())
    );
    assert_eq!(table.find_exact(b"x-69", b"value"), Some(62));
    assert_eq!(table.set_max_size_releasing_storage(0), 70);
    assert_eq!(table.entries.len(), 0);
    assert_eq!(table.entries.capacity(), 0);
}

#[test]
fn lookup_is_complete_and_work_is_bounded_by_input_size() {
    let mut table = DynamicTable::new(usize::MAX);
    let mut allocations = AllocationGate::default();
    for index in 0..2000_u16 {
        table
            .insert(b"x", &index.to_be_bytes(), &mut allocations)
            .unwrap();
    }
    assert_eq!(table.find_exact(b"x", &0_u16.to_be_bytes()), Some(2061));
    assert_eq!(table.find_exact(b"x", b"missing"), None);
    let work = table.take_lookup_work();
    assert!(work <= 2 * (b"x".len() + b"missing".len() + 2));
    assert_eq!(table.set_max_size_releasing_storage(0), 2000);
}

#[test]
fn lookup_collision_chains_are_complete_and_equality_safe() {
    let mut table = DynamicTable::new(4096);
    let mut allocations = AllocationGate::default();
    table.try_reserve_insertions(3, &mut allocations).unwrap();
    for (name, value) in [
        (b"a".as_slice(), b"1".as_slice()),
        (b"b", b"2"),
        (b"a", b"3"),
    ] {
        let entry = Entry::try_new(name, value, &mut allocations).unwrap();
        table.insert_prepared_with_hashes(entry, 7, 11);
    }

    assert_eq!(table.find_exact_with_hash(b"a", b"3", 11), Some(62));
    assert_eq!(table.find_exact_with_hash(b"b", b"2", 11), Some(63));
    assert_eq!(table.find_exact_with_hash(b"a", b"1", 11), Some(64));
    assert_eq!(table.find_exact_with_hash(b"a", b"2", 11), None);
    assert_eq!(table.find_name_with_hash(b"a", 7), Some(62));
    assert_eq!(table.find_name_with_hash(b"b", 7), Some(63));
    assert_eq!(table.find_name_with_hash(b"c", 7), None);
}

#[test]
fn small_tables_scan_without_hash_indexes() {
    let mut table = DynamicTable::new(4096);
    let mut allocations = AllocationGate::default();
    for index in 0..HASH_INDEX_THRESHOLD {
        table
            .insert(
                format!("n-{index}").as_bytes(),
                format!("v-{index}").as_bytes(),
                &mut allocations,
            )
            .unwrap();
    }
    assert_eq!(table.entries.len(), HASH_INDEX_THRESHOLD);
    assert!(!table.uses_hash_index());
    assert_eq!(table.newest_name.capacity(), 0);
    assert_eq!(table.newest_exact.capacity(), 0);
    assert_eq!(
        table.find_exact(b"n-15", b"v-15"),
        Some(62),
        "newest entry is index 62"
    );
    assert_eq!(
        table.find_exact(b"n-0", b"v-0"),
        Some(62 + HASH_INDEX_THRESHOLD - 1)
    );
    assert_eq!(table.find_name(b"n-15"), Some(62));
    assert_eq!(table.find_exact(b"missing", b"x"), None);

    table.insert(b"n-16", b"v-16", &mut allocations).unwrap();
    assert_eq!(table.entries.len(), HASH_INDEX_THRESHOLD + 1);
    assert!(table.uses_hash_index());
    assert!(table.newest_name.capacity() > 0);
    assert_eq!(table.find_exact(b"n-16", b"v-16"), Some(62));
    assert_eq!(
        table.find_exact(b"n-0", b"v-0"),
        Some(62 + HASH_INDEX_THRESHOLD)
    );
    assert_eq!(table.find_name(b"n-16"), Some(62));

    table.evict_one();
    assert_eq!(table.entries.len(), HASH_INDEX_THRESHOLD);
    assert!(!table.uses_hash_index());
    assert_eq!(table.find_exact(b"n-16", b"v-16"), Some(62));
    assert_eq!(table.find_exact(b"n-0", b"v-0"), None);
}

#[test]
fn lookup_work_does_not_scale_with_retained_entries_at_required_capacities() {
    for capacity in [0, 4096, 65_535, 65_536, 65_537, MAX_TABLE_SIZE] {
        let mut table = DynamicTable::new(capacity);
        let mut allocations = AllocationGate::default();
        for _ in 0..capacity / 32 {
            table.insert(b"", b"", &mut allocations).unwrap();
        }
        assert_eq!(table.entries.len(), capacity / 32);
        if capacity != 0 {
            assert_eq!(table.find_exact(b"", b""), Some(62));
            assert_eq!(table.find_name(b""), Some(62));
        }
        assert_eq!(table.find_exact(b"not-retained", b"value"), None);
        let work = table.take_lookup_work();
        assert!(
            work <= b"not-retained".len() + b"value".len(),
            "lookup work {work} scaled at capacity {capacity}"
        );
    }
}

#[test]
fn clear_and_zero_capacity_release_history_storage() {
    let mut table = DynamicTable::new(65_536);
    let mut allocations = AllocationGate::default();
    for index in 0..512_u16 {
        table
            .insert(b"x", &index.to_be_bytes(), &mut allocations)
            .unwrap();
    }
    assert!(table.entries.capacity() >= table.entries.len());
    assert!(table.newest_exact.capacity() > 0);
    let retained = table.entries.len();
    assert_eq!(table.set_max_size_releasing_storage(0), retained);
    assert_eq!(table.entries.capacity(), 0);
    assert_eq!(table.newest_name.capacity(), 0);
    assert_eq!(table.newest_exact.capacity(), 0);

    table.set_max_size_releasing_storage(4096);
    for index in 0..32_u8 {
        table.insert(&[index], b"v", &mut allocations).unwrap();
    }
    assert!(table.entries.capacity() > 0);
    assert!(
        !table
            .insert(&vec![b'x'; 4097], b"", &mut allocations)
            .unwrap()
            .0
    );
    assert_eq!(table.entries.capacity(), 0);
    assert_eq!(table.newest_name.capacity(), 0);
    assert_eq!(table.newest_exact.capacity(), 0);
}

#[test]
fn sensitive_name_selection_uses_exact_matches_in_required_order() {
    let mut encoder = Encoder::new();
    let mut allocations = AllocationGate::default();
    encoder
        .table
        .try_reserve_insertions(2, &mut allocations)
        .unwrap();
    encoder.table.ensure_id_space(2).unwrap();
    let dynamic_name = Entry::try_new(b":method", b"PATCH", &mut allocations).unwrap();
    encoder.table.insert_prepared(dynamic_name);
    let dynamic_exact = Entry::try_new(b":method", b"GET", &mut allocations).unwrap();
    encoder.table.insert_prepared(dynamic_exact);

    let block = encoder
        .encode(&[HeaderField::sensitive(":method", "GET")])
        .unwrap();
    assert_eq!(&block[..2], &[0x1f, 0x2f]);

    let mut static_before_name = Encoder::new();
    static_before_name
        .table
        .try_reserve_insertions(1, &mut allocations)
        .unwrap();
    static_before_name.table.ensure_id_space(1).unwrap();
    static_before_name
        .table
        .insert_prepared(Entry::try_new(b":method", b"PATCH", &mut allocations).unwrap());
    let block = static_before_name
        .encode(&[HeaderField::sensitive(":method", "GET")])
        .unwrap();
    assert_eq!(block[0], 0x12);
}

#[test]
fn encode_by_visits_each_field_once() {
    let fields = [
        HeaderField::new(":method", "GET"),
        HeaderField::new(":path", "/"),
        HeaderField::new("x-custom", "one"),
        HeaderField::new("x-custom", "two"),
    ];
    let mut visits = [0usize; 4];
    let mut encoder = Encoder::new();
    let encoded = encoder
        .encode_by(fields.len(), |index| {
            visits[index] += 1;
            let field = &fields[index];
            HeaderFieldRef {
                name: &field.name,
                value: &field.value,
                sensitive: field.sensitive,
            }
        })
        .unwrap();
    assert_eq!(visits, [1, 1, 1, 1]);
    let mut baseline = Encoder::new();
    assert_eq!(baseline.encode(&fields).unwrap(), encoded);
}

#[test]
fn planned_dynamic_lookup_work_is_linear_in_fields() {
    let common_request = [
        HeaderField::new(":method", "GET"),
        HeaderField::new(":scheme", "http"),
        HeaderField::new(":authority", "example.test"),
        HeaderField::new(":path", "/"),
        HeaderField::new("host", "example.test"),
        HeaderField::new("content-length", "0"),
    ];
    let mut encoder = Encoder::new();
    encoder.encode(&common_request).unwrap();
    assert_eq!(
        encoder.take_planned_lookup_work(),
        (2, 2),
        "the common request probes the staged indexes instead of revisiting fields"
    );

    let common_response = [
        HeaderField::new(":status", "200"),
        HeaderField::new("content-length", "13"),
    ];
    let mut encoder = Encoder::new();
    encoder.encode(&common_response).unwrap();
    assert_eq!(encoder.take_planned_lookup_work(), (0, 0));

    let unique_fields = [
        HeaderField::new("x-a", "1"),
        HeaderField::new("x-b", "2"),
        HeaderField::new("x-c", "3"),
        HeaderField::new("x-d", "4"),
        HeaderField::new("x-e", "5"),
        HeaderField::new("x-f", "6"),
    ];
    let mut encoder = Encoder::new();
    encoder.encode(&unique_fields).unwrap();
    assert_eq!(
        encoder.take_planned_lookup_work(),
        (6, 5),
        "six unique fields perform bounded hash probes instead of 15 scans per lookup kind"
    );
}

#[test]
fn planned_eviction_cursor_visits_each_staged_field_once() {
    let fields = (0..64)
        .map(|index| HeaderField::new(format!("x-{index:02}"), "v"))
        .collect::<Vec<_>>();
    let mut encoder = Encoder::new();
    encoder.set_max_table_size(80);
    encoder.encode(&fields).unwrap();

    assert_eq!(encoder.planned_lookup_work.take_eviction(), 62);
}

#[test]
fn planned_dynamic_lookup_is_equality_safe_under_forced_hash_collisions() {
    let fields = [
        HeaderField::new("x-a", "1"),
        HeaderField::new("x-b", "2"),
        HeaderField::new("x-a", "3"),
        HeaderField::new("x-c", "4"),
        HeaderField::new("x-a", "1"),
    ];
    let mut encoder = Encoder::new();
    encoder.set_forced_planned_lookup_hash(Some(7));
    let encoded = encoder.encode(&fields).unwrap();

    let mut decoder = Decoder::new();
    assert_eq!(decoder.decode(&encoded, usize::MAX), Ok(fields.to_vec()));
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[test]
fn planned_dynamic_lookup_ignores_evicted_staged_entries() {
    let fields = [
        HeaderField::new("x-a", "1"),
        HeaderField::new("x-b", "2"),
        HeaderField::new("x-c", "3"),
        HeaderField::new("x-a", "1"),
    ];
    let mut encoder = Encoder::new();
    encoder.set_max_table_size(80);
    encoder.set_forced_planned_lookup_hash(Some(7));
    let encoded = encoder.encode(&fields).unwrap();
    assert!(
        encoded.ends_with(&[0x40, 0x03, b'x', b'-', b'a', 0x01, b'1']),
        "the evicted exact candidate must be emitted as a new-name literal"
    );

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(80);
    assert_eq!(decoder.decode(&encoded, usize::MAX), Ok(fields.to_vec()));
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[test]
fn planned_dynamic_indexes_preserve_newest_first_offsets() {
    let staged = [
        HeaderField::new("x-a", "1"),
        HeaderField::new("x-b", "2"),
        HeaderField::new("x-a", "1"),
    ];
    let mut encoder = Encoder::new();
    let encoded = encoder.encode(&staged).unwrap();
    assert_eq!(encoded.last(), Some(&0xbf), "staged x-a shifts to index 63");

    let repeated_name = [
        HeaderField::new("x-a", "1"),
        HeaderField::new("x-a", "2"),
        HeaderField::new("x-a", ""),
    ];
    let mut encoder = Encoder::new();
    let encoded = encoder.encode(&repeated_name).unwrap();
    assert!(
        encoded.ends_with(&[0x7e, 0x00]),
        "the newest staged x-a name remains at index 62"
    );

    let mut encoder = Encoder::new();
    encoder
        .encode(&[HeaderField::new("x-old", "value")])
        .unwrap();
    let shifted_existing = [
        HeaderField::new("x-new", "value"),
        HeaderField::new("x-old", "value"),
    ];
    let encoded = encoder.encode(&shifted_existing).unwrap();
    assert_eq!(
        encoded.last(),
        Some(&0xbf),
        "a staged insertion shifts the existing exact match to index 63"
    );
}

#[test]
fn planned_dynamic_lookup_crosses_inline_overflow_and_eviction_boundaries() {
    let mut fields = (0..=INLINE_STAGED_FIELDS)
        .map(|index| HeaderField::new(format!("x-{index:02}"), "v"))
        .collect::<Vec<_>>();
    fields.push(HeaderField::new("x-00", "v"));
    fields.push(HeaderField::new(
        format!("x-{INLINE_STAGED_FIELDS:02}"),
        "v",
    ));

    let mut encoder = Encoder::new();
    encoder.set_max_table_size(600);
    let encoded = encoder.encode(&fields).unwrap();
    assert_eq!(
        encoded.last(),
        Some(&0xbf),
        "the active overflow-side field follows the reinserted inline-side field"
    );

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(600);
    assert_eq!(decoder.decode(&encoded, usize::MAX), Ok(fields));
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[test]
fn planned_dynamic_name_lookup_crosses_inline_overflow_and_eviction_boundaries() {
    let mut fields = vec![HeaderField::new("x-shared", "old")];
    fields.extend(
        (1..INLINE_STAGED_FIELDS).map(|index| HeaderField::new(format!("x-{index:02}"), "v")),
    );
    fields.push(HeaderField::new("x-shared", ""));
    fields.push(HeaderField::new("x-shared", "z"));

    let mut encoder = Encoder::new();
    encoder.set_max_table_size(600);
    let encoded = encoder.encode(&fields).unwrap();
    assert!(
        encoded.windows(3).any(|bytes| bytes == [0x7f, 0x0e, 0x00]),
        "the inline-side name has dynamic index 77 before its eviction"
    );
    assert!(
        encoded.ends_with(&[0x7e, 0x01, b'z']),
        "the overflow-side name becomes the newest dynamic entry at index 62"
    );

    let mut decoder = Decoder::new();
    decoder.set_max_allowed_table_size(600);
    assert_eq!(decoder.decode(&encoded, usize::MAX), Ok(fields));
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[test]
fn sensitive_same_block_lookup_prefers_staged_exact_over_newer_name() {
    let fields = [
        HeaderField::new("x-secret", ""),
        HeaderField::new("x-secret", "other"),
        HeaderField::sensitive("x-secret", ""),
    ];
    let mut encoder = Encoder::new();
    encoder.set_forced_planned_lookup_hash(Some(7));
    let encoded = encoder.encode(&fields).unwrap();
    assert!(
        encoded.ends_with(&[0x1f, 0x30, 0x00]),
        "the never-indexed literal uses the older exact match at index 63"
    );

    let mut decoder = Decoder::new();
    assert_eq!(decoder.decode(&encoded, usize::MAX), Ok(fields.to_vec()));
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[test]
fn large_noninserting_blocks_do_not_allocate_planned_lookup_overflow() {
    let field_sets = [
        vec![HeaderField::new(":method", "GET"); INLINE_STAGED_FIELDS + 1],
        vec![HeaderField::sensitive("authorization", "secret"); INLINE_STAGED_FIELDS + 1],
    ];
    for fields in field_sets {
        let mut encoder = Encoder::new();
        encoder.set_allocation_failure_after(Some(2));
        assert!(encoder.encode(&fields).is_ok());
    }
}

#[test]
fn reused_static_block_does_not_allocate_insertion_entry_storage() {
    let fields = [
        HeaderField::new(":method", "GET"),
        HeaderField::new(":scheme", "https"),
        HeaderField::new(":path", "/"),
        HeaderField::new(":status", "200"),
    ];
    let mut encoder = Encoder::new();
    encoder.encode(&fields).unwrap();
    encoder.set_allocation_failure_after(Some(1));

    assert!(encoder.encode(&fields).is_ok());
}

#[test]
fn planned_dynamic_lookup_overflow_grows_across_forced_collisions() {
    let fields = (0..65)
        .map(|index| HeaderField::new(format!("x-{index:02}"), index.to_string()))
        .collect::<Vec<_>>();
    let mut encoder = Encoder::new();
    encoder.set_forced_planned_lookup_hash(Some(7));
    let encoded = encoder.encode(&fields).unwrap();

    let mut decoder = Decoder::new();
    assert_eq!(decoder.decode(&encoded, usize::MAX), Ok(fields));
    assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
}

#[derive(Default)]
struct RecordingOutput {
    planned_len: Option<usize>,
    bytes: Vec<u8>,
}

impl EncodeOutput for RecordingOutput {
    fn try_reserve_exact(&mut self, encoded_len: usize) -> Result<(), Error> {
        self.planned_len = Some(encoded_len);
        self.bytes
            .try_reserve_exact(encoded_len)
            .map_err(|_| Error::AllocationFailed)
    }

    fn push(&mut self, byte: u8) {
        self.bytes.push(byte);
    }

    fn extend_from_slice(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    fn encoded_len(&self) -> usize {
        self.bytes.len()
    }
}

struct RecordingPreflightVisitor<'a> {
    visits: &'a Cell<usize>,
    reject: bool,
}

impl EncodePreflightVisitor for RecordingPreflightVisitor<'_> {
    type Output = usize;
    type Error = &'static str;
    const FINISH_AFTER_ENCODE_ERROR: bool = true;

    fn visit(&mut self, _field: HeaderFieldRef<'_>) {
        self.visits.set(self.visits.get() + 1);
    }

    fn finish(self) -> Result<Self::Output, Self::Error> {
        if self.reject {
            Err("rejected")
        } else {
            Ok(self.visits.get())
        }
    }
}

#[test]
fn encode_preflight_visitor_shares_the_single_field_fetch() {
    let fields = [
        HeaderField::new(":method", "GET"),
        HeaderField::new(":path", "/"),
        HeaderField::new("x-custom", "one"),
        HeaderField::new("x-custom", "two"),
    ];
    let mut source_visits = [0usize; 4];
    let visitor_visits = Cell::new(0);
    let mut encoder = Encoder::new();
    let mut output = RecordingOutput::default();

    let validated = encoder
        .encode_by_into_with_visitor(
            fields.len(),
            |index| {
                source_visits[index] += 1;
                let field = &fields[index];
                HeaderFieldRef {
                    name: &field.name,
                    value: &field.value,
                    sensitive: field.sensitive,
                }
            },
            RecordingPreflightVisitor {
                visits: &visitor_visits,
                reject: false,
            },
            &mut output,
        )
        .unwrap();

    let mut baseline = Encoder::new();
    let expected = baseline.encode(&fields).unwrap();
    assert_eq!(validated, fields.len());
    assert_eq!(source_visits, [1, 1, 1, 1]);
    assert_eq!(visitor_visits.get(), fields.len());
    assert_eq!(output.bytes, expected);
    assert_eq!(encoder.table.snapshot(), baseline.table.snapshot());
    assert_eq!(encoder.diagnostics(), baseline.diagnostics());
}

#[test]
fn encode_preflight_rejection_precedes_allocation_and_is_retryable() {
    let fields = (0..=INLINE_STAGED_FIELDS)
        .map(|index| HeaderField::new(format!("x-{index}"), index.to_string()))
        .collect::<Vec<_>>();
    let mut encoder = Encoder::new();
    encoder.set_max_table_size(128);
    encoder.diagnostics = Diagnostics {
        encoded_blocks: 7,
        ..Diagnostics::default()
    };
    let table_before = encoder.table.snapshot();
    let diagnostics_before = encoder.diagnostics();
    let minimum_before = encoder.pending_min_size;
    let final_before = encoder.pending_final_size;
    let source_visits = Cell::new(0);
    let visitor_visits = Cell::new(0);
    let mut output = RecordingOutput::default();
    encoder.set_allocation_failure_after(Some(0));

    assert_eq!(
        encoder.encode_by_into_with_visitor(
            fields.len(),
            |index| {
                source_visits.set(source_visits.get() + 1);
                let field = &fields[index];
                HeaderFieldRef {
                    name: &field.name,
                    value: &field.value,
                    sensitive: field.sensitive,
                }
            },
            RecordingPreflightVisitor {
                visits: &visitor_visits,
                reject: true,
            },
            &mut output,
        ),
        Err(EncodePreflightError::Visitor("rejected"))
    );
    assert_eq!(source_visits.get(), fields.len());
    assert_eq!(visitor_visits.get(), fields.len());
    assert_eq!(encoder.table.snapshot(), table_before);
    assert_eq!(encoder.diagnostics(), diagnostics_before);
    assert_eq!(encoder.pending_min_size, minimum_before);
    assert_eq!(encoder.pending_final_size, final_before);
    assert_eq!(output.planned_len, None);
    assert!(output.bytes.is_empty());

    encoder.set_allocation_failure_after(None);
    let mut retry_output = RecordingOutput::default();
    encoder
        .encode_by_into(
            fields.len(),
            |index| {
                let field = &fields[index];
                HeaderFieldRef {
                    name: &field.name,
                    value: &field.value,
                    sensitive: field.sensitive,
                }
            },
            &mut retry_output,
        )
        .unwrap();
    let mut baseline = Encoder::new();
    baseline.set_max_table_size(128);
    baseline.diagnostics = Diagnostics {
        encoded_blocks: 7,
        ..Diagnostics::default()
    };
    let expected = baseline.encode(&fields).unwrap();
    assert_eq!(retry_output.bytes, expected);
    assert_eq!(encoder.table.snapshot(), baseline.table.snapshot());
    assert_eq!(encoder.diagnostics(), baseline.diagnostics());
}

#[test]
fn encode_preflight_noop_does_not_fetch_fields_after_planning_failure() {
    let field_count = INLINE_STAGED_FIELDS + 1;
    let source_visits = Cell::new(0);
    let mut encoder = Encoder::new();
    let mut output = RecordingOutput::default();
    encoder.set_allocation_failure_after(Some(0));

    assert_eq!(
        encoder.encode_by_into(
            field_count,
            |_| {
                source_visits.set(source_visits.get() + 1);
                HeaderFieldRef {
                    name: b"x-test",
                    value: b"one",
                    sensitive: false,
                }
            },
            &mut output,
        ),
        Err(Error::AllocationFailed)
    );
    assert_eq!(source_visits.get(), 0);
    assert_eq!(output.planned_len, None);
    assert!(output.bytes.is_empty());
}

#[test]
fn planned_lookup_overflow_allocation_failures_are_transactional() {
    fn encode_fields(
        encoder: &mut Encoder,
        fields: &[HeaderField],
        output: &mut RecordingOutput,
    ) -> Result<(), Error> {
        encoder.encode_by_into(
            fields.len(),
            |index| {
                let field = &fields[index];
                HeaderFieldRef {
                    name: &field.name,
                    value: &field.value,
                    sensitive: field.sensitive,
                }
            },
            output,
        )
    }

    for field_count in [17, 33, 65] {
        let fields = (0..field_count)
            .map(|index| HeaderField::new(format!("x-{index:02}"), index.to_string()))
            .collect::<Vec<_>>();
        let setup = || {
            let mut encoder = Encoder::new();
            encoder.set_max_table_size(128);
            encoder.diagnostics = Diagnostics {
                encoded_blocks: 7,
                ..Diagnostics::default()
            };
            encoder
        };
        let mut expected_encoder = setup();
        let mut expected_output = RecordingOutput::default();
        encode_fields(&mut expected_encoder, &fields, &mut expected_output).unwrap();

        let allocations_until_success = (0..256)
            .find(|&successful_allocations| {
                let mut encoder = setup();
                encoder.set_allocation_failure_after(Some(successful_allocations));
                encode_fields(&mut encoder, &fields, &mut RecordingOutput::default()).is_ok()
            })
            .expect("overflow encode succeeds within the allocation sweep");
        assert!(
            allocations_until_success > field_count,
            "the sweep reaches the lookup growth for {field_count} candidates"
        );

        for successful_allocations in 0..allocations_until_success {
            let mut encoder = setup();
            let table_before = encoder.table.snapshot();
            let diagnostics_before = encoder.diagnostics();
            let minimum_before = encoder.pending_min_size;
            let final_before = encoder.pending_final_size;
            let mut output = RecordingOutput::default();
            encoder.set_allocation_failure_after(Some(successful_allocations));

            assert_eq!(
                encode_fields(&mut encoder, &fields, &mut output),
                Err(Error::AllocationFailed)
            );
            assert_eq!(encoder.table.snapshot(), table_before);
            assert_eq!(encoder.diagnostics(), diagnostics_before);
            assert_eq!(encoder.pending_min_size, minimum_before);
            assert_eq!(encoder.pending_final_size, final_before);
            assert_eq!(output.planned_len, None);
            assert!(output.bytes.is_empty());

            encoder.set_allocation_failure_after(None);
            let mut retry_output = RecordingOutput::default();
            encode_fields(&mut encoder, &fields, &mut retry_output).unwrap();
            assert_eq!(retry_output.bytes, expected_output.bytes);
            assert_eq!(encoder.table.snapshot(), expected_encoder.table.snapshot());
        }
    }
}

#[test]
fn planned_lookup_arithmetic_overflow_is_reported() {
    let mut lookups = PlannedLookupTables::new();
    assert_eq!(
        lookups.try_prepare_for_insertion(usize::MAX, &mut AllocationGate::default()),
        Err(Error::StateOverflow)
    );
    assert!(lookups.overflow.is_empty());

    let staged = StagedField {
        field: HeaderFieldRef {
            name: b"x",
            value: b"v",
            sensitive: false,
        },
        planning_active: true,
        insertion_ordinal: Some(usize::MAX),
        exact_index: None,
        name_index: None,
    };
    assert_eq!(
        planned_staged_index(&staged, usize::MAX),
        Err(Error::StateOverflow)
    );
}

#[test]
fn encode_plan_length_is_exact_inline_and_after_overflow() {
    for field_count in [4, INLINE_STAGED_FIELDS + 1] {
        let fields = (0..field_count)
            .map(|index| {
                if index == 0 {
                    HeaderField::new("x-duplicate", "one")
                } else if index == 1 {
                    HeaderField::new("x-duplicate", "two")
                } else if index % 3 == 0 {
                    HeaderField::sensitive("authorization", "secret")
                } else {
                    HeaderField::new("x-repeated", "value")
                }
            })
            .collect::<Vec<_>>();
        let mut encoder = Encoder::new();
        let mut output = RecordingOutput::default();

        encoder
            .encode_by_into(
                fields.len(),
                |index| {
                    let field = &fields[index];
                    HeaderFieldRef {
                        name: &field.name,
                        value: &field.value,
                        sensitive: field.sensitive,
                    }
                },
                &mut output,
            )
            .unwrap();

        assert_eq!(output.planned_len, Some(output.bytes.len()));
        let mut decoder = Decoder::new();
        assert_eq!(
            decoder.decode(&output.bytes, usize::MAX),
            Ok(fields.clone())
        );
        assert_eq!(encoder.table.snapshot(), decoder.table.snapshot());
    }
}

#[test]
fn encoder_allocation_failures_leave_logical_state_unchanged() {
    fn allocations_until_success(setup: impl Fn() -> Encoder, field: HeaderField) -> usize {
        for successful_allocations in 0..32 {
            let mut encoder = setup();
            encoder.set_allocation_failure_after(Some(successful_allocations));
            if encoder.encode(std::slice::from_ref(&field)).is_ok() {
                return successful_allocations;
            }
        }
        panic!("encode never succeeded")
    }

    let resize_setup = || {
        let mut encoder = Encoder::new();
        encoder.set_max_table_size(0);
        encoder.set_max_table_size(128);
        encoder
    };
    let resize_field = HeaderField::new("x-next", "value");
    let resize_needed = allocations_until_success(resize_setup, resize_field.clone());
    assert!(resize_needed > 0, "resized encode must allocate");
    for successful_allocations in 0..resize_needed {
        let mut encoder = resize_setup();
        let table_before = encoder.table.snapshot();
        let diagnostics_before = encoder.diagnostics();
        let minimum_before = encoder.pending_min_size;
        let final_before = encoder.pending_final_size;
        encoder.set_allocation_failure_after(Some(successful_allocations));

        assert_eq!(
            encoder.encode(std::slice::from_ref(&resize_field)),
            Err(Error::AllocationFailed)
        );
        assert_eq!(encoder.table.snapshot(), table_before);
        assert_eq!(encoder.diagnostics(), diagnostics_before);
        assert_eq!(encoder.pending_min_size, minimum_before);
        assert_eq!(encoder.pending_final_size, final_before);
    }

    let follow_up_setup = || {
        let mut encoder = Encoder::new();
        encoder
            .encode(&[HeaderField::new("x-prime", "value")])
            .unwrap();
        encoder
    };
    let follow_up_field = HeaderField::new("x-next", "value");
    let follow_up_needed = allocations_until_success(follow_up_setup, follow_up_field.clone());
    assert!(follow_up_needed > 0, "follow-up encode must allocate");
    for successful_allocations in 0..follow_up_needed {
        let mut encoder = follow_up_setup();
        let table_before = encoder.table.snapshot();
        let diagnostics_before = encoder.diagnostics();
        encoder.set_allocation_failure_after(Some(successful_allocations));
        assert_eq!(
            encoder.encode(std::slice::from_ref(&follow_up_field)),
            Err(Error::AllocationFailed)
        );
        assert_eq!(encoder.table.snapshot(), table_before);
        assert_eq!(encoder.diagnostics(), diagnostics_before);
    }
}
