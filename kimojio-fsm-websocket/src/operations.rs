use crate::*;
use std::ops::Range;

#[derive(Debug)]
pub struct ReadOp<B> {
    pub(crate) id: OperationId,
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
}
impl<B: Buffer> ReadOp<B> {
    pub fn id(&self) -> OperationId {
        self.id
    }
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buffer.as_mut()[self.range.clone()]
    }
    pub fn complete(self, result: IoResult<usize>) -> ReadCompletion<B> {
        ReadCompletion { op: self, result }
    }
}
#[derive(Debug)]
pub struct ReadCompletion<B> {
    pub(crate) op: ReadOp<B>,
    pub(crate) result: IoResult<usize>,
}
impl<B> ReadCompletion<B> {
    pub fn into_parts(self) -> (ReadOp<B>, IoResult<usize>) {
        (self.op, self.result)
    }
}

#[derive(Debug)]
pub(crate) struct Outgoing<W> {
    pub id: MessageId,
    pub command: SendMessage<W>,
    pub offset: usize,
    pub accepted: usize,
    pub acceptance: Acceptance,
}

#[derive(Debug)]
pub(crate) enum WriteStorage<W> {
    Data {
        outgoing: Outgoing<W>,
        range: Range<usize>,
        last: bool,
    },
    Control {
        bytes: [u8; 125],
        len: usize,
        close: bool,
    },
}

#[derive(Debug)]
pub struct WriteOp<W> {
    pub(crate) id: OperationId,
    pub(crate) header: [u8; 10],
    pub(crate) header_len: usize,
    pub(crate) cursor: usize,
    pub(crate) storage: WriteStorage<W>,
}
impl<W: AsRef<[u8]>> WriteOp<W> {
    pub fn id(&self) -> OperationId {
        self.id
    }
    pub fn slices(&self) -> [&[u8]; 2] {
        let payload = match &self.storage {
            WriteStorage::Data {
                outgoing, range, ..
            } => &outgoing.command.buffer.as_ref()[range.clone()],
            WriteStorage::Control { bytes, len, .. } => &bytes[..*len],
        };
        let mut skip = self.cursor;
        [&self.header[..self.header_len], payload].map(|bytes| {
            let start = skip.min(bytes.len());
            skip -= start;
            &bytes[start..]
        })
    }
    pub fn complete(self, result: IoResult<usize>) -> WriteCompletion<W> {
        WriteCompletion { op: self, result }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.slices().iter().map(|s| s.len()).sum()
    }
}
#[derive(Debug)]
pub struct WriteCompletion<W> {
    pub(crate) op: WriteOp<W>,
    pub(crate) result: IoResult<usize>,
}
impl<W> WriteCompletion<W> {
    pub fn into_parts(self) -> (WriteOp<W>, IoResult<usize>) {
        (self.op, self.result)
    }
}

#[derive(Debug)]
pub struct ChunkOp<B> {
    pub(crate) id: OperationId,
    pub(crate) message: MessageId,
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
}
impl<B: Buffer> ChunkOp<B> {
    pub fn id(&self) -> OperationId {
        self.id
    }
    pub fn message(&self) -> MessageId {
        self.message
    }
    pub fn bytes(&self) -> &[u8] {
        &self.buffer.as_ref()[self.range.clone()]
    }
    /// Releases the complete offered chunk. Ownership is the consumption credit.
    pub fn release(self) -> ChunkCompletion<B> {
        ChunkCompletion { op: self }
    }
}
#[derive(Debug)]
pub struct ChunkCompletion<B> {
    pub(crate) op: ChunkOp<B>,
}
impl<B> ChunkCompletion<B> {
    pub fn into_op(self) -> ChunkOp<B> {
        self.op
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Read,
    Write,
}
#[derive(Clone, Copy, Debug)]
pub struct ReadinessOp {
    pub(crate) id: OperationId,
    pub direction: Direction,
}
impl ReadinessOp {
    pub fn id(self) -> OperationId {
        self.id
    }
    pub fn complete(self, result: IoResult<()>) -> ReadinessCompletion {
        ReadinessCompletion { op: self, result }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct ReadinessCompletion {
    pub(crate) op: ReadinessOp,
    pub(crate) result: IoResult<()>,
}
impl ReadinessCompletion {
    pub fn into_parts(self) -> (ReadinessOp, IoResult<()>) {
        (self.op, self.result)
    }
}
#[derive(Clone, Copy, Debug)]
pub struct CancelOp {
    pub target: OperationId,
}
#[derive(Clone, Copy, Debug)]
pub struct CloseOp {
    pub(crate) id: OperationId,
}
impl CloseOp {
    pub fn id(self) -> OperationId {
        self.id
    }
    pub fn complete(self, result: IoResult<()>) -> CloseCompletion {
        CloseCompletion { op: self, result }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct CloseCompletion {
    pub(crate) op: CloseOp,
    pub(crate) result: IoResult<()>,
}
impl CloseCompletion {
    pub fn into_parts(self) -> (CloseOp, IoResult<()>) {
        (self.op, self.result)
    }
}
