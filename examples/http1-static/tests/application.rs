use http1_static::app::*;

#[derive(Debug)]
enum Action {
    Open(Open),
    Stat(Stat),
    Read(Read),
    Close(Close),
    Respond(Response<u64>),
    Body(Body<u64>),
    Failed,
    Settled,
}

struct Yield;
impl Ports<u64> for Yield {
    type Output = Action;
    fn open(&mut self, op: Open) -> Option<Action> {
        Some(Action::Open(op))
    }
    fn stat(&mut self, op: Stat) -> Option<Action> {
        Some(Action::Stat(op))
    }
    fn read(&mut self, op: Read) -> Option<Action> {
        Some(Action::Read(op))
    }
    fn close(&mut self, op: Close) -> Option<Action> {
        Some(Action::Close(op))
    }
    fn respond(&mut self, response: Response<u64>) -> Option<Action> {
        Some(Action::Respond(response))
    }
    fn body(&mut self, body: Body<u64>) -> Option<Action> {
        Some(Action::Body(body))
    }
    fn source_failed(&mut self, _: u64) -> Option<Action> {
        Some(Action::Failed)
    }
    fn settled(&mut self, _: u64) -> Option<Action> {
        Some(Action::Settled)
    }
}

fn opened(method: &[u8], length: u64) -> App<u64> {
    let mut app = App::new(1);
    assert!(app.request(10, method, b"/file"));
    let Action::Open(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!(op.path, b"file");
    assert!(app.next(&mut Yield).is_none());
    app.complete(Completion::Open {
        id: op.id,
        result: Ok(File(3)),
    })
    .unwrap();
    let Action::Stat(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    app.complete(Completion::Stat {
        id: op.id,
        result: Ok(Metadata {
            length,
            regular: true,
        }),
    })
    .unwrap();
    let Action::Respond(response) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!(response.status, 200);
    assert_eq!(response.length, length);
    assert_eq!(response.head, method == b"HEAD");
    app
}

fn close(app: &mut App<u64>) {
    let Action::Close(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!(op.file, File(3));
    assert!(app.next(&mut Yield).is_none());
    app.complete(Completion::Close {
        id: op.id,
        result: Ok(()),
    })
    .unwrap();
}

#[test]
fn traversal_and_encoding_policy() {
    for target in [
        b"/../secret".as_slice(),
        b"/%2e%2e/secret",
        b"/a/%2E./secret",
        b"/./file",
    ] {
        assert_eq!(relative_path(target), Err(403));
    }
    for target in [
        b"/%00".as_slice(),
        b"/x%G0",
        b"/x%",
        b"/back\\slash",
        b"http://host/file",
    ] {
        assert_eq!(relative_path(target), Err(400));
    }
    assert_eq!(relative_path(b"/?x=1").unwrap(), b"index.html");
    assert_eq!(
        relative_path(b"/hello%20there?q=/../").unwrap(),
        b"hello there"
    );
    assert_eq!(relative_path(b"/%252e%252e/file").unwrap(), b"%2e%2e/file");
    assert_eq!(relative_path(b"/%2fetc/passwd"), Err(404));
    assert_eq!(relative_path(b"/dir/"), Err(404));
}

#[test]
fn get_is_demand_driven_and_reuses_bounded_buffer() {
    let mut app = opened(b"GET", 7);
    assert!(app.next(&mut Yield).is_none());
    assert!(app.demand(10, 4));
    let Action::Read(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!((op.offset, op.limit), (0, 4));
    let pointer = op.buffer.as_ptr();
    app.complete(Completion::Read {
        id: op.id,
        buffer: op.buffer,
        result: Ok(4),
    })
    .unwrap();
    let Action::Body(body) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!(body.buffer.as_ptr(), pointer);
    assert_eq!(body.range, 0..4);
    assert!(!body.end);
    assert!(app.next(&mut Yield).is_none());
    app.body_sent(10, body.buffer, 4).unwrap();
    assert!(app.next(&mut Yield).is_none());
    app.demand(10, CHUNK_SIZE);
    let Action::Read(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!((op.offset, op.limit), (4, 3));
    assert_eq!(op.buffer.as_ptr(), pointer);
    app.complete(Completion::Read {
        id: op.id,
        buffer: op.buffer,
        result: Ok(3),
    })
    .unwrap();
    let Action::Body(body) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert!(body.end);
    app.body_sent(10, body.buffer, 3).unwrap();
    close(&mut app);
    assert!(!app.is_idle());
    app.exchange_finished(10);
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
    assert!(app.is_idle());
    assert!(app.request(11, b"GET", b"/next"));
}

#[test]
fn head_and_empty_get_never_read() {
    for (method, size) in [(b"HEAD".as_slice(), 100), (b"GET".as_slice(), 0)] {
        let mut app = opened(method, size);
        assert!(!app.demand(10, 123));
        close(&mut app);
        app.exchange_finished(10);
        assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
    }
}

#[test]
fn early_eof_and_read_errors_abort_and_close() {
    for result in [Ok(0), Err(FileError::Other)] {
        let mut app = opened(b"GET", 9);
        app.demand(10, CHUNK_SIZE);
        let Action::Read(op) = app.next(&mut Yield).unwrap() else {
            panic!()
        };
        app.complete(Completion::Read {
            id: op.id,
            buffer: op.buffer,
            result,
        })
        .unwrap();
        assert!(matches!(app.next(&mut Yield), Some(Action::Failed)));
        close(&mut app);
        app.exchange_finished(10);
        assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
    }
}

#[test]
fn zero_http_body_capacity_fails_without_issuing_a_zero_length_read() {
    let mut app = opened(b"GET", 9);
    assert!(app.demand(10, 0));
    assert!(matches!(app.next(&mut Yield), Some(Action::Failed)));
    close(&mut app);
    app.exchange_finished(10);
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
}

#[test]
fn source_finished_closes_file_but_waits_for_the_outstanding_http_buffer() {
    let mut app = opened(b"GET", 4);
    app.demand(10, 4);
    let Action::Read(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    app.complete(Completion::Read {
        id: op.id,
        buffer: op.buffer,
        result: Ok(4),
    })
    .unwrap();
    let Action::Body(body) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert!(app.source_finished(10));
    close(&mut app);
    app.exchange_finished(10);
    assert!(!app.is_idle());
    assert!(app.next(&mut Yield).is_none());
    app.body_sent(10, body.buffer, 4).unwrap();
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
    assert!(app.is_idle());
}

#[test]
fn partial_http_body_failure_settles_returned_buffer_and_file() {
    let mut app = opened(b"GET", 10);
    app.demand(10, 10);
    let Action::Read(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    app.complete(Completion::Read {
        id: op.id,
        buffer: op.buffer,
        result: Ok(10),
    })
    .unwrap();
    let Action::Body(body) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    app.body_sent(10, body.buffer, 2).unwrap();
    assert!(matches!(app.next(&mut Yield), Some(Action::Failed)));
    close(&mut app);
    app.exchange_finished(10);
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
}

#[test]
fn abort_waits_for_late_open_and_closes_it_once() {
    let mut app = App::new(1);
    app.request(10, b"GET", b"/file");
    let Action::Open(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    app.abort();
    assert!(app.next(&mut Yield).is_none());
    app.complete(Completion::Open {
        id: op.id,
        result: Ok(File(3)),
    })
    .unwrap();
    close(&mut app);
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
    assert!(app.next(&mut Yield).is_none());
}

#[test]
fn abort_during_read_returns_buffer_before_close() {
    let mut app = opened(b"GET", 100);
    app.demand(10, 10);
    let Action::Read(op) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    app.abort();
    assert!(app.next(&mut Yield).is_none());
    app.complete(Completion::Read {
        id: op.id,
        buffer: op.buffer,
        result: Ok(10),
    })
    .unwrap();
    close(&mut app);
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
}

#[test]
fn rejected_completions_preserve_resources_and_valid_state() {
    let mut a = App::new(1);
    let mut b = App::new(2);
    a.request(10, b"GET", b"/file");
    b.request(20, b"GET", b"/file");
    let Action::Open(a_op) = a.next(&mut Yield).unwrap() else {
        panic!()
    };
    let Action::Open(b_op) = b.next(&mut Yield).unwrap() else {
        panic!()
    };
    let rejected = a
        .complete(Completion::Open {
            id: b_op.id,
            result: Ok(File(5)),
        })
        .unwrap_err();
    assert!(matches!(
        rejected,
        Completion::Open {
            result: Ok(File(5)),
            ..
        }
    ));
    a.complete(Completion::Open {
        id: a_op.id,
        result: Ok(File(3)),
    })
    .unwrap();
    assert!(
        a.complete(Completion::Open {
            id: a_op.id,
            result: Ok(File(6))
        })
        .is_err()
    );
    assert!(matches!(a.next(&mut Yield), Some(Action::Stat(_))));
}

#[test]
fn missing_and_nonregular_files_and_method_rejection() {
    for (error, expected) in [
        (FileError::Missing, 404),
        (FileError::Forbidden, 403),
        (FileError::Other, 500),
    ] {
        let mut app = App::new(1);
        app.request(10, b"GET", b"/missing");
        let Action::Open(op) = app.next(&mut Yield).unwrap() else {
            panic!()
        };
        app.complete(Completion::Open {
            id: op.id,
            result: Err(error),
        })
        .unwrap();
        let Action::Respond(response) = app.next(&mut Yield).unwrap() else {
            panic!()
        };
        assert_eq!((response.status, response.length), (expected, 0));
        app.exchange_finished(10);
        assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
    }
    let mut app = App::new(1);
    app.request(10, b"POST", b"/file");
    let Action::Respond(response) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    assert_eq!(response.status, 405);
}

#[derive(Default)]
struct Record {
    yield_callbacks: bool,
    actions: Vec<Action>,
}

impl Record {
    fn accept(&mut self, action: Action) -> Option<()> {
        self.actions.push(action);
        self.yield_callbacks.then_some(())
    }
}

impl Ports<u64> for Record {
    type Output = ();
    fn open(&mut self, op: Open) -> Option<()> {
        self.accept(Action::Open(op))
    }
    fn stat(&mut self, op: Stat) -> Option<()> {
        self.accept(Action::Stat(op))
    }
    fn read(&mut self, op: Read) -> Option<()> {
        self.accept(Action::Read(op))
    }
    fn close(&mut self, op: Close) -> Option<()> {
        self.accept(Action::Close(op))
    }
    fn respond(&mut self, response: Response<u64>) -> Option<()> {
        self.accept(Action::Respond(response))
    }
    fn body(&mut self, body: Body<u64>) -> Option<()> {
        self.accept(Action::Body(body))
    }
    fn source_failed(&mut self, _: u64) -> Option<()> {
        self.accept(Action::Failed)
    }
    fn settled(&mut self, _: u64) -> Option<()> {
        self.accept(Action::Settled)
    }
}

fn drain(app: &mut App<u64>, ports: &mut Record) {
    for _ in 0..8 {
        if app.next(ports).is_none() {
            return;
        }
    }
    panic!("application did not block");
}

#[test]
fn exact_sequences_join_exchange_file_close_and_http_lease_in_every_order() {
    // 0 = exchange completion, 1 = HTTP lease return, 2 = file close CQE.
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        for accepted in [0, 2, 4] {
            for close_fails in [false, true] {
                for yield_callbacks in [false, true] {
                    let mut app = opened(b"GET", 4);
                    app.demand(10, 4);
                    let Action::Read(read) = app.next(&mut Yield).unwrap() else {
                        panic!()
                    };
                    let pointer = read.buffer.as_ptr();
                    app.complete(Completion::Read {
                        id: read.id,
                        buffer: read.buffer,
                        result: Ok(4),
                    })
                    .unwrap();
                    let Action::Body(body) = app.next(&mut Yield).unwrap() else {
                        panic!()
                    };
                    let mut buffer = Some(body.buffer);
                    assert_eq!(buffer.as_ref().unwrap().as_ptr(), pointer);
                    app.source_finished(10);
                    let mut ports = Record {
                        yield_callbacks,
                        ..Record::default()
                    };
                    drain(&mut app, &mut ports);
                    assert_eq!(ports.actions.len(), 1);
                    let Action::Close(close) = ports.actions.pop().unwrap() else {
                        panic!()
                    };
                    let mut exchange_done = false;
                    for (index, event) in order.into_iter().enumerate() {
                        match event {
                            0 => {
                                app.exchange_finished(10);
                                exchange_done = true;
                            }
                            1 => app.body_sent(10, buffer.take().unwrap(), accepted).unwrap(),
                            2 => app
                                .complete(Completion::Close {
                                    id: close.id,
                                    result: if close_fails {
                                        Err(FileError::Other)
                                    } else {
                                        Ok(())
                                    },
                                })
                                .unwrap(),
                            _ => unreachable!(),
                        }
                        drain(&mut app, &mut ports);
                        // Independent contract: errors notify only a live
                        // exchange; settlement requires all three returns.
                        let failure = !exchange_done
                            && ((event == 1 && accepted != 4) || (event == 2 && close_fails));
                        let settled = index == 2;
                        assert_eq!(
                            ports.actions.len(),
                            usize::from(failure) + usize::from(settled)
                        );
                        if failure {
                            assert!(matches!(ports.actions.remove(0), Action::Failed));
                        }
                        if settled {
                            assert!(matches!(ports.actions.remove(0), Action::Settled));
                        }
                        assert_eq!(app.is_idle(), settled);
                    }
                    assert!(app.request(11, b"GET", b"/next"));
                    let Action::Open(open) = app.next(&mut Yield).unwrap() else {
                        panic!()
                    };
                    assert_ne!(
                        open.id, close.id,
                        "operation identities cannot reuse live generations"
                    );
                }
            }
        }
    }
}

#[test]
fn continuing_callbacks_preserve_response_before_close_and_failure_before_close() {
    for yield_callbacks in [false, true] {
        let mut app = opened(b"GET", 4);
        app.demand(10, 4);
        let Action::Read(read) = app.next(&mut Yield).unwrap() else {
            panic!()
        };
        app.complete(Completion::Read {
            id: read.id,
            buffer: read.buffer,
            result: Ok(0),
        })
        .unwrap();
        let mut ports = Record {
            yield_callbacks,
            ..Record::default()
        };
        drain(&mut app, &mut ports);
        assert!(matches!(
            ports.actions.as_slice(),
            [Action::Failed, Action::Close(_)]
        ));

        let mut app = App::new(1);
        app.request(10, b"HEAD", b"/file");
        let Action::Open(open) = app.next(&mut Yield).unwrap() else {
            panic!()
        };
        app.complete(Completion::Open {
            id: open.id,
            result: Ok(File(3)),
        })
        .unwrap();
        let Action::Stat(stat) = app.next(&mut Yield).unwrap() else {
            panic!()
        };
        app.complete(Completion::Stat {
            id: stat.id,
            result: Ok(Metadata {
                length: 9,
                regular: true,
            }),
        })
        .unwrap();
        ports.actions.clear();
        drain(&mut app, &mut ports);
        assert!(matches!(
            ports.actions.as_slice(),
            [
                Action::Respond(Response {
                    length: 9,
                    head: true,
                    ..
                }),
                Action::Close(_)
            ]
        ));
    }
}

#[test]
fn invalid_read_count_and_body_return_preserve_the_original_lease() {
    let mut app = opened(b"GET", 4);
    app.demand(10, 4);
    let Action::Read(read) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    let pointer = read.buffer.as_ptr();
    let rejected = app
        .complete(Completion::Read {
            id: read.id,
            buffer: read.buffer,
            result: Ok(5),
        })
        .unwrap_err();
    assert!(app.next(&mut Yield).is_none());
    let Completion::Read { id, buffer, .. } = rejected else {
        panic!()
    };
    assert_eq!(buffer.as_ptr(), pointer);
    app.complete(Completion::Read {
        id,
        buffer,
        result: Ok(4),
    })
    .unwrap();
    let Action::Body(body) = app.next(&mut Yield).unwrap() else {
        panic!()
    };
    let buffer = app.body_sent(10, body.buffer, 5).unwrap_err();
    assert_eq!(buffer.as_ptr(), pointer);
    assert!(app.next(&mut Yield).is_none());
    app.body_sent(10, buffer, 4).unwrap();
    close(&mut app);
    app.exchange_finished(10);
    assert!(matches!(app.next(&mut Yield), Some(Action::Settled)));
}
