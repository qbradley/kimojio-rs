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
pub(crate) enum WriteStorage<B> {
    Head {
        bytes: Vec<u8>,
        kind: MetadataKind,
    },
    HeadBody {
        bytes: Vec<u8>,
        command: SendBody<B>,
        body_id: BodyId,
    },
    Body {
        command: SendBody<B>,
        body_id: BodyId,
        prefix: [u8; 24],
        prefix_len: usize,
        chunked: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataKind {
    Informational,
    FinalHead,
    BodyEnd,
}

#[derive(Debug)]
pub struct WriteOp<B> {
    pub(crate) id: OperationId,
    pub(crate) storage: WriteStorage<B>,
    pub(crate) cursor: usize,
}

impl<B: AsRef<[u8]>> WriteOp<B> {
    pub fn id(&self) -> OperationId {
        self.id
    }
    /// Remaining wire bytes. The cursor belongs to the connection.
    ///
    /// An async executor can borrow these slices from its locally owned op.
    /// It must end those borrows before it moves the op into a completion.
    pub fn slices(&self) -> [&[u8]; 3] {
        let slices: [&[u8]; 3] = match &self.storage {
            WriteStorage::Head { bytes, .. } => [bytes, &[], &[]],
            WriteStorage::HeadBody { bytes, command, .. } => {
                [bytes, &command.buffer.as_ref()[command.range.clone()], &[]]
            }
            WriteStorage::Body {
                command,
                prefix,
                prefix_len,
                chunked,
                ..
            } => [
                &prefix[..*prefix_len],
                &command.buffer.as_ref()[command.range.clone()],
                if *chunked { b"\r\n" } else { b"" },
            ],
        };
        let mut skip = self.cursor;
        slices.map(|bytes| {
            let offset = skip.min(bytes.len());
            skip -= offset;
            &bytes[offset..]
        })
    }
    pub fn complete(self, result: IoResult<usize>) -> WriteCompletion<B> {
        WriteCompletion { op: self, result }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.slices().iter().map(|s| s.len()).sum()
    }
}

#[derive(Debug)]
pub struct WriteCompletion<B> {
    pub(crate) op: WriteOp<B>,
    pub(crate) result: IoResult<usize>,
}

impl<B> WriteCompletion<B> {
    pub fn into_parts(self) -> (WriteOp<B>, IoResult<usize>) {
        (self.op, self.result)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    Read,
    Write,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Debug)]
pub struct BodyOp<B> {
    pub(crate) id: OperationId,
    pub(crate) exchange: ExchangeId,
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
    pub(crate) buffered_end: usize,
}

impl<B: Buffer> BodyOp<B> {
    pub fn id(&self) -> OperationId {
        self.id
    }
    pub fn exchange(&self) -> ExchangeId {
        self.exchange
    }
    pub fn bytes(&self) -> &[u8] {
        &self.buffer.as_ref()[self.range.clone()]
    }
    /// Returns storage and exact consumption, without adding delivery credit.
    pub fn release(self, consumed: usize) -> BodyCompletion<B> {
        BodyCompletion { op: self, consumed }
    }
}

#[derive(Debug)]
pub struct BodyCompletion<B> {
    pub(crate) op: BodyOp<B>,
    pub(crate) consumed: usize,
}

impl<B> BodyCompletion<B> {
    pub fn into_parts(self) -> (BodyOp<B>, usize) {
        (self.op, self.consumed)
    }
}

#[cfg(test)]
mod eager_layout_tests {
    use super::*;

    #[allow(dead_code)]
    enum OriginalWriteStorage<B> {
        Head {
            bytes: Vec<u8>,
            kind: MetadataKind,
        },
        Body {
            command: SendBody<B>,
            body_id: BodyId,
            prefix: [u8; 24],
            prefix_len: usize,
            chunked: bool,
        },
    }

    #[test]
    fn eager_variant_does_not_grow_existing_write_storage() {
        assert_eq!(
            size_of::<WriteStorage<Vec<u8>>>(),
            size_of::<OriginalWriteStorage<Vec<u8>>>()
        );
        assert_eq!(
            size_of::<WriteStorage<&[u8]>>(),
            size_of::<OriginalWriteStorage<&[u8]>>()
        );
    }
}
