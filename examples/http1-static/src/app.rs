//! Pure, bounded static-file application. File handles are opaque executor keys.
use std::ops::Range;

pub type Buffer = Box<[u8]>;
pub const MAX_PATH: usize = 4096;
pub const CHUNK_SIZE: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct File(pub u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Id {
    owner: u64,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FileError {
    Missing,
    Forbidden,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Metadata {
    pub length: u64,
    pub regular: bool,
}

#[derive(Debug)]
pub struct Open {
    pub id: Id,
    /// Decoded, relative UTF-8-independent path; never contains NUL.
    pub path: Vec<u8>,
}

#[derive(Debug)]
pub struct Stat {
    pub id: Id,
    pub file: File,
}

#[derive(Debug)]
pub struct Read {
    pub id: Id,
    pub file: File,
    pub offset: u64,
    pub buffer: Buffer,
    pub limit: usize,
}

#[derive(Debug)]
pub struct Close {
    pub id: Id,
    pub file: File,
}

#[derive(Debug)]
pub enum Completion {
    Open {
        id: Id,
        result: Result<File, FileError>,
    },
    Stat {
        id: Id,
        result: Result<Metadata, FileError>,
    },
    Read {
        id: Id,
        buffer: Buffer,
        result: Result<usize, FileError>,
    },
    Close {
        id: Id,
        result: Result<(), FileError>,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Response<E> {
    pub exchange: E,
    pub status: u16,
    pub length: u64,
    pub head: bool,
}

#[derive(Debug)]
pub struct Body<E> {
    pub exchange: E,
    pub buffer: Buffer,
    pub range: Range<usize>,
    pub end: bool,
}

/// Every callback accepts its operation exactly once. `None` means continue,
/// not rejection or completion. File operations require a matching completion.
pub trait Ports<E> {
    type Output;
    fn open(&mut self, op: Open) -> Option<Self::Output>;
    fn stat(&mut self, op: Stat) -> Option<Self::Output>;
    fn read(&mut self, op: Read) -> Option<Self::Output>;
    fn close(&mut self, op: Close) -> Option<Self::Output>;
    fn respond(&mut self, response: Response<E>) -> Option<Self::Output>;
    fn body(&mut self, body: Body<E>) -> Option<Self::Output>;
    fn source_failed(&mut self, exchange: E) -> Option<Self::Output>;
    fn settled(&mut self, exchange: E) -> Option<Self::Output>;
}

#[derive(Clone, Copy)]
enum Lifecycle<E> {
    Idle,
    Producing { exchange: E, head: bool },
    Draining { exchange: E, finished: bool },
}

enum FileState {
    Absent,
    Planned(Vec<u8>),
    Opening { id: Id },
    Opened(File),
    Ready(File),
    Stating { id: Id, file: File },
    Reading { id: Id, file: File, limit: usize },
    Closing { id: Id },
}

enum Payload {
    Local(Buffer),
    Ready {
        buffer: Buffer,
        count: usize,
        end: bool,
    },
    Reading,
    Http {
        count: usize,
    },
}

#[derive(Clone, Copy)]
enum Transition {
    Fail,
    Respond,
    Open,
    Stat,
    Read,
    Body,
    Close,
    Settle,
}

/// One application exchange and one file operation can be live at a time.
/// The HTTP connector retains a returned body until `body_sent` returns it.
pub struct App<E> {
    owner: u64,
    generation: u64,
    lifecycle: Lifecycle<E>,
    file: FileState,
    payload: Payload,
    response: Option<Response<E>>,
    demand: Option<usize>,
    offset: u64,
    remaining: u64,
    failed: bool,
}

impl<E: Copy + Eq> App<E> {
    pub fn new(owner: u64) -> Self {
        Self {
            owner,
            generation: 0,
            lifecycle: Lifecycle::Idle,
            file: FileState::Absent,
            payload: Payload::Local(vec![0; CHUNK_SIZE].into_boxed_slice()),
            response: None,
            demand: None,
            offset: 0,
            remaining: 0,
            failed: false,
        }
    }

    pub fn is_idle(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Idle)
    }

    fn exchange(&self) -> Option<E> {
        match self.lifecycle {
            Lifecycle::Idle => None,
            Lifecycle::Producing { exchange, .. } | Lifecycle::Draining { exchange, .. } => {
                Some(exchange)
            }
        }
    }

    fn producing(&self) -> bool {
        matches!(self.lifecycle, Lifecycle::Producing { .. })
    }

    fn finished(&self) -> bool {
        matches!(
            self.lifecycle,
            Lifecycle::Idle | Lifecycle::Draining { finished: true, .. }
        )
    }

    fn check(&self) {
        debug_assert_eq!(
            matches!(self.file, FileState::Reading { .. }),
            matches!(self.payload, Payload::Reading)
        );
        if self.is_idle() {
            debug_assert!(matches!(self.file, FileState::Absent));
            debug_assert!(matches!(self.payload, Payload::Local(_)));
            debug_assert!(self.response.is_none() && !self.failed && self.demand.is_none());
        }
        if !self.producing() {
            debug_assert!(!matches!(self.file, FileState::Planned(_)));
            debug_assert!(!matches!(self.payload, Payload::Ready { .. }));
            debug_assert!(self.demand.is_none());
        }
    }

    /// The caller must wait for `settled` before handing over another request.
    pub fn request(&mut self, exchange: E, method: &[u8], target: &[u8]) -> bool {
        if !self.is_idle() {
            return false;
        }
        self.check();
        let head = method == b"HEAD";
        self.lifecycle = Lifecycle::Producing { exchange, head };
        self.offset = 0;
        self.remaining = 0;
        self.demand = None;
        if method != b"GET" && !head {
            self.error_response(405);
        } else {
            match relative_path(target) {
                Ok(path) => self.file = FileState::Planned(path),
                Err(status) => self.error_response(status),
            }
        }
        true
    }

    pub fn demand(&mut self, exchange: E, capacity: usize) -> bool {
        if self.exchange() != Some(exchange) || !self.producing() {
            return false;
        }
        if capacity == 0 {
            self.stop(true);
            return true;
        }
        self.demand = Some(capacity.min(CHUNK_SIZE));
        true
    }

    /// Returns the payload allocation even when the HTTP write was partial.
    /// On rejection, the caller retains the allocation unchanged.
    pub fn body_sent(
        &mut self,
        exchange: E,
        buffer: Buffer,
        accepted: usize,
    ) -> Result<(), Buffer> {
        let Payload::Http { count: expected } = self.payload else {
            return Err(buffer);
        };
        if self.exchange() != Some(exchange) || accepted > expected {
            return Err(buffer);
        }
        self.payload = Payload::Local(buffer);
        self.remaining -= accepted as u64;
        if accepted != expected {
            self.stop(true);
        } else if self.remaining == 0 {
            self.stop(false);
        }
        self.check();
        Ok(())
    }

    /// Stops producing without treating outstanding HTTP writes as complete.
    pub fn source_finished(&mut self, exchange: E) -> bool {
        if self.exchange() != Some(exchange) {
            return false;
        }
        self.stop(false);
        true
    }

    pub fn exchange_finished(&mut self, exchange: E) -> bool {
        if self.exchange() != Some(exchange) {
            return false;
        }
        self.abort();
        true
    }

    pub fn abort(&mut self) {
        self.stop(false);
        if let Lifecycle::Draining { finished, .. } = &mut self.lifecycle {
            *finished = true;
        }
        self.response = None;
        self.check();
    }

    pub(crate) fn terminate(&mut self) {
        self.stop(false);
        self.response = None;
        self.check();
    }

    fn stop(&mut self, failed: bool) {
        if let Lifecycle::Producing { exchange, .. } = self.lifecycle {
            self.lifecycle = Lifecycle::Draining {
                exchange,
                finished: false,
            };
        }
        self.demand = None;
        if matches!(self.file, FileState::Planned(_)) {
            self.file = FileState::Absent;
        }
        if matches!(self.payload, Payload::Ready { .. }) {
            let Payload::Ready { buffer, .. } =
                std::mem::replace(&mut self.payload, Payload::Reading)
            else {
                unreachable!()
            };
            self.payload = Payload::Local(buffer);
        }
        self.failed |= failed && !self.finished();
    }

    fn error_response(&mut self, status: u16) {
        let Lifecycle::Producing { exchange, head } = self.lifecycle else {
            unreachable!("only production creates a response")
        };
        self.response = Some(Response {
            exchange,
            status,
            length: 0,
            head,
        });
        self.stop(false);
    }

    fn issue(&mut self) -> Id {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("operation id exhausted");
        Id {
            owner: self.owner,
            generation: self.generation,
        }
    }

    /// A wrong-owner, stale, wrong-kind or invalid-count completion is returned
    /// intact; in particular successful open descriptors and buffers are not lost.
    pub fn complete(&mut self, completion: Completion) -> Result<(), Completion> {
        self.check();
        let valid = match (&self.file, &completion) {
            (FileState::Opening { id }, Completion::Open { id: received, .. })
            | (FileState::Stating { id, .. }, Completion::Stat { id: received, .. })
            | (FileState::Closing { id }, Completion::Close { id: received, .. }) => id == received,
            (
                FileState::Reading { id, limit, .. },
                Completion::Read {
                    id: received,
                    buffer,
                    result,
                },
            ) => {
                id == received
                    && result
                        .as_ref()
                        .map_or(true, |count| *count <= *limit && *count <= buffer.len())
            }
            _ => false,
        };
        if !valid {
            return Err(completion);
        }
        let original = std::mem::replace(&mut self.file, FileState::Absent);
        match completion {
            Completion::Open { result, .. } => match result {
                Ok(file) => {
                    self.file = FileState::Opened(file);
                }
                Err(error) if self.producing() => self.error_response(status(error)),
                Err(_) => {}
            },
            Completion::Stat { result, .. } => {
                let FileState::Stating { file, .. } = original else {
                    unreachable!()
                };
                self.file = FileState::Ready(file);
                if self.producing() {
                    let Lifecycle::Producing { exchange, head } = self.lifecycle else {
                        unreachable!()
                    };
                    match result {
                        Ok(metadata) if metadata.regular => {
                            self.remaining = metadata.length;
                            self.response = Some(Response {
                                exchange,
                                status: 200,
                                length: metadata.length,
                                head,
                            });
                            if head || metadata.length == 0 {
                                self.stop(false);
                            }
                        }
                        Ok(_) => self.error_response(404),
                        Err(error) => self.error_response(status(error)),
                    }
                }
            }
            Completion::Read { buffer, result, .. } => {
                let FileState::Reading { file, .. } = original else {
                    unreachable!()
                };
                self.file = FileState::Ready(file);
                self.payload = Payload::Local(buffer);
                if self.producing() {
                    match result {
                        Ok(count) if count > 0 => {
                            self.offset += count as u64;
                            let Payload::Local(buffer) =
                                std::mem::replace(&mut self.payload, Payload::Reading)
                            else {
                                unreachable!()
                            };
                            self.payload = Payload::Ready {
                                buffer,
                                count,
                                end: count as u64 == self.remaining,
                            };
                        }
                        _ => self.stop(true),
                    }
                }
            }
            Completion::Close { result, .. } => {
                // Ownership was consumed by close, even when close reports an
                // error. Retrying a Linux close can close a reused descriptor.
                if result.is_err() && !self.finished() {
                    self.failed = true;
                }
            }
        }
        self.check();
        Ok(())
    }

    fn select(&self) -> Option<Transition> {
        match self.lifecycle {
            Lifecycle::Idle => None,
            Lifecycle::Producing { .. } => {
                if self.failed {
                    return Some(Transition::Fail);
                }
                if self.response.is_some() {
                    return Some(Transition::Respond);
                }
                match self.file {
                    FileState::Planned(_) => Some(Transition::Open),
                    FileState::Opened(_) => Some(Transition::Stat),
                    FileState::Ready(_) => match self.payload {
                        Payload::Ready { .. } => Some(Transition::Body),
                        Payload::Local(_) if self.demand.is_some() => Some(Transition::Read),
                        _ => None,
                    },
                    _ => None,
                }
            }
            Lifecycle::Draining { finished, .. } => {
                if self.failed {
                    return Some(Transition::Fail);
                }
                if self.response.is_some() {
                    return Some(Transition::Respond);
                }
                match self.file {
                    FileState::Opened(_) | FileState::Ready(_) => Some(Transition::Close),
                    FileState::Absent if finished && matches!(self.payload, Payload::Local(_)) => {
                        Some(Transition::Settle)
                    }
                    _ => None,
                }
            }
        }
    }

    fn commit<P: Ports<E>>(&mut self, transition: Transition, ports: &mut P) -> Option<P::Output> {
        let exchange = self.exchange().expect("selected live exchange");
        match transition {
            Transition::Fail => {
                self.failed = false;
                ports.source_failed(exchange)
            }
            Transition::Respond => ports.respond(self.response.take().unwrap()),
            Transition::Open => {
                let id = self.issue();
                let FileState::Planned(path) =
                    std::mem::replace(&mut self.file, FileState::Opening { id })
                else {
                    unreachable!()
                };
                ports.open(Open { id, path })
            }
            Transition::Stat => {
                let FileState::Opened(file) = self.file else {
                    unreachable!()
                };
                let id = self.issue();
                self.file = FileState::Stating { id, file };
                ports.stat(Stat { id, file })
            }
            Transition::Read => {
                let FileState::Ready(file) = self.file else {
                    unreachable!()
                };
                let capacity = self.demand.take().unwrap();
                let limit = self.remaining.min(capacity as u64) as usize;
                assert!(limit > 0);
                let id = self.issue();
                let Payload::Local(buffer) = std::mem::replace(&mut self.payload, Payload::Reading)
                else {
                    unreachable!()
                };
                self.file = FileState::Reading { id, file, limit };
                ports.read(Read {
                    id,
                    file,
                    offset: self.offset,
                    buffer,
                    limit,
                })
            }
            Transition::Body => {
                let Payload::Ready { buffer, count, end } =
                    std::mem::replace(&mut self.payload, Payload::Reading)
                else {
                    unreachable!()
                };
                self.payload = Payload::Http { count };
                ports.body(Body {
                    exchange,
                    buffer,
                    range: 0..count,
                    end,
                })
            }
            Transition::Close => {
                let (FileState::Opened(file) | FileState::Ready(file)) = self.file else {
                    unreachable!()
                };
                let id = self.issue();
                self.file = FileState::Closing { id };
                ports.close(Close { id, file })
            }
            Transition::Settle => {
                self.lifecycle = Lifecycle::Idle;
                ports.settled(exchange)
            }
        }
    }

    pub fn next<P: Ports<E>>(&mut self, ports: &mut P) -> Option<P::Output> {
        loop {
            self.check();
            let transition = self.select()?;
            if let Some(output) = self.commit(transition, ports) {
                self.check();
                return Some(output);
            }
        }
    }
}

fn status(error: FileError) -> u16 {
    match error {
        FileError::Missing => 404,
        FileError::Forbidden => 403,
        FileError::Other => 500,
    }
}

/// Decode once and reject traversal before the executor applies openat2's
/// BENEATH|NO_SYMLINKS policy. `/` maps to `index.html`; directories do not.
pub fn relative_path(target: &[u8]) -> Result<Vec<u8>, u16> {
    let path = target
        .split(|byte| *byte == b'?')
        .next()
        .unwrap_or_default();
    if !path.starts_with(b"/") || path.len() > MAX_PATH {
        return Err(400);
    }
    let mut result = Vec::with_capacity(path.len());
    let mut index = 1;
    while index < path.len() {
        let byte = if path[index] == b'%' {
            let high = *path.get(index + 1).ok_or(400_u16)?;
            let low = *path.get(index + 2).ok_or(400_u16)?;
            index += 3;
            hex(high).ok_or(400_u16)? * 16 + hex(low).ok_or(400_u16)?
        } else {
            let byte = path[index];
            index += 1;
            byte
        };
        if byte == 0 || byte == b'\\' || byte < 0x20 || byte == 0x7f || byte == b'#' {
            return Err(400);
        }
        result.push(byte);
    }
    if result.is_empty() {
        return Ok(b"index.html".to_vec());
    }
    if result
        .split(|byte| *byte == b'/')
        .any(|part| part == b".." || part == b".")
    {
        return Err(403);
    }
    if result.starts_with(b"/") || result.ends_with(b"/") {
        return Err(404);
    }
    Ok(result)
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}
