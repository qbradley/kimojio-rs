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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Open,
    Stat,
    Read,
    Close,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Idle,
    Open,
    Stat,
    Serve,
}

/// One application exchange and one file operation can be live at a time.
/// The HTTP connector retains a returned body until `body_sent` returns it.
pub struct App<E> {
    owner: u64,
    generation: u64,
    exchange: Option<E>,
    phase: Phase,
    pending: Option<(Id, Kind)>,
    file: Option<File>,
    path: Vec<u8>,
    buffer: Option<Buffer>,
    head: bool,
    response: Option<Response<E>>,
    body: Option<(usize, bool)>,
    body_outstanding: Option<usize>,
    demand: Option<usize>,
    offset: u64,
    remaining: u64,
    stopping: bool,
    exchange_done: bool,
    failed: bool,
    read_limit: usize,
}

impl<E: Copy + Eq> App<E> {
    pub fn new(owner: u64) -> Self {
        Self {
            owner,
            generation: 0,
            exchange: None,
            phase: Phase::Idle,
            pending: None,
            file: None,
            path: Vec::with_capacity(MAX_PATH),
            buffer: Some(vec![0; CHUNK_SIZE].into_boxed_slice()),
            head: false,
            response: None,
            body: None,
            body_outstanding: None,
            demand: None,
            offset: 0,
            remaining: 0,
            stopping: false,
            exchange_done: false,
            failed: false,
            read_limit: 0,
        }
    }

    pub fn is_idle(&self) -> bool {
        self.exchange.is_none()
    }

    /// The caller must wait for `settled` before handing over another request.
    pub fn request(&mut self, exchange: E, method: &[u8], target: &[u8]) -> bool {
        if !self.is_idle() {
            return false;
        }
        self.exchange = Some(exchange);
        self.phase = Phase::Open;
        self.head = method == b"HEAD";
        self.stopping = false;
        self.exchange_done = false;
        self.offset = 0;
        self.remaining = 0;
        self.demand = None;
        if method != b"GET" && !self.head {
            self.error_response(405);
        } else {
            match relative_path(target) {
                Ok(path) => self.path = path,
                Err(status) => self.error_response(status),
            }
        }
        true
    }

    pub fn demand(&mut self, exchange: E, capacity: usize) -> bool {
        if self.exchange != Some(exchange) || self.stopping {
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
        if self.exchange != Some(exchange)
            || self.body_outstanding.is_none()
            || self.buffer.is_some()
            || accepted > self.body_outstanding.unwrap()
        {
            return Err(buffer);
        }
        let expected = self.body_outstanding.take().unwrap();
        self.buffer = Some(buffer);
        self.remaining -= accepted as u64;
        if accepted != expected {
            self.stop(true);
        } else if self.remaining == 0 {
            self.stop(false);
        }
        Ok(())
    }

    /// Stops producing without treating outstanding HTTP writes as complete.
    pub fn source_finished(&mut self, exchange: E) -> bool {
        if self.exchange != Some(exchange) {
            return false;
        }
        self.stop(false);
        true
    }

    pub fn exchange_finished(&mut self, exchange: E) -> bool {
        if self.exchange != Some(exchange) {
            return false;
        }
        self.exchange_done = true;
        self.stop(false);
        true
    }

    pub fn abort(&mut self) {
        self.exchange_done = true;
        self.stop(false);
    }

    fn stop(&mut self, failed: bool) {
        self.stopping = true;
        self.demand = None;
        self.body = None;
        self.failed |= failed && !self.exchange_done;
    }

    fn error_response(&mut self, status: u16) {
        self.response = Some(Response {
            exchange: self.exchange.unwrap(),
            status,
            length: 0,
            head: self.head,
        });
        self.stop(false);
    }

    fn issue(&mut self, kind: Kind) -> Id {
        self.generation = self
            .generation
            .checked_add(1)
            .expect("operation id exhausted");
        let id = Id {
            owner: self.owner,
            generation: self.generation,
        };
        self.pending = Some((id, kind));
        id
    }

    /// A wrong-owner, stale, wrong-kind or invalid-count completion is returned
    /// intact; in particular successful open descriptors and buffers are not lost.
    pub fn complete(&mut self, completion: Completion) -> Result<(), Completion> {
        let (id, kind) = match &completion {
            Completion::Open { id, .. } => (*id, Kind::Open),
            Completion::Stat { id, .. } => (*id, Kind::Stat),
            Completion::Read { id, buffer, result } => {
                if let Ok(count) = result
                    && (*count > self.read_limit || *count > buffer.len())
                {
                    return Err(completion);
                }
                (*id, Kind::Read)
            }
            Completion::Close { id, .. } => (*id, Kind::Close),
        };
        if self.pending != Some((id, kind)) {
            return Err(completion);
        }
        self.pending = None;
        match completion {
            Completion::Open { result, .. } => match result {
                Ok(file) => {
                    self.file = Some(file);
                    self.phase = Phase::Stat;
                }
                Err(error) if !self.stopping => self.error_response(status(error)),
                Err(_) => {}
            },
            Completion::Stat { result, .. } if !self.stopping => match result {
                Ok(metadata) if metadata.regular => {
                    self.remaining = metadata.length;
                    self.response = Some(Response {
                        exchange: self.exchange.unwrap(),
                        status: 200,
                        length: metadata.length,
                        head: self.head,
                    });
                    self.phase = Phase::Serve;
                    if self.head || metadata.length == 0 {
                        self.stop(false);
                    }
                }
                Ok(_) => self.error_response(404),
                Err(error) => self.error_response(status(error)),
            },
            Completion::Read { buffer, result, .. } => {
                self.buffer = Some(buffer);
                if !self.stopping {
                    match result {
                        Ok(count) if count > 0 => {
                            self.offset += count as u64;
                            self.body = Some((count, count as u64 == self.remaining));
                        }
                        _ => self.stop(true),
                    }
                }
            }
            Completion::Close { result, .. } => {
                // Ownership was consumed by close, even when close reports an
                // error. Retrying a Linux close can close a reused descriptor.
                self.file = None;
                if result.is_err() && !self.exchange_done {
                    self.failed = true;
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub fn next<P: Ports<E>>(&mut self, ports: &mut P) -> Option<P::Output> {
        let exchange = self.exchange?;
        loop {
            let output = if self.failed {
                self.failed = false;
                ports.source_failed(exchange)
            } else if let Some(response) = self.response.take() {
                if self.exchange_done {
                    continue;
                }
                ports.respond(response)
            } else if self.pending.is_some() {
                return None;
            } else if self.stopping {
                if let Some(file) = self.file {
                    let id = self.issue(Kind::Close);
                    ports.close(Close { id, file })
                } else if self.exchange_done && self.body_outstanding.is_none() {
                    self.exchange = None;
                    self.phase = Phase::Idle;
                    return ports.settled(exchange);
                } else {
                    return None;
                }
            } else {
                match self.phase {
                    Phase::Open => {
                        let id = self.issue(Kind::Open);
                        let path = std::mem::take(&mut self.path);
                        ports.open(Open { id, path })
                    }
                    Phase::Stat => {
                        let id = self.issue(Kind::Stat);
                        ports.stat(Stat {
                            id,
                            file: self.file.unwrap(),
                        })
                    }
                    Phase::Serve => {
                        if let Some((count, end)) = self.body.take() {
                            self.body_outstanding = Some(count);
                            ports.body(Body {
                                exchange,
                                buffer: self.buffer.take().unwrap(),
                                range: 0..count,
                                end,
                            })
                        } else if self.body_outstanding.is_some() {
                            return None;
                        } else if let Some(capacity) = self.demand.take() {
                            let buffer = self.buffer.take().unwrap();
                            let limit = (self.remaining.min(capacity as u64)) as usize;
                            self.read_limit = limit;
                            let id = self.issue(Kind::Read);
                            ports.read(Read {
                                id,
                                file: self.file.unwrap(),
                                offset: self.offset,
                                buffer,
                                limit,
                            })
                        } else {
                            return None;
                        }
                    }
                    Phase::Idle => return None,
                }
            };
            if output.is_some() {
                return output;
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
