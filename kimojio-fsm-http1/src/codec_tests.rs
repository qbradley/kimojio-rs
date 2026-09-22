use super::*;

fn check_exact_limit(encode: impl Fn(&Config) -> Result<EncodedHead, CommandError>) {
    let config = Config {
        max_body_bytes: u64::MAX,
        ..Config::default()
    };
    let expected = encode(&config).unwrap();
    let length = expected.bytes.len();
    // Independent wire oracle: include generated fields, not the start line or
    // terminating empty line. Exercise the complete request/response matrix.
    let fields = expected
        .bytes
        .windows(2)
        .filter(|pair| *pair == b"\r\n")
        .count()
        - 2;
    assert_eq!(expected.fields, fields);
    for limit in [length - 1, length, length + 1] {
        let result = encode(&Config {
            max_head_bytes: limit,
            ..config.clone()
        });
        if limit < length {
            assert_eq!(result, Err(CommandError::Limit));
        } else {
            assert_eq!(result.unwrap(), expected);
        }
    }
}

#[test]
fn request_head_sizes_cover_decimal_boundaries_and_generated_expect() {
    let headers = [
        Header {
            name: "host",
            value: b"a",
        },
        Header {
            name: "x-value",
            value: b"\t\xff",
        },
    ];
    for version in [Version::Http10, Version::Http11] {
        for body in [
            BodyLength::Empty,
            BodyLength::Known(0),
            BodyLength::Known(1),
            BodyLength::Known(9),
            BodyLength::Known(10),
            BodyLength::Known(99),
            BodyLength::Known(100),
            BodyLength::Known(999),
            BodyLength::Known(1000),
            BodyLength::Known(u64::MAX),
            BodyLength::Streaming,
        ] {
            if version == Version::Http10 && body == BodyLength::Streaming {
                continue;
            }
            for expect_continue in [false, true] {
                for target in ["/", "/long/path?query=value"] {
                    let request = Request {
                        head: RequestHead {
                            method: "POST",
                            target,
                            version,
                            headers: &headers,
                        },
                        body,
                        expect_continue,
                    };
                    check_exact_limit(|config| encode_request(request, config));
                }
            }
        }
    }
}

#[test]
fn response_head_sizes_cover_suppression_and_generated_connection_fields() {
    for version in [Version::Http10, Version::Http11] {
        for status in [100, 103, 200, 204, 205, 304, 599] {
            for body in [
                BodyLength::Empty,
                BodyLength::Known(0),
                BodyLength::Known(9),
                BodyLength::Known(10),
                BodyLength::Known(99),
                BodyLength::Known(100),
                BodyLength::Known(u64::MAX),
                BodyLength::Streaming,
            ] {
                for head_method in [false, true] {
                    for close in [false, true] {
                        for tunnel in [false, true] {
                            if ((status < 200 || status == 204 || tunnel)
                                && body != BodyLength::Empty)
                                || (status == 205
                                    && !matches!(body, BodyLength::Empty | BodyLength::Known(0)))
                            {
                                continue;
                            }
                            for connection in [
                                None,
                                Some("close"),
                                Some("keep-alive"),
                                Some("keep-alive, close"),
                            ] {
                                let header = [Header {
                                    name: "connection",
                                    value: connection.unwrap_or("").as_bytes(),
                                }];
                                for reason in ["", "Some reason"] {
                                    let response = Response {
                                        head: ResponseHead {
                                            version,
                                            status,
                                            reason,
                                            headers: if connection.is_some() {
                                                &header
                                            } else {
                                                &[]
                                            },
                                        },
                                        body,
                                    };
                                    check_exact_limit(|config| {
                                        encode_response(
                                            response,
                                            head_method,
                                            close,
                                            tunnel,
                                            config,
                                        )
                                    });
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn specialized_numbers_match_standard_formatting_at_digit_boundaries() {
    let mut values = vec![0, 1, u64::MAX];
    let mut power = 10u64;
    loop {
        values.extend([power - 1, power, power + 1]);
        let Some(next) = power.checked_mul(10) else {
            break;
        };
        power = next;
    }
    for value in values {
        let expected = value.to_string();
        let mut out = HeadWriter::new(expected.len());
        out.decimal(value).unwrap();
        assert_eq!(out.bytes, expected.as_bytes());
        assert_eq!(
            HeadWriter::new(expected.len() - 1).decimal(value),
            Err(CommandError::Limit)
        );
    }
    let mut values = vec![0, 1, usize::MAX];
    for shift in (4..usize::BITS).step_by(4) {
        let power = 1usize << shift;
        values.extend([power - 1, power, power + 1]);
    }
    for value in values {
        let mut prefix = [0xa5; 24];
        let len = encode_chunk_size(value, &mut prefix);
        assert_eq!(&prefix[..len], format!("{value:x}\r\n").as_bytes());
        assert!(prefix[len..].iter().all(|byte| *byte == 0xa5));
    }
}

#[test]
fn status_and_start_lines_have_exact_wire_bytes() {
    for version in [Version::Http10, Version::Http11] {
        let version_text = super::version(version);
        for status in 100..=599 {
            let response = Response {
                head: ResponseHead {
                    version,
                    status,
                    reason: "Reason\twith space",
                    headers: &[],
                },
                body: BodyLength::Empty,
            };
            let EncodedHead { bytes, .. } =
                encode_response(response, false, false, false, &Config::default()).unwrap();
            assert!(
                bytes.starts_with(
                    format!("{version_text} {status} Reason\twith space\r\n").as_bytes()
                )
            );
        }
        let headers = [Header {
            name: "host",
            value: b"example",
        }];
        let request = Request {
            head: RequestHead {
                method: "POST",
                target: "/a?b=c",
                version,
                headers: &headers,
            },
            body: BodyLength::Known(1234567890),
            expect_continue: true,
        };
        let EncodedHead { bytes, .. } = encode_request(
            request,
            &Config {
                max_body_bytes: u64::MAX,
                ..Config::default()
            },
        )
        .unwrap();
        assert_eq!(bytes, format!(
            "POST /a?b=c {version_text}\r\nhost: example\r\ncontent-length: 1234567890\r\nexpect: 100-continue\r\n\r\n"
        ).as_bytes());
    }
}

#[test]
fn token_table_matches_ascii_grammar_for_every_byte() {
    assert!(!token(b""));
    for byte in 0..=255u8 {
        let expected = byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte);
        assert_eq!(token(&[byte]), expected);
        assert_eq!(token(&[b'a', byte, b'Z']), expected);
    }
}

#[test]
fn single_pass_chunk_sizes_cover_hex_boundaries_and_extensions() {
    for value in [
        0,
        1,
        15,
        16,
        255,
        256,
        4095,
        4096,
        u32::MAX as u64,
        u64::MAX,
    ] {
        for digits in [
            format!("{value:x}"),
            format!("{value:X}"),
            format!("000000{value:x}"),
        ] {
            for extension in ["", ";name", " ;name=token", ";name=\"quoted\\\"value\""] {
                assert_eq!(
                    chunk_size(format!("{digits}{extension}").as_bytes()),
                    Ok(value)
                );
            }
        }
    }
    for line in [
        "",
        "x",
        "-1",
        "10000000000000000",
        "fffffffffffffffff",
        "2;",
        "2 ",
        "2;name=\"unterminated",
    ] {
        assert_eq!(chunk_size(line.as_bytes()), Err(Failure::Protocol));
    }
}

#[test]
fn head_size_arithmetic_rejects_overflow_before_allocation() {
    assert_eq!(
        head_length(usize::MAX, &[usize::MAX, 1]),
        Err(CommandError::Limit)
    );
    assert_eq!(head_length(16, &[8, 9]), Err(CommandError::Limit));
    assert_eq!(head_length(16, &[8, 8]), Ok(16));
    assert_eq!(head_length(0, &[]), Ok(0));
}

#[test]
fn head_size_preflight_preserves_validation_error_precedence() {
    let config = Config {
        max_head_bytes: 20,
        ..Config::default()
    };
    // Each field fits alone, but the complete head would exceed the budget.
    let reserved = [Header {
        name: "content-length",
        value: b"0",
    }];
    let invalid = [Header {
        name: "bad name",
        value: b"x",
    }];
    for (headers, expected) in [
        (reserved.as_slice(), CommandError::InvalidFraming),
        (invalid.as_slice(), CommandError::InvalidHead),
    ] {
        let request = Request {
            head: RequestHead {
                method: "GET",
                target: "/",
                version: Version::Http10,
                headers,
            },
            body: BodyLength::Empty,
            expect_continue: false,
        };
        assert_eq!(encode_request(request, &config), Err(expected));
        let response = Response::new(200, "OK", headers, BodyLength::Empty);
        assert_eq!(
            encode_response(response, false, false, false, &config),
            Err(expected)
        );
    }
    let response = Response::new(200, "bad\nreason", &[], BodyLength::Empty);
    assert_eq!(
        encode_response(response, false, false, false, &config),
        Err(CommandError::InvalidHead)
    );
    let response = Response::new(204, "No Content", &[], BodyLength::Known(1));
    assert_eq!(
        encode_response(response, false, false, false, &config),
        Err(CommandError::InvalidFraming)
    );
}
