#[path = "base/kimojio-fsm-http1/tests/support/mod.rs"]
mod support;
use kimojio_fsm_http1::*;
use support::*;

#[test]
fn reused_server_starts_head_timeout_without_an_idle_timeout() {
    let mut machine = server(Config {
        head_timeout_ns: Some(100),
        idle_timeout_ns: None,
        ..config()
    });
    let Some(Event::Read(read)) = next_server(&mut machine) else {
        panic!("expected first read")
    };
    machine
        .complete_read(fill(read, b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"))
        .unwrap();
    let Some(Event::Request(exchange, _)) = next_server(&mut machine) else {
        panic!("expected request")
    };
    machine
        .respond(exchange, Response::new(200, "OK", &[], BodyLength::Empty))
        .unwrap();
    loop {
        match next_server(&mut machine).unwrap() {
            Event::Write(write) => machine.complete_write(finish_write(write)).unwrap(),
            Event::Incoming(_) => {}
            Event::Finished(finished) => {
                assert!(finished.reusable);
                break;
            }
            other => panic!("unexpected event {other:?}"),
        }
    }
    let Some(Event::Read(read)) = next_server(&mut machine) else {
        panic!("expected reused read")
    };
    machine.observe_time(Tick(10)).unwrap();
    machine.complete_read(fill(read, b"G")).unwrap();
    let mut deadlines = Vec::new();
    while let Some(event) = machine.next(&mut Capture) {
        match event {
            Event::Deadline(deadline) => deadlines.push(deadline.map(|d| d.at)),
            Event::Read(_) => break,
            other => panic!("unexpected event {other:?}"),
        }
    }
    assert_eq!(deadlines, [Some(Tick(110))]);
}
