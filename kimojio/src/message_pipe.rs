// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use crate::{
    CompletionResources, Errno, OwnedFd,
    io_type::IOType,
    operations,
    pointer_buffer::{IdPointerMsg, pointer_from_buffer_ref},
    pointer_from_buffer, pointer_to_buffer,
};
use futures::future::FusedFuture;
use std::{
    cell::RefCell,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};

const POINTER_SIZE: usize = std::mem::size_of::<*const ()>();

type MessagePipePair<T1, T2> = (MessagePipe<T1, T2>, MessagePipe<T2, T1>);

/// Create a pair of message pipes that can be used to send messages
/// across threads within the same process.
///
/// NOTE: both pipes can be used to send and receive, so you may not
/// get the drop detection you expect. Use make_message_pipe_oneway
/// if you require drop detection.
pub fn make_message_pipe<T1: Send, T2: Send>() -> MessagePipePair<T1, T2> {
    let (pipe1, pipe2) = crate::pipe::bipipe();
    (MessagePipe::new(pipe1), MessagePipe::new(pipe2))
}

/// Create a message pipe that can be used to send messages across threads
/// within the same process.
pub async fn make_message_pipe_oneway<T: Send>()
-> Result<(MessagePipeSender<T>, MessagePipeReceiver<T>), Errno> {
    let (pipe1, pipe2) = crate::pipe::bipipe();
    Ok((
        MessagePipeSender::new(pipe1).await?,
        MessagePipeReceiver::new(pipe2).await?,
    ))
}

/// Create a message pipe that can be used to send messages across threads
/// within the same process.
pub fn make_message_pipe_oneway_sync<T: Send>()
-> Result<(MessagePipeSender<T>, MessagePipeReceiver<T>), Errno> {
    let (pipe1, pipe2) = crate::pipe::bipipe();
    Ok((
        MessagePipeSender::new_sync(pipe1)?,
        MessagePipeReceiver::new_sync(pipe2)?,
    ))
}

/// An in process pipe for sending messages of type T and receiving
/// messages of type R. Can be used across threads but not between
/// processes.
///
/// `send_message` and `recv_message` must be called from uringruntime
/// threads and can be used to send messages between tasks in that
/// therad.
///
/// `send_message_sync` and `recv_message_sync` are blocking calls that
/// can be used to send or receive a message from any thread.  To send
/// messages from another thread to a uringruntime thread, use these
/// methods from the non-uringruntime thread and use `recv_message` and
/// `send_message` on the recipient uringruntime thread.
#[derive(Debug)]
pub struct MessagePipe<T: Send, R: Send> {
    pipe: OwnedFd,

    _marker: PhantomData<(T, R)>,
}

impl<T: Send, R: Send> MessagePipe<T, R> {
    pub fn new(pipe: OwnedFd) -> Self {
        Self {
            pipe,
            _marker: PhantomData,
        }
    }

    pub fn into_inner(self) -> OwnedFd {
        self.pipe
    }

    /// Send a message on the pipe from a uringruntime thread. Message must be
    /// boxed as it is sent in-process as a pointer.
    ///
    /// Cancellation waits for the I/O result. If transmission succeeded, the
    /// receiver owns the message. Otherwise, cancellation drops the message.
    pub async fn send_message(&self, message: Box<T>) -> Result<(), Errno> {
        let buffer = pointer_to_buffer(message);
        SendMessageFuture::<T>::new(&self.pipe, &buffer).await
    }

    /// Receive a message on the pipe. This must be called from a uringruntime thread.
    pub fn recv_message(&self) -> RecvMessageFuture<'_, R> {
        self.recv_message_with_timeout(None)
    }

    /// Receive a message on the pipe with an optional timeout.
    /// This must be called from a uringruntime thread.
    pub fn recv_message_with_timeout(&self, timeout: Option<Duration>) -> RecvMessageFuture<'_, R> {
        RecvMessageFuture {
            inner: ReadMessageFuture::new(&self.pipe, timeout),
        }
    }

    /// Send a message on the pipe from any thread. `message` must be boxed as
    /// it is sent in-process as a pointer. This is a blocking call and should
    /// never be called from an async task.
    pub fn send_message_sync(&self, message: Box<T>) -> Result<(), Errno> {
        let buffer = pointer_to_buffer(message);
        match rustix::io::write(&self.pipe, &buffer) {
            Ok(amount) => {
                assert!(amount == buffer.len());
                Ok(())
            }
            Err(e) => {
                // Reconstitute Box to prevent leaking on write failure
                unsafe {
                    // SAFETY: since write failed, buffer is only copy of the pointer
                    pointer_from_buffer::<T>(buffer);
                }
                Err(e)
            }
        }
    }

    /// Receive a message on the pipe. This is a blocking call and should never
    /// be called from an async task.
    pub fn recv_message_sync(&self) -> Result<Box<R>, Errno> {
        let mut buffer = [0u8; POINTER_SIZE];
        let amount = rustix::io::read(&self.pipe, &mut buffer)?;
        if amount == 0 {
            return Err(Errno::from_raw_os_error(libc::EPIPE));
        }
        assert_eq!(amount, buffer.len());
        unsafe {
            // SAFETY: The read bytes are the only copy of the pointer
            Ok(pointer_from_buffer(buffer))
        }
    }
}

#[derive(Debug)]
pub struct MessagePipeReceiver<R: Send> {
    pipe: OwnedFd,

    _marker: PhantomData<R>,
}

impl<R: Send> MessagePipeReceiver<R> {
    pub async fn new(pipe: OwnedFd) -> Result<Self, Errno> {
        operations::shutdown(&pipe, rustix::net::Shutdown::Write as i32).await?;
        Ok(Self {
            pipe,
            _marker: PhantomData,
        })
    }

    pub fn new_sync(pipe: OwnedFd) -> Result<Self, Errno> {
        rustix::net::shutdown(&pipe, rustix::net::Shutdown::Write)?;
        Ok(Self {
            pipe,
            _marker: PhantomData,
        })
    }

    pub fn into_inner(self) -> OwnedFd {
        self.pipe
    }

    /// Receive a message on the pipe. This must be called from a uringruntime thread.
    pub fn recv_message(&self) -> RecvMessageFuture<'_, R> {
        self.recv_message_with_timeout(None)
    }

    /// Receive a message on the pipe with an optional timeout.
    /// This must be called from a uringruntime thread.
    pub fn recv_message_with_timeout(&self, timeout: Option<Duration>) -> RecvMessageFuture<'_, R> {
        RecvMessageFuture {
            inner: ReadMessageFuture::new(&self.pipe, timeout),
        }
    }
}

impl<R: Send> Clone for MessagePipeReceiver<R> {
    fn clone(&self) -> Self {
        Self {
            pipe: self.pipe.try_clone().unwrap(),
            _marker: Default::default(),
        }
    }
}

#[derive(Debug)]
pub struct MessagePipeSender<T: Send> {
    pipe: OwnedFd,

    _marker: PhantomData<T>,
}

impl<T: Send> MessagePipeSender<T> {
    pub async fn new(pipe: OwnedFd) -> Result<Self, Errno> {
        operations::shutdown(&pipe, rustix::net::Shutdown::Read as i32).await?;
        Ok(Self {
            pipe,
            _marker: PhantomData,
        })
    }

    pub fn new_sync(pipe: OwnedFd) -> Result<Self, Errno> {
        rustix::net::shutdown(&pipe, rustix::net::Shutdown::Read)?;
        Ok(Self {
            pipe,
            _marker: PhantomData,
        })
    }

    pub fn into_inner(self) -> OwnedFd {
        self.pipe
    }

    /// Send a message on the pipe from a uringruntime thread. Message must be
    /// boxed as it is sent in-process as a pointer.
    ///
    /// Cancellation waits for the I/O result. If transmission succeeded, the
    /// receiver owns the message. Otherwise, cancellation drops the message.
    pub async fn send_message(&self, message: Box<T>) -> Result<(), Errno> {
        let buffer = pointer_to_buffer(message);
        SendMessageFuture::<T>::new(&self.pipe, &buffer).await
    }

    /// Send a message on the pipe from any thread. `message` must be boxed as
    /// it is sent in-process as a pointer. This is a blocking call and should
    /// never be called from an async task.
    pub fn send_message_sync(&self, message: Box<T>) -> Result<(), Errno> {
        let buffer = pointer_to_buffer(message);
        match rustix::io::write(&self.pipe, &buffer) {
            Ok(amount) => {
                assert!(amount == buffer.len());
                Ok(())
            }
            Err(e) => {
                // Reconstitute Box to prevent leaking on write failure
                unsafe {
                    // SAFETY
                    // since write failed, buffer is only copy of the pointer
                    pointer_from_buffer::<T>(buffer);
                }
                Err(e)
            }
        }
    }
}

impl<T: Send> Clone for MessagePipeSender<T> {
    fn clone(&self) -> Self {
        Self {
            pipe: self.pipe.try_clone().unwrap(),
            _marker: Default::default(),
        }
    }
}

struct SendMessageFuture<'a, T> {
    fut: crate::ring_future::UsizeFuture<'a>,
    buffer: &'a [u8],
    completed: bool,
    _marker: PhantomData<fn() -> T>,
}

impl<'a, T> SendMessageFuture<'a, T> {
    fn new(pipe: &'a OwnedFd, buffer: &'a [u8]) -> Self {
        Self {
            fut: operations::write_with_timeout(pipe, buffer, None),
            buffer,
            completed: false,
            _marker: PhantomData,
        }
    }

    fn finish(&mut self, result: Result<usize, Errno>) -> Result<(), Errno> {
        self.completed = true;
        match result {
            Ok(amount) => {
                assert_eq!(amount, self.buffer.len(), "partial message write");
                Ok(())
            }
            Err(error) => {
                // SAFETY: Failed writes do not transfer the pointer to the peer.
                unsafe { drop(message_from_bytes::<T>(self.buffer)) };
                Err(error)
            }
        }
    }
}

impl<T> Future for SendMessageFuture<'_, T> {
    type Output = Result<(), Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.fut).poll(cx) {
            Poll::Ready(result) => Poll::Ready(this.finish(result)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for SendMessageFuture<'_, T> {
    fn drop(&mut self) {
        if !self.completed {
            let result = self.fut.cancel_and_complete().unwrap();
            let _ = self.finish(result);
        }
    }
}

unsafe fn message_from_bytes<T>(buffer: &[u8]) -> Box<T> {
    let pointer = buffer[buffer.len() - POINTER_SIZE..].try_into().unwrap();
    // SAFETY: The caller owns the complete pointer in the final bytes.
    unsafe { pointer_from_buffer_ref(pointer) }
}

struct ReadMessageFuture<'a, T, const N: usize> {
    fut: crate::ring_future::UsizeFuture<'a>,
    buffer: Rc<RefCell<[u8; N]>>,
    _marker: PhantomData<fn() -> T>,
}

impl<'a, T, const N: usize> ReadMessageFuture<'a, T, N> {
    fn new(pipe: &'a OwnedFd, timeout: Option<Duration>) -> Self {
        use std::os::fd::AsRawFd;
        let buffer = Rc::new(RefCell::new([0u8; N]));
        let fd = pipe.as_raw_fd();
        let entry = rustix_uring::opcode::Read::new(
            rustix_uring::types::Fd(fd),
            buffer.borrow_mut().as_mut_ptr(),
            N as u32,
        )
        .offset(u64::MAX)
        .build();
        let fut = crate::ring_future::UsizeFuture::with_polled(
            entry,
            fd,
            timeout,
            IOType::Read,
            false,
            CompletionResources::Rc(buffer.clone()),
        );
        Self {
            fut,
            buffer,
            _marker: PhantomData,
        }
    }
}

impl<T, const N: usize> Future for ReadMessageFuture<'_, T, N> {
    type Output = Result<[u8; N], Errno>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.fut).poll(cx) {
            Poll::Ready(Ok(0)) => Poll::Ready(Err(Errno::PIPE)),
            Poll::Ready(Ok(amount)) => {
                assert_eq!(amount, N, "partial message read");
                Poll::Ready(Ok(*this.buffer.borrow()))
            }
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T, const N: usize> Drop for ReadMessageFuture<'_, T, N> {
    fn drop(&mut self) {
        if let Some(Ok(amount)) = self.fut.cancel_and_complete()
            && amount != 0
        {
            assert_eq!(amount, N, "partial message read");
            // SAFETY: The completed read consumed this pointer, but its caller
            // never received it. Cancellation has finished all access to the buffer.
            unsafe { drop(message_from_bytes::<T>(&*self.buffer.borrow())) };
        }
    }
}

/// A message receive operation.
///
/// Cancellation waits for the I/O result and drops any message already
/// consumed by the read. Such a message is not available to a later receive.
pub struct RecvMessageFuture<'a, T> {
    inner: ReadMessageFuture<'a, T, POINTER_SIZE>,
}

impl<'a, T> Future for RecvMessageFuture<'a, T> {
    type Output = Result<Box<T>, Errno>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        use std::task::Poll;

        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll(cx) {
            Poll::Ready(Ok(buffer)) => {
                let result = unsafe {
                    // SAFETY: The read bytes are the only copy of the pointer
                    pointer_from_buffer(buffer)
                };
                Poll::Ready(Ok(result))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<'a, T> FusedFuture for RecvMessageFuture<'a, T> {
    fn is_terminated(&self) -> bool {
        self.inner.fut.is_terminated()
    }
}

// Same as MessagePipe, but send and receive has an id param.
pub struct IdMessagePipe<T: Send, R: Send> {
    pipe: OwnedFd,

    _marker: PhantomData<(T, R)>,
}

impl<T: Send, R: Send> IdMessagePipe<T, R> {
    pub fn new(pipe: OwnedFd) -> Self {
        Self {
            pipe,
            _marker: PhantomData,
        }
    }

    pub fn into_inner(self) -> OwnedFd {
        self.pipe
    }

    /// Send a message on the pipe from a uringruntime thread. Message must be
    /// boxed as it is sent in-process as a pointer.
    ///
    /// Cancellation waits for the I/O result before releasing ownership.
    pub async fn send_message(&self, id: u64, message: Box<T>) -> Result<(), Errno> {
        let buffer = IdPointerMsg::from_id_box(id, message).into_buffer();
        SendMessageFuture::<T>::new(&self.pipe, &buffer).await
    }

    /// Receive a message on the pipe. This must be called from a uringruntime thread.
    ///
    /// Cancellation waits for the I/O result and drops any consumed message.
    pub async fn recv_message(&self) -> Result<(u64, Box<R>), Errno> {
        let buffer = ReadMessageFuture::<R, { 2 * POINTER_SIZE }>::new(&self.pipe, None).await?;
        Ok(IdPointerMsg::<R>::new(buffer).into_id_box())
    }

    /// Send a message on the pipe from any thread. `message` must be boxed as
    /// it is sent in-process as a pointer. This is a blocking call and should
    /// never be called from an async task.
    pub fn send_message_sync(&self, id: u64, message: Box<T>) -> Result<(), Errno> {
        let buffer = IdPointerMsg::<T>::from_id_box(id, message);
        let buffer_ref = buffer.as_ref();
        match rustix::io::write(&self.pipe, buffer_ref) {
            Ok(amount) => {
                assert!(amount == buffer_ref.len());
                // If we succeeded in writing to pipe, then we no longer own the pointer and should
                // not drop it.
                std::mem::forget(buffer);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Receive a message on the pipe. This is a blocking call and should never
    /// be called from an async task.
    pub fn recv_message_sync(&self) -> Result<(u64, Box<R>), Errno> {
        let mut buffer = [0u8; 2 * POINTER_SIZE];
        let amount = rustix::io::read(&self.pipe, &mut buffer)?;
        if amount == 0 {
            return Err(Errno::from_raw_os_error(libc::EPIPE));
        }
        assert_eq!(amount, buffer.len());
        Ok(IdPointerMsg::<R>::new(buffer).into_id_box())
    }
}

impl<T: Send, R: Send> Clone for MessagePipe<T, R> {
    fn clone(&self) -> Self {
        Self {
            pipe: self.pipe.try_clone().unwrap(),
            _marker: Default::default(),
        }
    }
}

#[cfg(test)]
mod test {
    use super::{IdMessagePipe, MessagePipe, make_message_pipe, make_message_pipe_oneway_sync};
    use crate::{MessagePipeReceiver, MessagePipeSender, make_message_pipe_oneway};
    use futures::{FutureExt, future::FusedFuture};
    use std::time::Duration;

    struct DropCounter(std::sync::Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    fn tracked_message() -> (
        Box<DropCounter>,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (Box::new(DropCounter(count.clone())), count)
    }

    fn fill_socket(fd: &crate::OwnedFd) {
        rustix::fs::fcntl_setfl(fd, rustix::fs::OFlags::NONBLOCK).unwrap();
        loop {
            match rustix::net::send(fd, &[0; 4096], rustix::net::SendFlags::DONTWAIT) {
                Ok(_) => {}
                Err(crate::Errno::AGAIN) => break,
                Err(error) => panic!("failed to fill socket: {error}"),
            }
        }
    }

    #[crate::test]
    async fn canceled_sends_drop_unsent_messages() {
        let (fd, _peer) = crate::pipe::bipipe();
        fill_socket(&fd);
        let sender = MessagePipe::<DropCounter, ()>::new(fd);
        let (message, count) = tracked_message();
        {
            let mut send = std::pin::pin!(sender.send_message(message));
            assert!(futures::poll!(send.as_mut()).is_pending());
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (fd, _peer) = crate::pipe::bipipe();
        fill_socket(&fd);
        let sender = MessagePipeSender::<DropCounter>::new_sync(fd).unwrap();
        let (message, count) = tracked_message();
        {
            let mut send = std::pin::pin!(sender.send_message(message));
            assert!(futures::poll!(send.as_mut()).is_pending());
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (fd, _peer) = crate::pipe::bipipe();
        fill_socket(&fd);
        let sender = IdMessagePipe::<DropCounter, ()>::new(fd);
        let (message, count) = tracked_message();
        {
            let mut send = std::pin::pin!(sender.send_message(42, message));
            assert!(futures::poll!(send.as_mut()).is_pending());
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[crate::test]
    async fn dropped_sends_preserve_transmitted_messages() {
        let (sender, receiver) = make_message_pipe::<DropCounter, ()>();
        let (message, count) = tracked_message();
        let mut send = Box::pin(sender.send_message(message));
        assert!(futures::poll!(send.as_mut()).is_pending());
        let received = receiver.recv_message().await.unwrap();
        drop(send);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(received);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (fd, peer) = crate::pipe::bipipe();
        let sender = IdMessagePipe::<DropCounter, ()>::new(fd);
        let receiver = IdMessagePipe::<(), DropCounter>::new(peer);
        let (message, count) = tracked_message();
        let mut send = Box::pin(sender.send_message(42, message));
        assert!(futures::poll!(send.as_mut()).is_pending());
        let (id, received) = receiver.recv_message().await.unwrap();
        assert_eq!(id, 42);
        drop(send);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(received);
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[crate::test]
    async fn dropped_receives_release_consumed_messages() {
        let (sender, receiver) = make_message_pipe::<DropCounter, ()>();
        let (message, count) = tracked_message();
        sender.send_message_sync(message).unwrap();
        {
            let mut receive = std::pin::pin!(receiver.recv_message());
            assert!(futures::poll!(receive.as_mut()).is_pending());
            crate::operations::nop().await.unwrap();
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (sender, receiver) = make_message_pipe_oneway_sync::<DropCounter>().unwrap();
        let (message, count) = tracked_message();
        sender.send_message_sync(message).unwrap();
        {
            let mut receive = std::pin::pin!(receiver.recv_message());
            assert!(futures::poll!(receive.as_mut()).is_pending());
            crate::operations::nop().await.unwrap();
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);

        let (fd, peer) = crate::pipe::bipipe();
        let sender = IdMessagePipe::<DropCounter, ()>::new(fd);
        let receiver = IdMessagePipe::<(), DropCounter>::new(peer);
        let (message, count) = tracked_message();
        sender.send_message_sync(42, message).unwrap();
        {
            let mut receive = std::pin::pin!(receiver.recv_message());
            assert!(futures::poll!(receive.as_mut()).is_pending());
            crate::operations::nop().await.unwrap();
        }
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[crate::test]
    async fn unpolled_operations_preserve_ownership() {
        let (sender, receiver) = make_message_pipe::<DropCounter, ()>();
        let (message, count) = tracked_message();
        drop(sender.send_message(message));
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
        let (message, count) = tracked_message();
        sender.send_message_sync(message).unwrap();
        drop(receiver.recv_message());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
        drop(receiver.recv_message().await.unwrap());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[crate::test]
    async fn pending_receive_cancellation_allows_reuse() {
        let (sender, receiver) = make_message_pipe::<DropCounter, ()>();
        {
            let mut receive = std::pin::pin!(receiver.recv_message());
            assert!(futures::poll!(receive.as_mut()).is_pending());
        }
        let (message, count) = tracked_message();
        sender.send_message_sync(message).unwrap();
        drop(receiver.recv_message().await.unwrap());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[crate::test]
    async fn test_message_pipe_behavior_send() {
        let (tx, rx): (MessagePipeSender<String>, MessagePipeReceiver<String>) =
            make_message_pipe_oneway().await.unwrap();

        let rx2 = rx.clone();
        drop(rx);
        drop(rx2);

        match tx.send_message(Box::new("message".to_string())).await {
            Ok(_) => panic!("Successfully sent message, should have errored out"),
            Err(err) => println!("Errored out {err:?} while sending message, as expected"),
        }
    }

    #[crate::test]
    async fn test_message_pipe_behavior_recv() {
        let (tx, rx): (MessagePipeSender<String>, MessagePipeReceiver<String>) =
            make_message_pipe_oneway().await.unwrap();

        let tx2 = tx.clone();

        drop(tx);
        drop(tx2);

        match rx.recv_message().await {
            Ok(_) => panic!("Successfully received message, should have errored out"),
            Err(err) => println!("Errored out {err:?} while receiving message, as expected"),
        }
    }

    #[crate::test]
    async fn test_make_message_pipe() {
        let (pipe1, pipe2): (MessagePipe<String, i32>, MessagePipe<i32, String>) =
            make_message_pipe();

        // Test bidirectional communication
        let send_task = crate::operations::spawn_task(async move {
            pipe1
                .send_message(Box::new("hello".to_string()))
                .await
                .unwrap();
            let response = pipe1.recv_message().await.unwrap();
            assert_eq!(*response, 42);
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = pipe2.recv_message().await.unwrap();
            assert_eq!(*message, "hello");
            pipe2.send_message(Box::new(42)).await.unwrap();
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_make_message_pipe_oneway_sync() {
        let (tx, rx): (MessagePipeSender<String>, MessagePipeReceiver<String>) =
            make_message_pipe_oneway_sync().unwrap();

        let send_task = crate::operations::spawn_task(async move {
            tx.send_message(Box::new("sync test".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = rx.recv_message().await.unwrap();
            assert_eq!(*message, "sync test");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_new_and_into_inner() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let msg_pipe: MessagePipe<String, i32> = MessagePipe::new(pipe1);

        // Test into_inner
        let fd = msg_pipe.into_inner();

        // Create new message pipe and test basic communication
        let msg_pipe2: MessagePipe<i32, String> = MessagePipe::new(pipe2);
        let msg_pipe1: MessagePipe<String, i32> = MessagePipe::new(fd);

        let send_task = crate::operations::spawn_task(async move {
            msg_pipe1
                .send_message(Box::new("test".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = msg_pipe2.recv_message().await.unwrap();
            assert_eq!(*message, "test");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_recv_with_timeout() {
        let (pipe1, pipe2): (MessagePipe<String, i32>, MessagePipe<i32, String>) =
            make_message_pipe();

        // Test timeout
        let timeout_result = pipe1
            .recv_message_with_timeout(Some(Duration::from_millis(10)))
            .await;
        assert!(timeout_result.is_err());

        // Test successful receive with timeout
        let send_task = crate::operations::spawn_task(async move {
            pipe2.send_message(Box::new(123)).await.unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = pipe1
                .recv_message_with_timeout(Some(Duration::from_millis(100)))
                .await
                .unwrap();
            assert_eq!(*message, 123);
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_sync_operations() {
        let (pipe1, pipe2): (MessagePipe<String, i32>, MessagePipe<i32, String>) =
            make_message_pipe();

        let sync_task = std::thread::spawn(move || {
            // Test sync send
            pipe1
                .send_message_sync(Box::new("sync message".to_string()))
                .unwrap();

            // Test sync receive
            let response = pipe1.recv_message_sync().unwrap();
            assert_eq!(*response, 456);
        });

        let async_task = crate::operations::spawn_task(async move {
            // Receive sync message
            let message = pipe2.recv_message().await.unwrap();
            assert_eq!(*message, "sync message");

            // Send response
            pipe2.send_message(Box::new(456)).await.unwrap();
        });

        async_task.await.unwrap();
        sync_task.join().unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_sender_new_sync_and_into_inner() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let sender: MessagePipeSender<String> = MessagePipeSender::new_sync(pipe1).unwrap();

        // Test into_inner
        let fd = sender.into_inner();

        // Create receiver and test communication
        let receiver: MessagePipeReceiver<String> = MessagePipeReceiver::new(pipe2).await.unwrap();
        let sender: MessagePipeSender<String> = MessagePipeSender::new(fd).await.unwrap();

        let send_task = crate::operations::spawn_task(async move {
            sender
                .send_message(Box::new("test sender".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = receiver.recv_message().await.unwrap();
            assert_eq!(*message, "test sender");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_sender_sync_operations() {
        let (tx, rx): (MessagePipeSender<String>, MessagePipeReceiver<String>) =
            make_message_pipe_oneway_sync().unwrap();

        let sync_task = std::thread::spawn(move || {
            tx.send_message_sync(Box::new("sync sender test".to_string()))
                .unwrap();
        });

        let async_task = crate::operations::spawn_task(async move {
            let message = rx.recv_message().await.unwrap();
            assert_eq!(*message, "sync sender test");
        });

        sync_task.join().unwrap();
        async_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_receiver_new_sync_and_into_inner() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let receiver: MessagePipeReceiver<String> = MessagePipeReceiver::new_sync(pipe2).unwrap();

        // Test into_inner
        let fd = receiver.into_inner();

        // Create sender and test communication
        let sender: MessagePipeSender<String> = MessagePipeSender::new(pipe1).await.unwrap();
        let receiver: MessagePipeReceiver<String> = MessagePipeReceiver::new(fd).await.unwrap();

        let send_task = crate::operations::spawn_task(async move {
            sender
                .send_message(Box::new("test receiver".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = receiver.recv_message().await.unwrap();
            assert_eq!(*message, "test receiver");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_receiver_recv_with_timeout() {
        let (tx, rx): (MessagePipeSender<String>, MessagePipeReceiver<String>) =
            make_message_pipe_oneway().await.unwrap();

        // Test timeout
        let timeout_result = rx
            .recv_message_with_timeout(Some(Duration::from_millis(10)))
            .await;
        assert!(timeout_result.is_err());

        // Test successful receive with timeout
        let send_task = crate::operations::spawn_task(async move {
            tx.send_message(Box::new("timeout test".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = rx
                .recv_message_with_timeout(Some(Duration::from_millis(100)))
                .await
                .unwrap();
            assert_eq!(*message, "timeout test");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_id_message_pipe_new_and_into_inner() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let id_pipe: IdMessagePipe<String, i32> = IdMessagePipe::new(pipe1);

        // Test into_inner
        let fd = id_pipe.into_inner();

        // Create new id message pipes and test communication
        let id_pipe1: IdMessagePipe<String, i32> = IdMessagePipe::new(fd);
        let id_pipe2: IdMessagePipe<i32, String> = IdMessagePipe::new(pipe2);

        let send_task = crate::operations::spawn_task(async move {
            id_pipe1
                .send_message(123, Box::new("id test".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let (id, message) = id_pipe2.recv_message().await.unwrap();
            assert_eq!(id, 123);
            assert_eq!(*message, "id test");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_id_message_pipe_async_operations() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let id_pipe1: IdMessagePipe<String, i32> = IdMessagePipe::new(pipe1);
        let id_pipe2: IdMessagePipe<i32, String> = IdMessagePipe::new(pipe2);

        let send_task = crate::operations::spawn_task(async move {
            id_pipe1
                .send_message(456, Box::new("async id test".to_string()))
                .await
                .unwrap();
            let (response_id, response) = id_pipe1.recv_message().await.unwrap();
            assert_eq!(response_id, 789);
            assert_eq!(*response, 999);
        });

        let recv_task = crate::operations::spawn_task(async move {
            let (id, message) = id_pipe2.recv_message().await.unwrap();
            assert_eq!(id, 456);
            assert_eq!(*message, "async id test");

            id_pipe2.send_message(789, Box::new(999)).await.unwrap();
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_id_message_pipe_sync_operations() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let id_pipe1: IdMessagePipe<String, i32> = IdMessagePipe::new(pipe1);
        let id_pipe2: IdMessagePipe<i32, String> = IdMessagePipe::new(pipe2);

        let sync_task = std::thread::spawn(move || {
            // Test sync send
            id_pipe1
                .send_message_sync(321, Box::new("sync id test".to_string()))
                .unwrap();

            // Test sync receive
            let (response_id, response) = id_pipe1.recv_message_sync().unwrap();
            assert_eq!(response_id, 654);
            assert_eq!(*response, 888);
        });

        let async_task = crate::operations::spawn_task(async move {
            // Receive sync message
            let (id, message) = id_pipe2.recv_message().await.unwrap();
            assert_eq!(id, 321);
            assert_eq!(*message, "sync id test");

            // Send response
            id_pipe2.send_message(654, Box::new(888)).await.unwrap();
        });

        async_task.await.unwrap();
        sync_task.join().unwrap();
    }

    #[crate::test]
    async fn test_id_message_pipe_multiple_instances() {
        let (pipe1, pipe2) = crate::pipe::bipipe();
        let (pipe3, pipe4) = crate::pipe::bipipe();

        let id_pipe1: IdMessagePipe<String, i32> = IdMessagePipe::new(pipe1);
        let id_pipe2: IdMessagePipe<i32, String> = IdMessagePipe::new(pipe2);
        let id_pipe3: IdMessagePipe<String, i32> = IdMessagePipe::new(pipe3);
        let id_pipe4: IdMessagePipe<i32, String> = IdMessagePipe::new(pipe4);

        let send_task1 = crate::operations::spawn_task(async move {
            id_pipe1
                .send_message(111, Box::new("test 1".to_string()))
                .await
                .unwrap();
        });

        let send_task2 = crate::operations::spawn_task(async move {
            id_pipe3
                .send_message(222, Box::new("test 2".to_string()))
                .await
                .unwrap();
        });

        let recv_task1 = crate::operations::spawn_task(async move {
            let (id, message) = id_pipe2.recv_message().await.unwrap();
            assert_eq!(id, 111);
            assert_eq!(*message, "test 1");
        });

        let recv_task2 = crate::operations::spawn_task(async move {
            let (id, message) = id_pipe4.recv_message().await.unwrap();
            assert_eq!(id, 222);
            assert_eq!(*message, "test 2");
        });

        send_task1.await.unwrap();
        send_task2.await.unwrap();
        recv_task1.await.unwrap();
        recv_task2.await.unwrap();
    }

    #[crate::test]
    async fn test_recv_message_future_is_terminated() {
        let (tx, rx): (MessagePipeSender<String>, MessagePipeReceiver<String>) =
            make_message_pipe_oneway().await.unwrap();

        let future = rx.recv_message().fuse();

        // Check that future is not terminated initially
        assert!(!future.is_terminated());

        // Send a message
        tx.send_message(Box::new("terminate test".to_string()))
            .await
            .unwrap();

        // Complete the future
        let message = future.await.unwrap();
        assert_eq!(*message, "terminate test");
    }

    #[crate::test]
    async fn test_message_pipe_clone() {
        let (pipe1, pipe2): (MessagePipe<String, i32>, MessagePipe<i32, String>) =
            make_message_pipe();
        let pipe1_clone = pipe1.clone();

        let send_task = crate::operations::spawn_task(async move {
            pipe1_clone
                .send_message(Box::new("clone test".to_string()))
                .await
                .unwrap();
        });

        let recv_task = crate::operations::spawn_task(async move {
            let message = pipe2.recv_message().await.unwrap();
            assert_eq!(*message, "clone test");
        });

        send_task.await.unwrap();
        recv_task.await.unwrap();
    }

    #[crate::test]
    async fn test_message_pipe_error_handling() {
        let (pipe1, _pipe2): (MessagePipe<String, i32>, MessagePipe<i32, String>) =
            make_message_pipe();

        // Drop pipe2 to close the connection
        drop(_pipe2);

        // Sending should fail
        let result = pipe1
            .send_message(Box::new("should fail".to_string()))
            .await;
        assert!(result.is_err());

        // Receiving should fail
        let result = pipe1.recv_message().await;
        assert!(result.is_err());
    }

    #[crate::test]
    async fn test_id_message_pipe_error_handling() {
        let (pipe1, _pipe2) = crate::pipe::bipipe();
        let id_pipe: IdMessagePipe<String, i32> = IdMessagePipe::new(pipe1);

        // Drop pipe2 to close the connection
        drop(_pipe2);

        // Sending should fail
        let result = id_pipe
            .send_message(123, Box::new("should fail".to_string()))
            .await;
        assert!(result.is_err());

        // Receiving should fail
        let result = id_pipe.recv_message().await;
        assert!(result.is_err());
    }
}
