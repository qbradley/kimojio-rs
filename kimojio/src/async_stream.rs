// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! AsyncStream is a trait that represents the ability to read
//! and write to a stream. It is useful for creating generic
//! I/O code that is agnostic to the underlying transport.

use std::{future::Future, rc::Rc, time::Instant};

use crate::{BufferResult, BufferSlice, Errno, OwnedFd, operations, try_clone_owned_fd};

/// A trait for asynchronous reading from a stream.
///
/// Each completed operation returns its buffer, including on failure.
/// The buffer must own its storage or contain only static borrows.
/// Implementors must retain kernel-accessible storage until the I/O completes,
/// even if the caller drops or forgets the future.
/// Cancellation does not undo bytes that the operation already transferred.
///
/// ```no_run
/// use kimojio::{AsyncStreamRead, AsyncStreamWrite, BufferSlice, Errno, OwnedFdStream};
///
/// # async fn echo(stream: &mut OwnedFdStream) -> Result<(), Errno> {
/// let (result, buffer) = stream.try_read(vec![0; 4096], None).await;
/// let amount = result?;
/// let (result, buffer) = stream.write(BufferSlice::new(buffer, 0..amount), None).await;
/// result?;
/// let buffer = buffer.into_inner();
/// # drop(buffer);
/// # Ok(())
/// # }
/// ```
///
/// Non-static borrowed buffers cannot enter the I/O path:
///
/// ```compile_fail
/// use kimojio::{AsyncStreamRead, OwnedFdStream};
///
/// async fn borrowed_read(stream: &mut OwnedFdStream) {
///     let mut buffer = vec![0; 16];
///     stream.try_read(buffer.as_mut_slice(), None).await;
/// }
/// ```
pub trait AsyncStreamRead {
    /// Attempts to read data into the buffer, returning the number of bytes read.
    ///
    /// The result can be less than the buffer length. Zero indicates end of stream.
    fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> impl Future<Output = BufferResult<usize, B>>;

    /// Reads exactly enough bytes to fill the buffer.
    ///
    /// Repeatedly reads until the buffer is full or an error occurs.
    /// On failure, the returned buffer can contain bytes from a partial read.
    async fn read<B: AsMut<[u8]> + 'static>(
        &mut self,
        mut buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        let len = buffer.as_mut().len();
        let mut filled = 0;
        while filled < len {
            let (result, returned) = self
                .try_read(BufferSlice::new(buffer, filled..len), deadline)
                .await;
            buffer = returned.into_inner();
            match result {
                Ok(0) => return (Err(Errno::PIPE), buffer),
                Ok(amount) => filled += amount,
                Err(error) => return (Err(error), buffer),
            }
        }
        (Ok(()), buffer)
    }
}

/// A trait for asynchronous writing to a stream.
///
/// Each completed operation returns its buffer, including on failure.
/// Implementors must retain buffer storage and I/O metadata until the kernel
/// completes the operation, even if the caller drops or forgets the future.
///
/// Non-static borrowed buffers cannot enter the I/O path:
///
/// ```compile_fail
/// use kimojio::{AsyncStreamWrite, OwnedFdStream};
///
/// async fn borrowed_write(stream: &mut OwnedFdStream) {
///     let buffer = vec![1; 16];
///     stream.write(buffer.as_slice(), None).await;
/// }
/// ```
pub trait AsyncStreamWrite {
    /// Writes all bytes from the buffer to the stream.
    ///
    /// On failure, the stream can already contain part of the buffer.
    fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> impl Future<Output = BufferResult<(), B>>;

    /// Shuts down the write side of the stream.
    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>>;

    /// Closes the stream entirely.
    fn close(&mut self) -> impl Future<Output = Result<(), Errno>>;

    /// Writes all data from multiple buffers to the stream.
    ///
    /// Returns the original vector and buffer ranges, including on failure.
    ///
    /// ```compile_fail
    /// use kimojio::{AsyncStreamWrite, OwnedFdStream};
    ///
    /// async fn borrowed_writev(stream: &mut OwnedFdStream) {
    ///     let buffer = vec![1; 16];
    ///     stream.writev(vec![buffer.as_slice()], None).await;
    /// }
    /// ```
    async fn writev<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffers: Vec<B>,
        deadline: Option<Instant>,
    ) -> BufferResult<(), Vec<B>> {
        struct SelectedBuffer<B> {
            buffers: Vec<B>,
            index: usize,
        }
        impl<B: AsRef<[u8]>> AsRef<[u8]> for SelectedBuffer<B> {
            fn as_ref(&self) -> &[u8] {
                self.buffers[self.index].as_ref()
            }
        }

        let mut selected = SelectedBuffer { buffers, index: 0 };
        while selected.index < selected.buffers.len() {
            let (result, returned) = self.write(selected, deadline).await;
            selected = returned;
            if let Err(error) = result {
                return (Err(error), selected.buffers);
            }
            selected.index += 1;
        }
        (Ok(()), selected.buffers)
    }
}

/// A trait for streams that can be split into separate read and write halves.
pub trait SplittableStream {
    /// The type of the read half after splitting.
    type ReadStream: AsyncStreamRead;
    /// The type of the write half after splitting.
    type WriteStream: AsyncStreamWrite;

    /// Splits the stream into independent read and write halves.
    async fn split(self) -> Result<(Self::ReadStream, Self::WriteStream), Errno>;
}

struct OwnedFdReadState {
    read_used: usize,
    read_available: usize,
    read_buffer: Option<Box<[u8]>>,
}

/// The read half of an `OwnedFdStream` after splitting.
pub struct OwnedFdStreamRead {
    fd: Option<Rc<OwnedFd>>,
    read_state: OwnedFdReadState,
}

/// The write half of an `OwnedFdStream` after splitting.
pub struct OwnedFdStreamWrite {
    fd: Option<Rc<OwnedFd>>,
}

/// A stream wrapper around an owned file descriptor.
///
/// Provides buffered reading and direct writing with async support.
/// The stream and its I/O stay on one runtime thread.
pub struct OwnedFdStream {
    fd: Option<Rc<OwnedFd>>,
    read_state: OwnedFdReadState,
}

impl OwnedFdStream {
    pub fn new(fd: OwnedFd) -> Self {
        let fd = Some(Rc::new(fd));
        Self {
            fd,
            read_state: OwnedFdReadState {
                read_used: 0,
                read_available: 0,
                read_buffer: Some(Box::new([0; 16384])),
            },
        }
    }

    /// Returns the file descriptor after canceled I/O releases it.
    ///
    /// This method can block until pending I/O completes.
    /// A forgotten future can retain the descriptor indefinitely.
    pub fn into_inner(self) -> Option<OwnedFd> {
        let mut fd = self.fd?;
        loop {
            match Rc::try_unwrap(fd) {
                Ok(fd) => return Some(fd),
                Err(shared) => fd = shared,
            }
            let task_state = crate::task::TaskState::get();
            drop(crate::runtime::submit_and_complete_io_all(task_state, true));
        }
    }
}

impl SplittableStream for OwnedFdStream {
    type ReadStream = OwnedFdStreamRead;
    type WriteStream = OwnedFdStreamWrite;

    async fn split(self) -> Result<(OwnedFdStreamRead, OwnedFdStreamWrite), Errno> {
        let (read_fd, write_fd) = if let Some(fd) = self.fd {
            (Some(Rc::new(try_clone_owned_fd(&fd)?)), Some(fd))
        } else {
            (None, None)
        };
        Ok((
            OwnedFdStreamRead {
                fd: read_fd,
                read_state: self.read_state,
            },
            OwnedFdStreamWrite { fd: write_fd },
        ))
    }
}

async fn try_read_impl<B: AsMut<[u8]> + 'static>(
    fd: &Option<Rc<OwnedFd>>,
    mut buffer: B,
    read_state: &mut OwnedFdReadState,
    deadline: Option<Instant>,
) -> BufferResult<usize, B> {
    let result = async {
        let Some(fd) = fd else {
            return Err(Errno::PIPE);
        };
        if buffer.as_mut().is_empty() {
            return Ok(0);
        }
        if read_state.read_available == 0 {
            let read_buffer = read_state
                .read_buffer
                .take()
                .unwrap_or_else(|| Box::new([0; 16384]));
            let (result, read_buffer) =
                operations::read_owned_with_deadline(fd.clone(), read_buffer, deadline).await;
            read_state.read_buffer = Some(read_buffer);
            let amount = result?;
            if amount == 0 {
                return Ok(0);
            }

            read_state.read_used = 0;
            read_state.read_available = amount;
        }

        let read_used = read_state.read_used;
        let tocopy = std::cmp::min(buffer.as_mut().len(), read_state.read_available);
        read_state.read_used += tocopy;
        read_state.read_available -= tocopy;

        buffer.as_mut()[..tocopy].copy_from_slice(
            &read_state.read_buffer.as_ref().unwrap()[read_used..read_used + tocopy],
        );
        Ok(tocopy)
    }
    .await;
    (result, buffer)
}

impl AsyncStreamRead for OwnedFdStreamRead {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        try_read_impl(&self.fd, buffer, &mut self.read_state, deadline).await
    }
}

impl AsyncStreamRead for OwnedFdStream {
    async fn try_read<B: AsMut<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<usize, B> {
        try_read_impl(&self.fd, buffer, &mut self.read_state, deadline).await
    }
}

async fn write_impl<B: AsRef<[u8]> + 'static>(
    fd: &Option<Rc<OwnedFd>>,
    mut buffer: B,
    deadline: Option<Instant>,
) -> BufferResult<(), B> {
    if let Some(fd) = fd {
        let len = buffer.as_ref().len();
        let mut written = 0;
        while written < len {
            let (result, returned) = operations::write_owned_with_deadline(
                fd.clone(),
                BufferSlice::new(buffer, written..len),
                deadline,
            )
            .await;
            buffer = returned.into_inner();
            match result {
                Ok(0) => return (Err(Errno::PIPE), buffer),
                Ok(amount) => written += amount,
                Err(error) => return (Err(error), buffer),
            }
        }
        (Ok(()), buffer)
    } else {
        (Err(Errno::PIPE), buffer)
    }
}

async fn writev_impl<B: AsRef<[u8]> + 'static>(
    fd: &Option<Rc<OwnedFd>>,
    buffers: Vec<B>,
    deadline: Option<Instant>,
) -> BufferResult<(), Vec<B>> {
    if let Some(fd) = fd {
        operations::writev_owned_with_deadline(fd.clone(), buffers, deadline).await
    } else {
        (Err(Errno::PIPE), buffers)
    }
}

async fn shutdown_impl(fd: &Option<Rc<OwnedFd>>) -> Result<(), Errno> {
    if let Some(fd) = fd {
        operations::shutdown(&**fd, libc::SHUT_RDWR).await?;
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

async fn close_impl(fd: &mut Option<Rc<OwnedFd>>) -> Result<(), Errno> {
    if let Some(fd) = fd.take() {
        operations::close_shared_fd(fd).await?;
        Ok(())
    } else {
        Err(Errno::from_raw_os_error(crate::EPIPE))
    }
}

impl AsyncStreamWrite for OwnedFdStreamWrite {
    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        write_impl(&self.fd, buffer, deadline).await
    }

    async fn writev<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffers: Vec<B>,
        deadline: Option<Instant>,
    ) -> BufferResult<(), Vec<B>> {
        writev_impl(&self.fd, buffers, deadline).await
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>> {
        shutdown_impl(&self.fd)
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_impl(&mut self.fd)
    }
}

impl AsyncStreamWrite for OwnedFdStream {
    async fn write<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffer: B,
        deadline: Option<Instant>,
    ) -> BufferResult<(), B> {
        write_impl(&self.fd, buffer, deadline).await
    }

    async fn writev<B: AsRef<[u8]> + 'static>(
        &mut self,
        buffers: Vec<B>,
        deadline: Option<Instant>,
    ) -> BufferResult<(), Vec<B>> {
        writev_impl(&self.fd, buffers, deadline).await
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<(), Errno>> {
        shutdown_impl(&self.fd)
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Errno>> {
        close_impl(&mut self.fd)
    }
}

#[cfg(test)]
mod test {
    use crate::{
        AsyncStreamRead, AsyncStreamWrite, BufferSlice, OwnedFdStream, SplittableStream,
        operations, pipe::bipipe,
    };
    use rustix::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::io::{Seek, Write};
    use std::{
        cell::Cell,
        rc::Rc,
        time::{Duration, Instant},
    };

    #[crate::test]
    async fn manual_channel_test() {
        let (client, server) = bipipe();
        let mut stream1 = OwnedFdStream::new(client);
        let mut stream2 = OwnedFdStream::new(server);

        let iters = 200usize;
        for iter in 0..iters {
            let mut buffer = [0; 128];
            // let mut buffer = BufferView::new(&mut buffer);
            // buffer.set(&iter.to_le_bytes());
            let id = 1u32;
            // buffer.prefix(&id.to_le_bytes());
            let length: u32 = 12;
            // buffer.prefix(&length.to_le_bytes());
            buffer[0..4].copy_from_slice(&length.to_le_bytes());
            buffer[4..8].copy_from_slice(&id.to_le_bytes());
            buffer[8..16].copy_from_slice(&iter.to_le_bytes());

            stream1
                .write(BufferSlice::new(buffer, 0..16), None)
                .await
                .0
                .unwrap();

            let (result, header) = stream2.read([0; 4], None).await;
            result.unwrap();
            let length = u32::from_le_bytes(header) as usize;
            let (result, response) = stream2.read(vec![0; length], None).await;
            result.unwrap();
            let id = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
            assert_eq!(id, 1);
            let response = &response[4..length];
            assert_eq!(&iter.to_le_bytes(), response);
        }

        stream1.shutdown().await.unwrap();
        stream2.shutdown().await.unwrap();
    }

    #[crate::test]
    async fn try_read_test() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"hello world")
            .expect("Failed to write to file");
        file.seek(std::io::SeekFrom::Start(0)).unwrap();
        let mut stream = OwnedFdStream::new(unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) });

        let (result, buffer) = stream.try_read([0; 100], None).await;
        assert_eq!(result, Ok(11));
        let (result, buffer) = stream.try_read(buffer, None).await;
        assert_eq!(result, Ok(0));
        assert_eq!(&buffer[0..11], b"hello world");
    }

    #[crate::test]
    async fn short_read_test() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"hello world")
            .expect("Failed to write to file");
        file.seek(std::io::SeekFrom::Start(0)).unwrap();

        // go into fd and back
        let stream = OwnedFdStream::new(unsafe { OwnedFd::from_raw_fd(file.into_raw_fd()) });
        let fd = stream.into_inner().unwrap();
        let mut stream = OwnedFdStream::new(fd);

        let (result, buffer) = stream.read([0; 11], None).await;
        result.unwrap();
        assert_eq!(&buffer[0..11], b"hello world");
        let (e, _) = stream.read(buffer, None).await;
        assert!(e.is_err());
    }

    #[crate::test]
    async fn read_failed_test() {
        let (client, server) = bipipe();
        operations::close(server).await.unwrap();

        let mut stream = OwnedFdStream::new(client);
        stream.read([0; 11], None).await.0.unwrap_err();
    }

    #[crate::test]
    async fn owned_fd_stream_split_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Write data using the write stream
        write_stream.write(b"hello split", None).await.0.unwrap();

        // Read data using the read stream from the server side
        let mut server_stream = OwnedFdStream::new(server);
        let (result, buffer) = server_stream.read([0u8; 11], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"hello split");
    }

    #[crate::test]
    async fn owned_fd_stream_read_stream_try_read_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (mut read_stream, _write_stream) = stream.split().await.unwrap();

        // Write data from server side
        let mut server_stream = OwnedFdStream::new(server);
        server_stream.write(b"test try_read", None).await.0.unwrap();

        // Test try_read on the read stream
        let (result, buffer) = read_stream.try_read([0u8; 5], None).await;
        assert_eq!(result, Ok(5));
        assert_eq!(&buffer, b"test ");

        // Read remaining data
        let (result, remaining) = read_stream.try_read([0u8; 8], None).await;
        assert_eq!(result, Ok(8));
        assert_eq!(&remaining, b"try_read");
    }

    #[crate::test]
    async fn owned_fd_stream_read_stream_read_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (mut read_stream, _write_stream) = stream.split().await.unwrap();

        // Test reading exact amount
        let write_task = {
            let mut server_stream = OwnedFdStream::new(server);
            crate::operations::spawn_task(async move {
                server_stream.write(b"exact", None).await.0.unwrap();
                server_stream.write(b"concat", None).await.0.unwrap();
                server_stream
            })
        };

        // Read exact amount first
        let (result, buffer) = read_stream.read([0u8; 5], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"exact");

        // Read concatenated data
        let (result, buffer2) = read_stream.read([0u8; 6], None).await;
        result.unwrap();
        assert_eq!(&buffer2, b"concat");

        write_task.await.unwrap();
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_write_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Test writing various sizes
        write_stream.write(b"small", None).await.0.unwrap();
        write_stream
            .write(b" large data chunk", None)
            .await
            .0
            .unwrap();

        // Read back from server
        let mut server_stream = OwnedFdStream::new(server);
        let (result, buffer) = server_stream.read([0u8; 22], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"small large data chunk");
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_writev_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Test writev with multiple buffers
        let buffers = vec![
            b"hello".to_vec(),
            b" ".to_vec(),
            b"vectored".to_vec(),
            b" write".to_vec(),
        ];

        write_stream.writev(buffers, None).await.0.unwrap();

        // Read back from server
        let mut server_stream = OwnedFdStream::new(server);
        let (result, buffer) = server_stream.read([0u8; 20], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"hello vectored write");
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_shutdown_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Write some data before shutdown
        write_stream
            .write(b"before shutdown", None)
            .await
            .0
            .unwrap();

        // Shutdown the write stream
        write_stream.shutdown().await.unwrap();

        // Server should be able to read existing data
        let mut server_stream = OwnedFdStream::new(server);
        let (result, buffer) = server_stream.read([0u8; 15], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before shutdown");

        // Writing after shutdown should fail
        let (result, _) = write_stream.write(b"after shutdown", None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn owned_fd_stream_write_stream_close_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Write some data before close
        write_stream.write(b"before close", None).await.0.unwrap();

        // Close the write stream
        write_stream.close().await.unwrap();

        // Server should be able to read existing data
        let mut server_stream = OwnedFdStream::new(server);
        let (result, buffer) = server_stream.read([0u8; 12], None).await;
        result.unwrap();
        assert_eq!(&buffer, b"before close");

        // Writing after close should fail
        let (result, _) = write_stream.write(b"after close", None).await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn owned_fd_stream_split_concurrent_operations_test() {
        let (client, server) = bipipe();
        let stream1 = OwnedFdStream::new(client);
        let stream2 = OwnedFdStream::new(server);
        let (read1, write1) = stream1.split().await.unwrap();
        let (read2, write2) = stream2.split().await.unwrap();

        let write_task = {
            let mut write1 = write1;
            let mut write2 = write2;
            crate::operations::spawn_task(async move {
                write1.write(b"from1to2", None).await.0.unwrap();
                write2.write(b"from2to1", None).await.0.unwrap();
                write1.shutdown().await.unwrap();
                write2.shutdown().await.unwrap();
            })
        };

        let read_task = {
            let mut read1 = read1;
            let mut read2 = read2;
            crate::operations::spawn_task(async move {
                // read1 should receive data from write2
                let (result, buffer1) = read1.read([0u8; 8], None).await;
                result.unwrap();
                // read2 should receive data from write1
                let (result, buffer2) = read2.read([0u8; 8], None).await;
                result.unwrap();

                (buffer1, buffer2)
            })
        };

        let (buffer1, buffer2) = read_task.await.unwrap();
        write_task.await.unwrap();

        assert_eq!(&buffer1, b"from2to1");
        assert_eq!(&buffer2, b"from1to2");
    }

    #[crate::test]
    async fn owned_fd_stream_split_writev_large_test() {
        let (client, server) = bipipe();
        let stream = OwnedFdStream::new(client);
        let (_read_stream, mut write_stream) = stream.split().await.unwrap();

        // Test writev with large data that spans multiple buffers
        let buf1 = vec![65u8; 4096]; // 'A' repeated 4096 times
        let buf2 = vec![66u8; 4096]; // 'B' repeated 4096 times
        let buf3 = b"end".to_vec();
        let buffers = vec![buf1, buf2, buf3];

        let read_task = {
            let mut server_stream = OwnedFdStream::new(server);
            crate::operations::spawn_task(async move {
                // Read data in chunks
                let mut result = Vec::new();
                for _ in 0..8 {
                    let (read_result, temp_buffer) = server_stream.read([0u8; 1024], None).await;
                    read_result.unwrap();
                    result.extend_from_slice(&temp_buffer);
                }
                let (read_result, end_buffer) = server_stream.read([0u8; 3], None).await;
                read_result.unwrap();
                result.extend_from_slice(&end_buffer);
                result
            })
        };

        write_stream.writev(buffers, None).await.0.unwrap();

        let result = read_task.await.unwrap();

        // Verify the data
        assert_eq!(result.len(), 8195); // 4096 + 4096 + 3
        assert!(result[0..4096].iter().all(|&x| x == 65)); // All 'A's
        assert!(result[4096..8192].iter().all(|&x| x == 66)); // All 'B's
        assert_eq!(&result[8192..8195], b"end");
    }

    #[crate::test]
    async fn owned_buffers_and_ranges_are_returned_even_on_error() {
        let (client, server) = bipipe();
        let mut writer = OwnedFdStream::new(client);
        let mut reader = OwnedFdStream::new(server);
        let message = b"hello".to_vec();
        let message_address = message.as_ptr();
        let (result, message) = writer.write(message, None).await;
        result.unwrap();
        assert_eq!(message.as_ptr(), message_address);
        assert_eq!(message, b"hello");

        let buffer = vec![0xa5; 12];
        let buffer_address = buffer.as_ptr();
        let (result, buffer) = reader.read(BufferSlice::new(buffer, 3..8), None).await;
        result.unwrap();
        let buffer = buffer.into_inner();
        assert_eq!(buffer.as_ptr(), buffer_address);
        assert_eq!(&buffer[..3], &[0xa5; 3]);
        assert_eq!(&buffer[3..8], b"hello");
        assert_eq!(&buffer[8..], &[0xa5; 4]);

        let expired = Some(Instant::now() - Duration::from_secs(1));
        let (result, buffer) = reader.try_read(buffer, expired).await;
        assert_eq!(result, Err(crate::Errno::TIMEDOUT));
        assert_eq!(buffer.as_ptr(), buffer_address);
        let (result, buffer) = reader
            .try_read(buffer, Some(Instant::now() + Duration::from_millis(1)))
            .await;
        assert_eq!(result, Err(crate::Errno::TIME));
        assert_eq!(buffer.as_ptr(), buffer_address);
        let (result, message) = writer.write(message, expired).await;
        assert_eq!(result, Err(crate::Errno::TIMEDOUT));
        assert_eq!(message.as_ptr(), message_address);

        writer.close().await.unwrap();
        let (result, message) = writer.write(message, None).await;
        assert_eq!(result, Err(crate::Errno::PIPE));
        assert_eq!(message.as_ptr(), message_address);
    }

    #[crate::test]
    async fn prefix_buffers_transfer_only_the_selected_contents() {
        use crate::{OwnedBuffer, PrefixBuffer, StaticBuffer};
        let (client, server) = bipipe();
        let mut writer = OwnedFdStream::new(client);
        let mut reader = OwnedFdStream::new(server);
        let outgoing = OwnedBuffer::with(64, b"owned");
        let address = outgoing.as_ref().as_ptr();
        let (result, outgoing) = writer.write(outgoing, None).await;
        result.unwrap();
        assert_eq!(outgoing.as_ref().as_ptr(), address);

        let mut incoming = StaticBuffer::<64>::new();
        incoming.reserve(5);
        let (result, incoming) = reader.read(incoming, None).await;
        result.unwrap();
        assert_eq!(incoming.as_ref(), b"owned");
        assert_eq!(incoming.available(), 59);
    }

    #[crate::test]
    async fn exact_read_returns_partial_contents_on_eof() {
        let (client, server) = bipipe();
        let mut writer = OwnedFdStream::new(client);
        let mut reader = OwnedFdStream::new(server);
        writer.write(b"hi", None).await.0.unwrap();
        writer.close().await.unwrap();
        let (result, buffer) = reader.read([0xa5; 4], None).await;
        assert_eq!(result, Err(crate::Errno::PIPE));
        assert_eq!(buffer, [b'h', b'i', 0xa5, 0xa5]);
    }

    #[crate::test]
    async fn canceled_buffered_read_can_be_restarted() {
        let (client, server) = bipipe();
        let mut reader = OwnedFdStream::new(client);
        {
            let mut read = Box::pin(reader.try_read(vec![0; 8], None));
            assert!(futures::poll!(read.as_mut()).is_pending());
        }
        assert!(reader.read_state.read_buffer.is_none());
        for _ in 0..1000 {
            operations::nop().await.unwrap();
            if Rc::strong_count(reader.fd.as_ref().unwrap()) == 1 {
                break;
            }
        }
        assert_eq!(Rc::strong_count(reader.fd.as_ref().unwrap()), 1);
        operations::write(&server, b"new data").await.unwrap();
        let (result, buffer) = reader.read(vec![0; 8], None).await;
        result.unwrap();
        assert_eq!(buffer, b"new data");
        assert!(reader.read_state.read_buffer.is_some());
    }

    #[crate::test]
    async fn into_inner_waits_for_canceled_io_to_release_the_descriptor() {
        let (client, server) = bipipe();
        let mut reader = OwnedFdStream::new(client);
        {
            let mut read = Box::pin(reader.try_read(vec![0; 8], None));
            assert!(futures::poll!(read.as_mut()).is_pending());
        }
        let fd = reader.into_inner().unwrap();
        operations::write(&server, b"returned").await.unwrap();
        let mut buffer = [0; 8];
        assert_eq!(operations::read(&fd, &mut buffer).await, Ok(8));
        assert_eq!(&buffer, b"returned");
    }

    #[crate::test]
    async fn vectored_write_preserves_owners_across_partial_writes() {
        let (client, server) = bipipe();
        rustix::net::sockopt::set_socket_send_buffer_size(&client, 4096).unwrap();
        let mut writer = OwnedFdStream::new(client);
        let mut reader = OwnedFdStream::new(server);
        let buffers = vec![
            Vec::new(),
            vec![1; 65536],
            vec![2; 65539],
            Vec::new(),
            vec![3; 65536],
        ];
        let expected = buffers.concat();
        let owners = buffers.as_ptr();
        let addresses: Vec<_> = buffers.iter().map(|buffer| buffer.as_ptr()).collect();
        let read = operations::spawn_task(async move {
            let (result, buffer) = reader.read(vec![0; expected.len()], None).await;
            result.unwrap();
            assert_eq!(buffer, expected);
        });
        let (result, buffers) = writer.writev(buffers, None).await;
        result.unwrap();
        assert_eq!(buffers.as_ptr(), owners);
        assert_eq!(
            buffers
                .iter()
                .map(|buffer| buffer.as_ptr())
                .collect::<Vec<_>>(),
            addresses
        );
        read.await.unwrap();
    }

    struct TrackedWriteBuffer {
        bytes: [u8; 16],
        drops: Rc<Cell<usize>>,
    }

    impl AsRef<[u8]> for TrackedWriteBuffer {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }

    impl Drop for TrackedWriteBuffer {
        fn drop(&mut self) {
            drop(crate::task::TaskState::get());
            self.drops.set(self.drops.get() + 1);
        }
    }

    #[crate::test]
    async fn dropped_writes_retain_buffers_and_descriptors_until_completion() {
        for vectored in [false, true] {
            let (client, _server) = bipipe();
            let mut writer = OwnedFdStream::new(client);
            let descriptor = Rc::downgrade(writer.fd.as_ref().unwrap());
            let drops = Rc::new(Cell::new(0));
            if vectored {
                let buffers = vec![
                    TrackedWriteBuffer {
                        bytes: [1; 16],
                        drops: drops.clone(),
                    },
                    TrackedWriteBuffer {
                        bytes: [2; 16],
                        drops: drops.clone(),
                    },
                ];
                let mut write = Box::pin(writer.writev(buffers, None));
                assert!(futures::poll!(write.as_mut()).is_pending());
                drop(write);
            } else {
                let buffer = TrackedWriteBuffer {
                    bytes: [1; 16],
                    drops: drops.clone(),
                };
                let mut write = Box::pin(writer.write(buffer, None));
                assert!(futures::poll!(write.as_mut()).is_pending());
                drop(write);
            }
            drop(writer);
            assert_eq!(drops.get(), 0);
            assert!(descriptor.upgrade().is_some());
            let expected_drops = if vectored { 2 } else { 1 };
            for _ in 0..1000 {
                operations::nop().await.unwrap();
                if drops.get() == expected_drops {
                    break;
                }
            }
            assert_eq!(drops.get(), expected_drops);
            assert!(descriptor.upgrade().is_none());
        }
    }
}
