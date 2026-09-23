//! Owned I/O and body operations exchanged between the protocol core and executor.
//!
//! A port receives an operation, performs it once, then returns its completion
//! to the same connection. Keep each operation's storage alive until completion;
//! cancellation requests do not release storage or settle the original operation.
use crate::*;
use std::ops::Range;

/// Read request containing the exact buffer slice the executor may fill.
#[derive(Debug)]
pub struct ReadOp<B> {
    pub(crate) id: OperationId,
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
}

impl<B: Buffer> ReadOp<B> {
    /// Returns the token required to complete this read.
    pub fn id(&self) -> OperationId {
        self.id
    }
    /// Returns the writable receive region; do not resize or replace its storage.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        &mut self.buffer.as_mut()[self.range.clone()]
    }
    /// Packages this operation with the transport result for core settlement.
    ///
    /// On success, report the number of initialized bytes in `bytes_mut()`.
    pub fn complete(self, result: IoResult<usize>) -> ReadCompletion<B> {
        ReadCompletion { op: self, result }
    }
}

/// Settled read operation returned to `Client::complete_read` or `Server::complete_read`.
#[derive(Debug)]
pub struct ReadCompletion<B> {
    pub(crate) op: ReadOp<B>,
    pub(crate) result: IoResult<usize>,
}

impl<B> ReadCompletion<B> {
    /// Recovers the original operation and its transport result.
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

/// Write request exposing the remaining wire bytes as up to three slices.
///
/// Keep the operation in stable local storage while those slices are borrowed;
/// end all borrows before moving it into [`Self::complete`].
#[derive(Debug)]
pub struct WriteOp<B> {
    pub(crate) id: OperationId,
    pub(crate) storage: WriteStorage<B>,
    pub(crate) cursor: usize,
}

impl<B: AsRef<[u8]>> WriteOp<B> {
    /// Returns the token required to complete this write.
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
    /// Packages this operation with the exact accepted wire-byte count or error.
    pub fn complete(self, result: IoResult<usize>) -> WriteCompletion<B> {
        WriteCompletion { op: self, result }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.slices().iter().map(|s| s.len()).sum()
    }
}

/// Settled write operation returned to `Client::complete_write` or `Server::complete_write`.
#[derive(Debug)]
pub struct WriteCompletion<B> {
    pub(crate) op: WriteOp<B>,
    pub(crate) result: IoResult<usize>,
}

impl<B> WriteCompletion<B> {
    /// Recovers the original operation and transport result.
    pub fn into_parts(self) -> (WriteOp<B>, IoResult<usize>) {
        (self.op, self.result)
    }
}

/// I/O direction whose readiness the executor must wait for.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Direction {
    /// Transport can accept incoming bytes.
    Read,
    /// Transport can accept outgoing bytes.
    Write,
}

/// Nonblocking readiness wait requested after a transport reports `WouldBlock`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReadinessOp {
    pub(crate) id: OperationId,
    /// Direction the executor must monitor.
    pub direction: Direction,
}

impl ReadinessOp {
    /// Returns the token required to complete this readiness wait.
    pub fn id(self) -> OperationId {
        self.id
    }
    /// Packages the readiness result for core settlement; do not retry the old I/O op directly.
    pub fn complete(self, result: IoResult<()>) -> ReadinessCompletion {
        ReadinessCompletion { op: self, result }
    }
}

/// Settled readiness wait returned to `complete_readiness`.
#[derive(Clone, Copy, Debug)]
pub struct ReadinessCompletion {
    pub(crate) op: ReadinessOp,
    pub(crate) result: IoResult<()>,
}

/// Request to cancel an outstanding operation.
///
/// Cancellation is advisory: continue retaining the target operation and its
/// storage until its normal completion arrives.
#[derive(Clone, Copy, Debug)]
pub struct CancelOp {
    /// Identity of the original operation to cancel.
    pub target: OperationId,
}

/// Request to close the transport after the core's ownership rules permit it.
#[derive(Clone, Copy, Debug)]
pub struct CloseOp {
    pub(crate) id: OperationId,
}

impl CloseOp {
    /// Returns the token required to complete the close.
    pub fn id(self) -> OperationId {
        self.id
    }
    /// Packages the transport-close result for core settlement.
    pub fn complete(self, result: IoResult<()>) -> CloseCompletion {
        CloseCompletion { op: self, result }
    }
}

/// Settled close operation returned to `complete_close`.
#[derive(Clone, Copy, Debug)]
pub struct CloseCompletion {
    pub(crate) op: CloseOp,
    pub(crate) result: IoResult<()>,
}

/// Exclusive lease of received body bytes offered to the application.
///
/// Release it with the exact number of consumed bytes. Partial consumption
/// returns the unconsumed suffix for a later offer.
#[derive(Debug)]
pub struct BodyOp<B> {
    pub(crate) id: OperationId,
    pub(crate) exchange: ExchangeId,
    pub(crate) buffer: B,
    pub(crate) range: Range<usize>,
    pub(crate) buffered_end: usize,
}

impl<B: Buffer> BodyOp<B> {
    /// Returns the token identifying this body delivery.
    pub fn id(&self) -> OperationId {
        self.id
    }
    /// Returns the exchange receiving these body bytes.
    pub fn exchange(&self) -> ExchangeId {
        self.exchange
    }
    /// Views the currently offered payload.
    pub fn bytes(&self) -> &[u8] {
        &self.buffer.as_ref()[self.range.clone()]
    }
    /// Returns storage and exact consumption, without adding delivery credit.
    pub fn release(self, consumed: usize) -> BodyCompletion<B> {
        BodyCompletion { op: self, consumed }
    }
}

/// Returned body lease and application consumption count.
#[derive(Debug)]
pub struct BodyCompletion<B> {
    pub(crate) op: BodyOp<B>,
    pub(crate) consumed: usize,
}

impl<B> BodyCompletion<B> {
    /// Recovers the original body lease and number of consumed bytes.
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
