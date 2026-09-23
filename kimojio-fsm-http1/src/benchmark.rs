//! Unstable helpers for simulated-I/O benchmarks, not real transports.

use crate::ReadOp;

/// Exchange equal-length buffers while retaining the read's identity and range.
///
/// Only for an executor that performs no actual I/O. Never exchange storage
/// submitted to a kernel operation or associated with registered buffers.
#[inline]
pub fn swap_read_buffer(op: &mut ReadOp<Vec<u8>>, buffer: &mut Vec<u8>) {
    assert_eq!(
        op.buffer.len(),
        buffer.len(),
        "replay buffer length changed"
    );
    std::mem::swap(&mut op.buffer, buffer);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConnectionId, OperationId, OperationKind};

    #[test]
    fn swap_preserves_operation_identity_and_range() {
        let id = OperationId {
            connection: ConnectionId {
                slot: 1,
                generation: 1,
            },
            sequence: 2,
            kind: OperationKind::Read,
        };
        let mut op = ReadOp {
            id,
            buffer: vec![1; 4],
            range: 1..3,
        };
        let mut replacement = vec![2; 4];
        swap_read_buffer(&mut op, &mut replacement);
        assert_eq!(op.id(), id);
        assert_eq!(op.range, 1..3);
        assert_eq!(op.bytes_mut(), &[2, 2]);
        assert_eq!(replacement, [1; 4]);
        let (returned, result) = op.complete(Ok(2)).into_parts();
        assert_eq!(returned.id(), id);
        assert_eq!(result, Ok(2));
    }

    #[test]
    fn wrong_length_is_rejected_before_either_buffer_changes() {
        let mut op = ReadOp {
            id: OperationId {
                connection: ConnectionId {
                    slot: 1,
                    generation: 1,
                },
                sequence: 2,
                kind: OperationKind::Read,
            },
            buffer: vec![1; 4],
            range: 0..4,
        };
        let mut replacement = vec![2; 3];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            swap_read_buffer(&mut op, &mut replacement);
        }));
        assert!(result.is_err());
        assert_eq!(op.buffer, [1; 4]);
        assert_eq!(replacement, [2; 3]);
    }
}
