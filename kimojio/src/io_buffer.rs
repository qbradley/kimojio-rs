// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::ops::Range;

use crate::Errno;

/// An I/O result and its buffer, returned together even when the operation fails.
pub type BufferResult<T, B> = (Result<T, Errno>, B);

/// An owned buffer restricted to a byte range without copying its contents.
///
/// The range must be in bounds when the buffer is accessed.
#[derive(Debug)]
pub struct BufferSlice<B> {
    buffer: B,
    range: Range<usize>,
}

impl<B> BufferSlice<B> {
    /// Selects the byte range for the next operation without borrowing the buffer.
    pub fn new(buffer: B, range: Range<usize>) -> Self {
        Self { buffer, range }
    }

    /// Returns the complete buffer, including bytes outside the selected range.
    pub fn into_inner(self) -> B {
        self.buffer
    }
}

impl<B: AsRef<[u8]>> AsRef<[u8]> for BufferSlice<B> {
    fn as_ref(&self) -> &[u8] {
        &self.buffer.as_ref()[self.range.clone()]
    }
}

impl<B: AsMut<[u8]>> AsMut<[u8]> for BufferSlice<B> {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.buffer.as_mut()[self.range.clone()]
    }
}
