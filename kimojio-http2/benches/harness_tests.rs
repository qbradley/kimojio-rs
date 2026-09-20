#[allow(
    dead_code,
    reason = "allocator controls share the real probe implementation"
)]
#[path = "support/allocation.rs"]
mod allocation;
#[allow(dead_code, reason = "also used by standalone benchmark executables")]
mod support;

#[test]
fn options_and_payload_negative_controls() {
    assert!(support::Options::parse(["--concurrency".into(), "129".into()]).is_err());
    assert!(support::Options::parse(["--chunk".into(), "0".into()]).is_err());
    assert!(support::Options::parse(["--phase".into(), "cold".into()]).is_err());
    assert!(support::Options::parse(["--bench".into()]).is_ok());
    assert!(support::Options::parse(["--test".into()]).is_ok());
    assert!(support::compare(&[0, 1, 2], 0, 3, true).is_ok());
    assert!(support::compare(&[0, 9, 2], 0, 3, true).is_err());
    assert!(support::compare(&[0, 1, 2, 3], 0, 3, true).is_err());
}

#[kimojio::test]
async fn real_socket_modes_streaming_and_retirement() {
    for backend in ["native", "generic"] {
        for case in ["empty", "fixed", "stream", "duplex"] {
            let report = support::run(
                support::Options {
                    backend: backend.into(),
                    case: case.into(),
                    concurrency: 8,
                    cohorts: 2,
                    warmup: 1,
                    bytes: 32768,
                    ..support::Options::default()
                },
                support::NoMeter,
            )
            .await
            .unwrap();
            assert_eq!(report["client_retirements"], 24);
            assert_eq!(report["server_retirements"], 24);
            assert_eq!(report["drivers_closed_successfully"], 2);
            if case == "duplex" {
                assert_eq!(report["early_response_witnesses_including_warmup"], 24);
            }
        }
    }
}

#[kimojio::test]
async fn cold_owned_buffers_and_concurrency_bounds() {
    for backend in ["native", "generic"] {
        for concurrency in [1, 32, 128] {
            let report = support::run(
                support::Options {
                    backend: backend.into(),
                    case: "fixed".into(),
                    phase: "cold".into(),
                    concurrency,
                    cohorts: 1,
                    warmup: 0,
                    owned: true,
                    ..support::Options::default()
                },
                support::NoMeter,
            )
            .await
            .unwrap();
            assert_eq!(report["client_retirements"], concurrency);
            assert_eq!(report["server_retirements"], concurrency);
            assert_eq!(report["drivers_closed_successfully"], 2);
        }
    }
}

#[kimojio::test]
async fn corrupted_payload_cannot_report_success() {
    for backend in ["native", "generic"] {
        let result = support::run(
            support::Options {
                backend: backend.into(),
                case: "duplex".into(),
                concurrency: 8,
                cohorts: 1,
                warmup: 0,
                bytes: 32768,
                corrupt_response: true,
                ..support::Options::default()
            },
            support::NoMeter,
        )
        .await;
        assert!(result.is_err());
    }
}
