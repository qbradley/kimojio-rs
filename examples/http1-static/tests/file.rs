use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use http1_static::{app, file, ring};
use rustix::fs::{Mode, OFlags};

enum Action {
    File(file::Request),
    Response(app::Response<u64>),
    Body(app::Body<u64>),
    Failed,
    Settled,
}

struct Ports;
impl app::Ports<u64> for Ports {
    type Output = Action;
    fn open(&mut self, op: app::Open) -> Option<Action> {
        Some(Action::File(file::Request::Open(op)))
    }
    fn stat(&mut self, op: app::Stat) -> Option<Action> {
        Some(Action::File(file::Request::Stat(op)))
    }
    fn read(&mut self, op: app::Read) -> Option<Action> {
        Some(Action::File(file::Request::Read(op)))
    }
    fn close(&mut self, op: app::Close) -> Option<Action> {
        Some(Action::File(file::Request::Close(op)))
    }
    fn respond(&mut self, response: app::Response<u64>) -> Option<Action> {
        Some(Action::Response(response))
    }
    fn body(&mut self, body: app::Body<u64>) -> Option<Action> {
        Some(Action::Body(body))
    }
    fn source_failed(&mut self, _: u64) -> Option<Action> {
        Some(Action::Failed)
    }
    fn settled(&mut self, _: u64) -> Option<Action> {
        Some(Action::Settled)
    }
}

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .join(format!(
                "http1-file-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        std::fs::create_dir_all(path.join("root")).unwrap();
        std::fs::write(path.join("outside"), b"secret").unwrap();
        std::fs::write(path.join("root/index.html"), b"hello from actual io_uring").unwrap();
        std::os::unix::fs::symlink("../outside", path.join("root/escape")).unwrap();
        Self(path)
    }

    fn request(&self, method: &[u8], target: &[u8], truncate: bool) -> (u16, Vec<u8>, bool) {
        let root = Rc::new(
            rustix::fs::open(
                self.0.join("root"),
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .unwrap(),
        );
        let mut executor = ring::Ring::new(8).expect("real io_uring required");
        let mut machine = app::App::new(1);
        machine.request(1, method, target);
        let mut descriptor = None;
        let mut key = 0;
        let mut status = 0;
        let mut bytes = Vec::new();
        let mut failed = false;
        for _ in 0..100 {
            match machine.next(&mut Ports) {
                Some(Action::File(request)) => {
                    let operation = match request {
                        file::Request::Open(op) => file::FileOperation::open(1, op, root.clone()),
                        file::Request::Stat(op) => {
                            assert_eq!(op.file.0, key);
                            file::FileOperation::stat(1, op, descriptor.take().unwrap())
                        }
                        file::Request::Read(op) => {
                            assert_eq!(op.file.0, key);
                            file::FileOperation::read(1, op, descriptor.take().unwrap())
                        }
                        file::Request::Close(op) => {
                            assert_eq!(op.file.0, key);
                            file::FileOperation::close(1, op, descriptor.take().unwrap())
                        }
                    };
                    let token = executor.submit(operation).ok().unwrap();
                    let event = executor.poll(None).unwrap().pop().unwrap();
                    let ring::Event::Completed { operation, .. } = event else {
                        panic!()
                    };
                    let finished = operation.finish(token);
                    if let app::Completion::Open {
                        result: Ok(file), ..
                    } = &finished.completion
                    {
                        key = file.0;
                    }
                    descriptor = finished.descriptor;
                    machine.complete(finished.completion).unwrap();
                }
                Some(Action::Response(response)) => {
                    status = response.status;
                    if truncate {
                        std::fs::write(self.0.join("root/index.html"), []).unwrap();
                    }
                    if response.head || response.length == 0 {
                        machine.exchange_finished(1);
                    } else {
                        machine.demand(1, 7);
                    }
                }
                Some(Action::Body(body)) => {
                    let count = body.range.len();
                    bytes.extend_from_slice(&body.buffer[body.range]);
                    machine.body_sent(1, body.buffer, count).unwrap();
                    if body.end {
                        machine.exchange_finished(1);
                    } else {
                        machine.demand(1, 7);
                    }
                }
                Some(Action::Failed) => {
                    failed = true;
                    machine.exchange_finished(1);
                }
                Some(Action::Settled) => {
                    assert!(descriptor.is_none());
                    assert!(executor.is_empty());
                    return (status, bytes, failed);
                }
                None => panic!("unexpected quiescence"),
            }
        }
        panic!("application failed to settle");
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn real_open_stat_chunked_reads_close_and_head() {
    let fixture = Fixture::new();
    let (status, body, failed) = fixture.request(b"GET", b"/", false);
    assert_eq!(status, 200);
    assert_eq!(body, b"hello from actual io_uring");
    assert!(!failed);
    assert_eq!(
        fixture.request(b"HEAD", b"/", false),
        (200, Vec::new(), false)
    );
}

#[test]
fn real_secure_open_rejects_symlinks_missing_and_directories() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture.request(b"GET", b"/escape", false),
        (403, Vec::new(), false)
    );
    assert_eq!(
        fixture.request(b"GET", b"/missing", false),
        (404, Vec::new(), false)
    );
    std::fs::create_dir(fixture.0.join("root/dir")).unwrap();
    assert_eq!(
        fixture.request(b"GET", b"/dir", false),
        (404, Vec::new(), false)
    );
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        fixture.0.join("root/fifo"),
        Mode::RUSR | Mode::WUSR,
    )
    .unwrap();
    assert_eq!(
        fixture.request(b"GET", b"/fifo", false),
        (404, Vec::new(), false)
    );
}

#[test]
fn real_file_truncation_after_stat_fails_body_and_settles() {
    let fixture = Fixture::new();
    assert_eq!(fixture.request(b"GET", b"/", true), (200, Vec::new(), true));
}
