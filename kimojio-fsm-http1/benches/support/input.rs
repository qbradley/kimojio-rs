use kimojio_fsm_http1::ReadOp;
use std::hint::black_box;

/// Statically selected simulated receive transport.
pub trait ReceiveMode {
    fn new(input: &[u8], read_limit: usize) -> Self;
    fn begin(&mut self) {}
    fn fill(&mut self, op: &mut ReadOp<Vec<u8>>, input: &[u8], offset: usize, count: usize);
}

pub struct CopyInput;

impl ReceiveMode for CopyInput {
    fn new(_: &[u8], _: usize) -> Self {
        Self
    }

    fn fill(&mut self, op: &mut ReadOp<Vec<u8>>, input: &[u8], offset: usize, count: usize) {
        op.bytes_mut()[..count].copy_from_slice(black_box(&input[offset..offset + count]));
    }
}

#[cfg(feature = "bench-internals")]
pub struct ReplayInput {
    buffers: Vec<Vec<u8>>,
    stride: usize,
    next: usize,
    // This slot holds the spare buffer while its fixture buffer belongs to the
    // original read operation, the core, or a body lease. It survives begin().
    active: Option<usize>,
}

#[cfg(feature = "bench-internals")]
impl ReceiveMode for ReplayInput {
    fn new(input: &[u8], read_limit: usize) -> Self {
        assert!(read_limit > 0);
        let stride = read_limit.min(super::IO_BYTES);
        let buffers = input
            .chunks(stride)
            .map(|bytes| {
                // Keep the original read capacity, including the last short read.
                let mut buffer = vec![0; super::IO_BYTES];
                buffer[..bytes.len()].copy_from_slice(bytes);
                buffer
            })
            .collect();
        Self {
            buffers,
            stride,
            next: 0,
            active: None,
        }
    }

    fn begin(&mut self) {
        self.next = 0;
    }

    fn fill(&mut self, op: &mut ReadOp<Vec<u8>>, input: &[u8], offset: usize, count: usize) {
        use kimojio_fsm_http1::benchmark::swap_read_buffer;

        assert!(self.next < self.buffers.len());
        assert_eq!(offset, self.next * self.stride);
        assert_eq!(count, (input.len() - offset).min(self.stride));
        if self.active != Some(self.next) {
            if let Some(previous) = self.active {
                // Restore the previous fixture before taking the next one.
                swap_read_buffer(op, &mut self.buffers[previous]);
            }
            swap_read_buffer(op, &mut self.buffers[self.next]);
            self.active = Some(self.next);
        }
        self.next += 1;
    }
}
