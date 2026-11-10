// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{
    AsyncEvent, AsyncLock, AsyncStreamRead, AsyncStreamWrite, BufferResult, Errno, SplittableStream,
};
use std::{cell::Cell, rc::Rc, time::Instant};

/// A simple in-memory pipe implementation for testing purposes.
#[derive(Clone, Default)]
pub struct BufferPipe {
    read: Rc<BufferStream>,
    write: Rc<BufferStream>,
}

impl BufferPipe {
    /// Creates a pair of connected buffer pipes for bidirectional communication.
    ///
    /// Each pipe can read from one buffer and write to the other.
    pub fn new() -> (Self, Self) {
        let a = Rc::new(BufferStream::new());
        let b = Rc::new(BufferStream::new());
        (
            Self {
                read: a.clone(),
                write: b.clone(),
            },
            Self { read: b, write: a },
        )
    }
}

impl AsyncStreamRead for BufferPipe {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        self.read.try_read(buffer, deadline).await
    }

    async fn read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        self.read.read(buffer, deadline).await
    }
}

impl AsyncStreamWrite for BufferPipe {
    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        self.write.write(buffer, deadline).await
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.write.shutdown().await
    }

    async fn close(&mut self) -> Result<(), Errno> {
        self.read.close().await?;
        self.write.close().await
    }
}

impl SplittableStream for BufferPipe {
    type ReadStream = BufferReadStream;
    type WriteStream = BufferWriteStream;

    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), Errno> {
        let read = self.read;
        let write = self.write;
        Ok((BufferReadStream(read), BufferWriteStream(write)))
    }
}

struct BufferStreamState {
    buffer: [u8; 1024],
    head: usize,
    amount: usize,
}

/// A single-direction in-memory buffer stream.
///
/// Provides async read and write operations backed by an in-memory buffer.
pub struct BufferStream {
    state: AsyncLock<BufferStreamState>,
    available: AsyncEvent,
    not_full: AsyncEvent,
    closed: Cell<bool>,
}

impl BufferStream {
    /// Creates a new empty buffer stream.
    pub fn new() -> Self {
        let not_full = AsyncEvent::new();
        not_full.set();
        Self {
            state: AsyncLock::new(BufferStreamState {
                buffer: [0; 1024],
                head: 0,
                amount: 0,
            }),
            available: AsyncEvent::new(),
            not_full,
            closed: Cell::new(false),
        }
    }

    fn update_events(&self, state: &BufferStreamState) {
        let available = state.amount > 0;
        let full = state.amount == state.buffer.len();

        if available {
            self.available.set();
        } else {
            self.available.reset();
        }

        if full {
            self.not_full.reset();
        } else {
            self.not_full.set();
        }
    }

    async fn try_read<B: AsMut<[u8]> + 'static>(
        &self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        let result = self.try_read_impl(buffer.as_mut(), deadline).await;
        (result, buffer)
    }

    async fn read<B: AsMut<[u8]> + 'static>(
        &self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let result = async {
            if self.closed.get() && !self.available.is_set() {
                return Err(Errno::PIPE);
            }
            let buffer = buffer.as_mut();
            let mut filled = 0;
            while filled < buffer.len() {
                match self.try_read_impl(&mut buffer[filled..], deadline).await? {
                    0 => return Err(Errno::PIPE),
                    amount => filled += amount,
                }
            }
            Ok(())
        }
        .await;
        (result, buffer)
    }

    async fn write<B: AsRef<[u8]> + 'static>(
        &self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let result = self.write_impl(buffer.as_ref(), deadline).await;
        (result, buffer)
    }

    async fn try_read_impl(
        &self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        if self.closed.get() && !self.available.is_set() {
            return Err(Errno::from_raw_os_error(crate::EPIPE));
        }

        self.available.wait_with_deadline(deadline).await?;

        let mut state = self.state.lock_with_deadline(deadline).await?;

        let end = std::cmp::min(state.head + state.amount, state.buffer.len());
        let amount = std::cmp::min(end - state.head, buffer.len());

        if amount > 0 {
            buffer[0..amount].copy_from_slice(&state.buffer[state.head..state.head + amount]);
            state.head = (state.head + amount) % state.buffer.len();
            state.amount -= amount;
        }

        self.update_events(&state);

        Ok(amount)
    }

    async fn write_impl(&self, mut buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        if self.closed.get() {
            return Err(Errno::from_raw_os_error(crate::EPIPE));
        }

        while !buffer.is_empty() {
            // TODO: timeout
            self.not_full.wait_with_deadline(deadline).await?;

            let mut state = self.state.lock().await?;
            let free_head = (state.head + state.amount) % state.buffer.len();
            let free_amount = state.buffer.len() - state.amount;
            let free_end = std::cmp::min(free_head + free_amount, state.buffer.len());
            let amount = std::cmp::min(buffer.len(), free_end - free_head);

            state.buffer[free_head..free_head + amount].copy_from_slice(&buffer[0..amount]);
            state.amount += amount;
            buffer = &buffer[amount..];

            self.update_events(&state);
        }

        Ok(())
    }

    async fn shutdown(&self) -> Result<(), Errno> {
        self.closed.set(true);
        self.available.set();
        self.not_full.set();
        Ok(())
    }

    async fn close(&self) -> Result<(), Errno> {
        self.closed.set(true);
        self.available.set();
        self.not_full.set();
        Ok(())
    }
}

impl Default for BufferStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncStreamRead for BufferStream {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        BufferStream::try_read(self, buffer, deadline).await
    }

    async fn read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        BufferStream::read(self, buffer, deadline).await
    }
}

impl AsyncStreamWrite for BufferStream {
    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        BufferStream::write(self, buffer, deadline).await
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        BufferStream::shutdown(self).await
    }

    async fn close(&mut self) -> Result<(), Errno> {
        BufferStream::close(self).await
    }
}

/// The read half of a `BufferPipe` after splitting.
pub struct BufferReadStream(Rc<BufferStream>);

/// The write half of a `BufferPipe` after splitting.
pub struct BufferWriteStream(Rc<BufferStream>);

impl AsyncStreamRead for BufferReadStream {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        self.0.try_read(buffer, deadline).await
    }

    async fn read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        self.0.read(buffer, deadline).await
    }
}

impl AsyncStreamWrite for BufferWriteStream {
    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        self.0.write(buffer, deadline).await
    }
    async fn shutdown(&mut self) -> Result<(), Errno> {
        self.0.shutdown().await
    }
    async fn close(&mut self) -> Result<(), Errno> {
        self.0.close().await
    }
}

impl SplittableStream for BufferStream {
    type ReadStream = BufferReadStream;
    type WriteStream = BufferWriteStream;

    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), Errno> {
        let write = Rc::new(self);
        let read = write.clone();

        Ok((BufferReadStream(read), BufferWriteStream(write)))
    }
}

#[cfg(test)]
mod test {
    use super::{BufferPipe, BufferStream};
    use crate::{AsyncStreamRead, AsyncStreamWrite, SplittableStream, operations};
    use std::rc::Rc;

    #[crate::test]
    async fn buffer_stream_test_write_larger_than_internal_buffer() {
        let mut data = Vec::new();
        let stream = Rc::new(BufferStream::new());

        let task = {
            let stream = stream.clone();
            operations::spawn_task(async move {
                loop {
                    let (result, buffer) = stream.read([0; 1024], None).await;
                    if result.is_err() {
                        return data;
                    }
                    data.extend_from_slice(&buffer);
                }
            })
        };

        stream.write([42u8; 4096], None).await.0.unwrap();
        stream.shutdown().await.unwrap();
        let data = task.await.unwrap();
        assert_eq!(&[42u8; 4096], &data[0..]);
    }

    #[crate::test]
    async fn buffer_stream_contiguous_full_buffer_test() {
        let stream = BufferStream::new();
        // Fill the stream completely.
        let data = [7u8; 1024];
        stream.write(data, None).await.0.unwrap();

        // Read back all data.
        let (result, read_buf) = stream.read([0u8; 1024], None).await;
        result.unwrap();
        assert_eq!(data, read_buf);
    }

    #[crate::test]
    async fn buffer_stream_interleaved_read_write_test() {
        let stream = BufferStream::new();
        // Write part of the data.
        stream.write(b"abc", None).await.0.unwrap();
        // Perform a partial read.
        let (result, buf) = stream.try_read([0u8; 2], None).await;
        assert_eq!(result, Ok(2));
        assert_eq!(&buf, b"ab");

        // Now write additional data.
        stream.write(b"defgh", None).await.0.unwrap();

        // Read the remaining data.
        let (result, buf2) = stream.read([0u8; 6], None).await;
        result.unwrap();
        // Expected concatenation: "cdefgh"
        assert_eq!(&buf2, b"cdefgh");
    }

    #[crate::test]
    async fn buffer_stream_close_and_error_test() {
        let stream = BufferStream::new();
        stream.write(b"data", None).await.0.unwrap();

        // Shut down the stream.
        stream.shutdown().await.unwrap();

        // Subsequent operations should error.
        let (res2, _) = stream.write(b"more", None).await;
        assert!(res2.is_err());

        let (result, buf) = stream.read([0u8; 4], None).await;
        result.unwrap();
        assert_eq!(&buf, b"data");

        let (res, buf) = stream.read(buf, None).await;
        assert!(res.is_err());
        let (res3, _) = stream.try_read(buf, None).await;
        assert!(res3.is_err());
    }

    #[crate::test]
    async fn buffer_stream_multiple_small_writes_test() {
        let stream = BufferStream::new();

        // Write several small chunks.
        let data: [&'static [u8]; 4] = [b"hello", b" ", b"world", b"!"];
        for chunk in data {
            stream.write(chunk, None).await.0.unwrap();
        }

        // Read back the entire message.
        let (read_result, result) = stream.read(vec![0u8; 12], None).await;
        read_result.unwrap();
        assert_eq!(&result, b"hello world!");
    }

    #[crate::test]
    async fn buffer_stream_tests() {
        let mut buffer_stream = BufferStream::new();
        let buffer1 = [1; 1024];

        async fn expect(stream: &mut BufferStream, buffer: &[u8]) {
            let (result, result_buffer) = stream.read(vec![0; buffer.len()], None).await;
            result.unwrap();
            assert_eq!(buffer, result_buffer);
        }

        async fn write_read(stream: &mut BufferStream, buffer: &[u8]) {
            stream.write(buffer.to_vec(), None).await.0.unwrap();
            expect(stream, buffer).await;
        }

        write_read(&mut buffer_stream, &[1; 1024]).await;
        write_read(&mut buffer_stream, &[2; 512]).await;
        write_read(&mut buffer_stream, &[3; 512]).await;

        write_read(&mut buffer_stream, &[4; 512]).await;
        write_read(&mut buffer_stream, &[5; 1024]).await;

        // 2. try_read returns some data when the buffer has some
        buffer_stream.write(b"hello", None).await.0.unwrap();
        let (result, buffer1) = buffer_stream.try_read(buffer1, None).await;
        assert_eq!(result, Ok(5));
        assert_eq!(&buffer1[0..5], b"hello");

        // 3. try_read returns a short read when the buffer has partly wrapped
        buffer_stream.write(b"world", None).await.0.unwrap();
        let (result, buffer1) = buffer_stream.try_read(buffer1, None).await;
        assert_eq!(result, Ok(5));
        assert_eq!(&buffer1[0..5], b"world");

        // 4. read does not return until all the data has been read (need to cover partly wrapped case too)
        buffer_stream.write(b"hello", None).await.0.unwrap();
        buffer_stream.write(b"world", None).await.0.unwrap();
        let (result, buffer) = buffer_stream.read([0; 10], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"helloworld");

        // 5. try_read, read, and write to a closed stream returns an error
        buffer_stream.shutdown().await.unwrap();
        let (result, _) = buffer_stream.read(buffer, None).await;
        assert!(result.is_err());
        let (result, _) = buffer_stream.write(b"test", None).await;
        assert!(result.is_err());

        async fn shutdown_and_close(mut write_stream: impl AsyncStreamWrite) {
            write_stream.shutdown().await.unwrap();
            write_stream.close().await.unwrap();
        }

        shutdown_and_close(buffer_stream).await;
    }

    #[crate::test]
    async fn buffer_stream_split_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Test basic write to write stream and read from read stream
        write_stream.write(b"hello world", None).await.0.unwrap();

        let (result, buffer) = read_stream.read([0u8; 11], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"hello world");
    }

    #[crate::test]
    async fn buffer_read_stream_try_read_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write data to write stream
        write_stream.write(b"test data", None).await.0.unwrap();

        // Test try_read on read stream
        let (result, buffer) = read_stream.try_read([0u8; 5], None).await;
        assert_eq!(result, Ok(5));
        assert_eq!(&buffer, b"test ");

        // Read remaining data
        let (result, remaining) = read_stream.try_read([0u8; 4], None).await;
        assert_eq!(result, Ok(4));
        assert_eq!(&remaining, b"data");
    }

    #[crate::test]
    async fn buffer_read_stream_read_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Test reading exact amount
        write_stream.write(b"exact", None).await.0.unwrap();
        let (result, buffer) = read_stream.read([0u8; 5], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"exact");

        // Test reading with concurrent writes
        let write_task = {
            let mut write_stream = write_stream;
            crate::operations::spawn_task(async move {
                write_stream.write(b"part1", None).await.0.unwrap();
                write_stream.write(b"part2", None).await.0.unwrap();
                write_stream
            })
        };

        let (result, large_buffer) = read_stream.read([0u8; 10], None).await;
        result.unwrap();
        assert_eq!(&large_buffer, b"part1part2");

        write_task.await.unwrap();
    }

    #[crate::test]
    async fn buffer_write_stream_write_test() {
        let buffer_stream = BufferStream::new();
        let (read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        let read_task = {
            let mut read_stream = read_stream;
            crate::operations::spawn_task(async move {
                // Read back small and medium text
                let (result, buffer1) = read_stream.read([0u8; 24], None).await;
                result.unwrap();

                // Read back large data in chunks
                let mut large_buffer = Vec::new();
                for _ in 0..2 {
                    let (result, temp_buffer) = read_stream.read([0u8; 1024], None).await;
                    result.unwrap();
                    large_buffer.extend_from_slice(&temp_buffer);
                }
                (buffer1, large_buffer)
            })
        };

        // Test writing various sizes
        write_stream.write(b"small", None).await.0.unwrap();
        write_stream
            .write(b" medium sized text ", None)
            .await
            .0
            .unwrap();

        // Test writing large data that exceeds internal buffer
        let large_data = [42u8; 2048];
        write_stream.write(large_data, None).await.0.unwrap();

        let (buffer1, buffer2) = read_task.await.unwrap();
        assert_eq!(&buffer1, b"small medium sized text ");
        assert_eq!(&buffer2, &large_data);
    }

    #[crate::test]
    async fn buffer_write_stream_shutdown_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write some data before shutdown
        write_stream
            .write(b"before shutdown", None)
            .await
            .0
            .unwrap();

        // Shutdown write stream
        write_stream.shutdown().await.unwrap();

        // Should be able to read existing data
        let (result, buffer) = read_stream.read([0u8; 15], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let (result, _) = write_stream.write(b"after shutdown", None).await;
        assert!(result.is_err());

        // Reading after shutdown when no data is available should fail
        let (result, _) = read_stream.read([0u8; 1], None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_write_stream_close_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write some data before close
        write_stream.write(b"before close", None).await.0.unwrap();

        // Close write stream
        write_stream.close().await.unwrap();

        // Should be able to read existing data
        let (result, buffer) = read_stream.read([0u8; 12], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let (result, _) = write_stream.write(b"after close", None).await;
        assert!(result.is_err());

        // Reading after close when no data is available should fail
        let (result, _) = read_stream.read([0u8; 1], None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_split_streams_concurrent_operations_test() {
        let buffer_stream = BufferStream::new();
        let (read_stream, write_stream) = buffer_stream.split().await.unwrap();

        let read_task = {
            let mut read_stream = read_stream;
            crate::operations::spawn_task(async move {
                let mut result = Vec::new();
                // Read three chunks
                for _ in 0..3 {
                    let (read_result, buffer) = read_stream.read([0u8; 10], None).await;
                    read_result.unwrap();
                    result.extend_from_slice(&buffer);
                }
                result
            })
        };

        let write_task = {
            let mut write_stream = write_stream;
            crate::operations::spawn_task(async move {
                write_stream.write(b"chunk1____", None).await.0.unwrap();
                write_stream.write(b"chunk2____", None).await.0.unwrap();
                write_stream.write(b"chunk3____", None).await.0.unwrap();
                write_stream.shutdown().await.unwrap();
            })
        };

        let read_result = read_task.await.unwrap();
        write_task.await.unwrap();

        assert_eq!(&read_result, b"chunk1____chunk2____chunk3____");
    }

    #[crate::test]
    async fn buffer_split_streams_partial_reads_test() {
        let buffer_stream = BufferStream::new();
        let (mut read_stream, mut write_stream) = buffer_stream.split().await.unwrap();

        // Write data in chunks
        write_stream.write(b"abcdefgh", None).await.0.unwrap();

        // Read partial data with try_read
        let (result, buffer1) = read_stream.try_read([0u8; 3], None).await;
        assert_eq!(result, Ok(3));
        assert_eq!(&buffer1, b"abc");

        let (result, buffer2) = read_stream.try_read([0u8; 2], None).await;
        assert_eq!(result, Ok(2));
        assert_eq!(&buffer2, b"de");

        // Read remaining with exact read
        let (result, buffer3) = read_stream.read([0u8; 3], None).await;
        result.unwrap();
        assert_eq!(&buffer3, b"fgh");
    }

    #[crate::test]
    async fn buffer_pipe_split_test() {
        let (pipe1, pipe2) = BufferPipe::new();
        let (mut read_stream, mut write_stream) = pipe1.split().await.unwrap();

        // Write data using the write stream from split
        write_stream.write(b"pipe data", None).await.0.unwrap();

        // The other pipe should be able to read this data
        let (mut other_read, mut other_write) = pipe2.split().await.unwrap();
        let (result, buffer) = other_read.read([0u8; 9], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"pipe data");

        // Write from the other direction
        other_write.write(b"reverse", None).await.0.unwrap();

        // Read from the original read stream
        let (result, reverse_buffer) = read_stream.read([0u8; 7], None).await;
        result.unwrap();
        assert_eq!(&reverse_buffer, b"reverse");
    }

    #[crate::test]
    async fn default_writev_returns_all_owners_on_success_and_failure() {
        let (mut writer, mut reader) = BufferPipe::new();
        let buffers = vec![b"owned".to_vec(), Vec::new(), b" buffers".to_vec()];
        let owners = buffers.as_ptr();
        let (result, buffers) = writer.writev(buffers, None).await;
        result.unwrap();
        assert_eq!(buffers.as_ptr(), owners);
        let (result, contents) = reader.read(vec![0; 13], None).await;
        result.unwrap();
        assert_eq!(contents, b"owned buffers");
        writer.shutdown().await.unwrap();
        let (result, buffers) = writer.writev(buffers, None).await;
        assert_eq!(result, Err(crate::Errno::PIPE));
        assert_eq!(buffers.as_ptr(), owners);
        assert_eq!(
            buffers,
            vec![b"owned".to_vec(), Vec::new(), b" buffers".to_vec()]
        );
    }

    #[crate::test]
    async fn buffer_pipe_try_read_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Write data to pipe2
        pipe2
            .write(b"test data for try_read", None)
            .await
            .0
            .unwrap();

        // Use try_read on pipe1 to read partial data
        let (result, buffer1) = pipe1.try_read([0u8; 4], None).await;
        assert_eq!(result, Ok(4));
        assert_eq!(&buffer1, b"test");

        // Read remaining data with another try_read
        let (result, buffer2) = pipe1.try_read([0u8; 18], None).await;
        assert_eq!(result, Ok(18));
        assert_eq!(&buffer2, b" data for try_read");
    }

    #[crate::test]
    async fn buffer_pipe_read_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Test reading exact amount
        pipe2.write(b"exact", None).await.0.unwrap();
        let (result, buffer) = pipe1.read([0u8; 5], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"exact");

        // Test reading with concurrent writes
        let write_task = {
            let mut pipe2 = pipe2;
            crate::operations::spawn_task(async move {
                pipe2.write(b"hello", None).await.0.unwrap();
                pipe2.write(b"world", None).await.0.unwrap();
                pipe2
            })
        };

        let (result, large_buffer) = pipe1.read([0u8; 10], None).await;
        result.unwrap();
        assert_eq!(&large_buffer, b"helloworld");

        write_task.await.unwrap();
    }

    #[crate::test]
    async fn buffer_pipe_write_test() {
        let (mut pipe1, pipe2) = BufferPipe::new();

        // Test writing various sizes
        pipe1.write(b"small", None).await.0.unwrap();
        pipe1.write(b" medium sized text ", None).await.0.unwrap();

        // Test writing large data that exceeds internal buffer
        let large_data = [42u8; 2048];

        let read_task = {
            let mut pipe2 = pipe2;
            crate::operations::spawn_task(async move {
                // Read back small and medium text
                let (result, buffer1) = pipe2.read([0u8; 24], None).await;
                result.unwrap();

                // Read back large data in chunks
                let mut large_buffer = Vec::new();
                for _ in 0..2 {
                    let (result, temp_buffer) = pipe2.read([0u8; 1024], None).await;
                    result.unwrap();
                    large_buffer.extend_from_slice(&temp_buffer);
                }
                (buffer1, large_buffer)
            })
        };

        pipe1.write(large_data, None).await.0.unwrap();

        let (buffer1, buffer2) = read_task.await.unwrap();
        assert_eq!(&buffer1, b"small medium sized text ");
        assert_eq!(&buffer2, &large_data);
    }

    #[crate::test]
    async fn buffer_pipe_shutdown_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Write some data before shutdown
        pipe1.write(b"before shutdown", None).await.0.unwrap();

        // Shutdown pipe1
        pipe1.shutdown().await.unwrap();

        // Should be able to read existing data from pipe2
        let (result, buffer) = pipe2.read([0u8; 15], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let (result, _) = pipe1.write(b"after shutdown", None).await;
        assert!(result.is_err());

        // Reading after shutdown when no data is available should fail
        let (result, _) = pipe2.read([0u8; 1], None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_pipe_close_test() {
        let (mut pipe1, mut pipe2) = BufferPipe::new();

        // Write some data before close
        pipe1.write(b"before close", None).await.0.unwrap();

        // Close pipe1
        pipe1.close().await.unwrap();

        // Should be able to read existing data from pipe2
        let (result, buffer) = pipe2.read([0u8; 12], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let (result, _) = pipe1.write(b"after close", None).await;
        assert!(result.is_err());

        // Reading after close when no data is available should fail
        let (result, _) = pipe2.read([0u8; 1], None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_stream_default_test() {
        let stream = BufferStream::default();

        // Test that default stream works like a new stream
        stream.write(b"default test", None).await.0.unwrap();

        let (result, buffer) = stream.read([0u8; 12], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"default test");
    }

    #[crate::test]
    async fn buffer_stream_shutdown_test() {
        let buffer_stream = BufferStream::new();

        // Write some data before shutdown
        buffer_stream
            .write(b"before shutdown", None)
            .await
            .0
            .unwrap();

        // Shutdown the stream
        buffer_stream.shutdown().await.unwrap();

        // Should be able to read existing data
        let (result, buffer) = buffer_stream.read([0u8; 15], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let (result, _) = buffer_stream.write(b"after shutdown", None).await;
        assert!(result.is_err());

        // Reading after shutdown when no data is available should fail
        let (result, empty_buffer) = buffer_stream.read([0u8; 1], None).await;
        assert!(result.is_err());

        // try_read should also fail after shutdown with no data
        let (result, _) = buffer_stream.try_read(empty_buffer, None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn buffer_stream_close_test() {
        let buffer_stream = BufferStream::new();

        // Write some data before close
        buffer_stream.write(b"before close", None).await.0.unwrap();

        // Close the stream
        buffer_stream.close().await.unwrap();

        // Should be able to read existing data
        let (result, buffer) = buffer_stream.read([0u8; 12], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let (result, _) = buffer_stream.write(b"after close", None).await;
        assert!(result.is_err());

        // Reading after close when no data is available should fail
        let (result, empty_buffer) = buffer_stream.read([0u8; 1], None).await;
        assert!(result.is_err());

        // try_read should also fail after close with no data
        let (result, _) = buffer_stream.try_read(empty_buffer, None).await;
        assert!(result.is_err());
    }
}
