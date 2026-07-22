use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::process::Command;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use std::thread;
use std::time::{Duration, Instant};

use kimojio_fsm_http::{
    H2Client, H2FairStreamScheduler, H2Frame, H2FrameRef, H2FrameType, H2ReceiveWindow, H2Server,
    H2StreamEvent, Http1ConnectionDecoder, Http1ConnectionEvent, Http1HeaderScratch, Http1Server,
};

struct CountingAllocator;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
static MEASUREMENT_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    static ACTIVE: Cell<bool> = const { Cell::new(false) };
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if ACTIVE.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if ACTIVE.get() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

struct ActiveMeasurement;

impl ActiveMeasurement {
    fn start() -> Self {
        assert!(
            !ACTIVE.replace(true),
            "allocation measurement is not nested"
        );
        Self
    }
}

impl Drop for ActiveMeasurement {
    fn drop(&mut self) {
        ACTIVE.set(false);
    }
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, usize) {
    assert!(!ACTIVE.get(), "allocation measurement is not nested");
    let _guard = MEASUREMENT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    ALLOCATIONS.store(0, Ordering::Relaxed);
    let active = ActiveMeasurement::start();
    let output = f();
    drop(active);
    (output, ALLOCATIONS.load(Ordering::Relaxed))
}

#[test]
fn nested_allocation_measurement_fails_fast() {
    const CHILD_ENV: &str = "KIMOJIO_NESTED_ALLOCATION_MEASUREMENT_CHILD";
    if std::env::var_os(CHILD_ENV).is_some() {
        let panic = std::panic::catch_unwind(|| measure(|| measure(|| 7)));
        assert!(panic.is_err());
        return;
    }

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "nested_allocation_measurement_fails_fast",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "nested measurement child failed");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("nested allocation measurement did not fail fast");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn allocation_measurement_recovers_after_a_caught_panic() {
    let panic = std::panic::catch_unwind(|| {
        measure(|| assert!(!std::hint::black_box(true), "measured panic"))
    });

    assert!(panic.is_err());
    assert!(!ACTIVE.get());
    let (value, allocations) = measure(|| 7);
    assert_eq!(value, 7);
    assert_eq!(allocations, 0);
}

#[test]
fn borrowed_frame_decode_avoids_the_owned_payload_allocation() {
    static FRAME: [u8; 25] = [
        0, 0, 16, 0, 1, 0, 0, 0, 1, b'0', b'1', b'2', b'3', b'4', b'5', b'6', b'7', b'8', b'9',
        b'a', b'b', b'c', b'd', b'e', b'f',
    ];

    let ((borrowed, consumed), borrowed_allocations) =
        measure(|| H2FrameRef::decode(&FRAME).unwrap());
    assert_eq!(consumed, FRAME.len());
    assert_eq!(borrowed.payload.as_ptr(), FRAME[9..].as_ptr());
    assert_eq!(borrowed_allocations, 0);

    let ((owned, consumed), owned_allocations) = measure(|| H2Frame::decode(&FRAME).unwrap());
    assert_eq!(consumed, FRAME.len());
    assert_eq!(owned.payload, borrowed.payload);
    assert_eq!(owned_allocations, 1);
}

#[test]
fn http1_header_scratch_does_not_allocate_after_construction() {
    const INFORMATIONAL: &[u8] = b"HTTP/1.1 100 Continue\r\nx-first: one\r\n\r\n";
    const FINAL: &[u8] = b"HTTP/1.1 204 No Content\r\nx-final: two\r\n\r\n";

    let mut decoder = Http1ConnectionDecoder::response(
        "GET",
        kimojio_fsm_http::HttpLimits::new()
            .set_max_header_bytes(1024)
            .set_max_body_bytes(1024),
    );
    let mut scratch = Http1HeaderScratch::new(8);
    let (_, allocations) = measure(|| {
        scratch.with_input(INFORMATIONAL, |input, headers| {
            assert!(matches!(
                decoder.next_event(input, headers).unwrap(),
                Http1ConnectionEvent::Head {
                    informational: true,
                    ..
                }
            ));
        });
        scratch.with_input(FINAL, |input, headers| {
            assert!(matches!(
                decoder.next_event(input, headers).unwrap(),
                Http1ConnectionEvent::Head {
                    informational: false,
                    ..
                }
            ));
        });
    });

    assert_eq!(allocations, 0);
}

#[test]
fn http1_decoder_reuse_does_not_allocate() {
    const REQUEST: &[u8] = b"GET / HTTP/1.1\r\nhost: example.test\r\n\r\n";
    let mut decoder = Http1ConnectionDecoder::request(kimojio_fsm_http::HttpLimits::new());
    let mut scratch = Http1HeaderScratch::new(4);
    scratch.with_input(REQUEST, |input, headers| {
        assert!(matches!(
            decoder.next_event(input, headers).unwrap(),
            Http1ConnectionEvent::Head { .. }
        ));
    });
    scratch.with_input(&[], |input, headers| {
        assert_eq!(
            decoder.next_event(input, headers).unwrap(),
            Http1ConnectionEvent::Complete
        );
    });

    let (_, allocations) = measure(|| {
        decoder.begin_next_message().unwrap();
        scratch.with_input(REQUEST, |input, headers| {
            assert!(matches!(
                decoder.next_event(input, headers).unwrap(),
                Http1ConnectionEvent::Head { .. }
            ));
        });
    });

    assert_eq!(allocations, 0);
}

#[test]
fn borrowed_data_event_avoids_the_owned_event_allocation() {
    let mut server = H2Server::default();
    let mut client = H2Client::default();
    let preface = client.connection_preface();
    server.accept_event(&preface).unwrap();
    let (_, headers) = client
        .open_stream("POST", "https", "example.test", "/upload", &[], false)
        .unwrap();
    let queued = client.next_outbound_block().unwrap();
    assert_eq!(queued.commit(), headers);
    let header_bytes = queued.bytes().to_vec();
    client.acknowledge_outbound_block(headers).unwrap();
    server.accept_event(&header_bytes).unwrap();

    let mut data = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Data,
        flags: 0,
        stream_id: 1,
        payload: b"borrowed-payload".to_vec(),
    }
    .encode(&mut data);

    let ((event, consumed, output), borrowed_allocations) =
        measure(|| server.accept_event_ref(&data).unwrap());
    assert_eq!(consumed, data.len());
    assert!(output.is_empty());
    let Some(H2StreamEvent::Data { payload, .. }) = event else {
        panic!("expected borrowed DATA event");
    };
    assert_eq!(payload.as_ptr(), data[9..].as_ptr());
    assert_eq!(borrowed_allocations, 0);

    let ((event, consumed, output), owned_allocations) =
        measure(|| server.accept_event(&data).unwrap());
    assert_eq!(consumed, data.len());
    assert!(output.is_empty());
    assert!(matches!(event, Some(H2StreamEvent::Data { .. })));
    assert_eq!(owned_allocations, 1);

    let mut client = H2Client::default();
    client.connection_preface();
    let mut settings = Vec::new();
    H2Frame {
        frame_type: H2FrameType::Settings,
        flags: 0,
        stream_id: 0,
        payload: Vec::new(),
    }
    .encode(&mut settings);
    client.accept(&settings).unwrap();
    let (stream_id, _) = client
        .open_stream("POST", "https", "example.test", "/download", &[], false)
        .unwrap();
    let mut response_server = H2Server::default();
    let response_commit = response_server
        .response_headers_frame(stream_id, 200, &[], false)
        .unwrap();
    let queued = response_server.next_outbound_block().unwrap();
    assert_eq!(queued.commit(), response_commit);
    let response_headers = queued.bytes().to_vec();
    response_server
        .acknowledge_outbound_block(response_commit)
        .unwrap();
    client.accept(&response_headers).unwrap();

    let ((event, consumed, output), borrowed_allocations) =
        measure(|| client.accept_ref(&data).unwrap());
    assert_eq!(consumed, data.len());
    assert!(output.is_empty());
    let Some(kimojio_fsm_http::H2ClientEvent::Data { payload, .. }) = event else {
        panic!("expected borrowed client DATA event");
    };
    assert_eq!(payload.as_ptr(), data[9..].as_ptr());
    assert_eq!(borrowed_allocations, 0);

    let ((event, consumed, output), owned_allocations) = measure(|| client.accept(&data).unwrap());
    assert_eq!(consumed, data.len());
    assert!(output.is_empty());
    assert!(matches!(
        event,
        Some(kimojio_fsm_http::H2ClientEvent::Data { .. })
    ));
    assert_eq!(owned_allocations, 1);
}

#[test]
fn adaptive_receive_window_growth_does_not_allocate() {
    let start = Instant::now();
    let mut window = H2ReceiveWindow::with_adaptive_growth(
        64 * 1024,
        4 * 1024 * 1024,
        Duration::from_millis(100),
        start,
    )
    .unwrap();

    let (_, allocations) = measure(|| {
        for elapsed_ms in [10, 20] {
            let now = start + Duration::from_millis(elapsed_ms);
            window.receive_data_at(64 * 1024, now).unwrap();
            window.consume_data_at(64 * 1024, now).unwrap();
        }
    });

    assert_eq!(window.limit(), 128 * 1024);
    assert_eq!(allocations, 0);
}

#[test]
fn fair_scheduler_selection_retirement_and_cap_churn_do_not_allocate_after_warmup() {
    const STREAM_CAP: u32 = 100;
    const GENERATIONS: u32 = 1_000;
    const WARM_GENERATIONS: u32 = 2;
    let mut scheduler = H2FairStreamScheduler::with_capacity(STREAM_CAP as usize);
    for generation in 0..WARM_GENERATIONS {
        let base = 1_u32.saturating_add(generation.saturating_mul(STREAM_CAP * 2));
        for index in 0..STREAM_CAP {
            scheduler.register(base.saturating_add(index.saturating_mul(2)));
        }
        for index in 0..STREAM_CAP {
            scheduler.remove(base.saturating_add(index.saturating_mul(2)));
        }
    }
    let live_base = 1_u32.saturating_add(WARM_GENERATIONS.saturating_mul(STREAM_CAP * 2));
    for index in 0..STREAM_CAP {
        scheduler.register(live_base.saturating_add(index.saturating_mul(2)));
    }

    let (_, allocations) = measure(|| {
        for _ in 0..100_000 {
            assert!(scheduler.next_ready(|_| true).is_some());
        }
        for index in 0..STREAM_CAP {
            let stream_id = live_base.saturating_add(index.saturating_mul(2));
            scheduler.remove(stream_id);
        }
        for generation in 0..GENERATIONS {
            let base = live_base
                .saturating_add(STREAM_CAP * 2)
                .saturating_add(generation.saturating_mul(STREAM_CAP * 2));
            for index in 0..STREAM_CAP {
                scheduler.register(base.saturating_add(index.saturating_mul(2)));
            }
            for _ in 0..STREAM_CAP {
                let stream_id = scheduler
                    .next_ready(|_| true)
                    .expect("all churn streams are ready");
                scheduler.mark_drained(stream_id);
            }
            for index in 0..STREAM_CAP {
                scheduler.remove(base.saturating_add(index.saturating_mul(2)));
            }
        }
    });

    let diagnostics = scheduler.diagnostics();
    assert_eq!(allocations, 0);
    assert_eq!(
        diagnostics.observed_streams,
        (STREAM_CAP * (WARM_GENERATIONS + 1 + GENERATIONS)) as usize
    );
    assert_eq!(diagnostics.tracked_streams, 0);
    assert_eq!(diagnostics.queued_streams, 0);
    assert_eq!(diagnostics.max_scan_depth, 1);
    assert_eq!(diagnostics.skipped_streams, 0);
}

#[test]
fn fair_scheduler_first_cap_generation_uses_reserved_storage() {
    const STREAM_CAP: u32 = 100;
    let mut scheduler = H2FairStreamScheduler::with_capacity(STREAM_CAP as usize);

    let (_, allocations) = measure(|| {
        for index in 0..STREAM_CAP {
            scheduler.register(index.saturating_mul(2).saturating_add(1));
        }
        for index in 0..STREAM_CAP {
            scheduler.remove(index.saturating_mul(2).saturating_add(1));
        }
    });

    assert_eq!(allocations, 0);
}

/// Outbound streaming emits one chunk size line per chunk, so building it must
/// not allocate. It previously formatted into a throwaway `Vec` that was copied
/// into the output buffer and dropped, which nothing here would have caught.
#[test]
fn chunked_body_prefix_does_not_allocate_per_chunk() {
    let mut output = Vec::with_capacity(256);
    // Warm the buffer so growth is not attributed to the prefix itself.
    Http1Server::push_chunked_body_prefix(&mut output, 0x1000);
    output.clear();

    let (_, allocations) = measure(|| {
        for len in [1_usize, 0xf, 0x10, 0xffff, 0] {
            Http1Server::push_chunked_body_prefix(&mut output, len);
        }
    });

    assert_eq!(allocations, 0);
    assert_eq!(output, b"1\r\nf\r\n10\r\nffff\r\n".to_vec());
}
