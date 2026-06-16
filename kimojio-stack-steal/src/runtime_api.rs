// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::os::fd::OwnedFd;
use std::time::Duration;

use kimojio_stack::{
    IoReadBuffer, IoRuntime, IoWriteBuffer, RuntimeIoError, RuntimeIoErrorKind, RuntimeReadResult,
    RuntimeSocket, RuntimeWriteResult, SocketIoRuntime, StackRuntime, StackRuntimeContext,
};
use rustix::net::Shutdown;

use crate::{
    RingError, RingFd, RingMode, RingReadResult, RingTimeout, RingWriteResult, Runtime,
    RuntimeContext,
};

impl StackRuntime for Runtime {
    type Context<'cx> = RuntimeContext<'cx>;

    fn block_on<F, T>(&mut self, main: F) -> T
    where
        F: for<'cx> FnOnce(&Self::Context<'cx>) -> T,
    {
        Runtime::block_on(self, main)
    }
}

impl StackRuntimeContext for RuntimeContext<'_> {
    type ChildContext<'cx> = RuntimeContext<'cx>;

    fn yield_now(&self) {
        RuntimeContext::yield_now(self);
    }

    fn spawn_scoped<F, T>(&self, f: F) -> T
    where
        F: for<'cx> FnOnce(&Self::ChildContext<'cx>) -> T,
    {
        self.scope(|scope| {
            let handle = scope.spawn_local(move |cx| f(cx));
            handle.join(self)
        })
    }

    fn spawn_stealable_scoped<F, T>(&self, f: F) -> T
    where
        F: for<'cx> FnOnce(&Self::ChildContext<'cx>) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.scope(|scope| {
            let handle = scope.spawn_stealable(move |cx| f(cx));
            handle.join(self)
        })
    }
}

impl IoRuntime for RuntimeContext<'_> {
    type Sleep = RingTimeout;

    fn sleep_async(&self, duration: Duration) -> Result<Self::Sleep, RuntimeIoError> {
        let mode = if self.current_worker().is_some() {
            RingMode::WorkerLocal
        } else {
            RingMode::Shared
        };
        self.create_ring(mode)
            .and_then(|ring| ring.timeout(self, duration))
            .map_err(runtime_io_error)
    }

    fn sleep_for(&self, duration: Duration) -> Result<(), RuntimeIoError> {
        let mode = if self.current_worker().is_some() {
            RingMode::WorkerLocal
        } else {
            RingMode::Shared
        };
        self.create_ring(mode)
            .map_err(runtime_io_error)?
            .sleep(self, duration)
            .map_err(runtime_io_error)
    }
}

impl RuntimeSocket for RingFd {}

impl<B> RuntimeReadResult<B> for RingReadResult<B>
where
    B: IoReadBuffer + Send + 'static,
{
    type Output = kimojio_stack::ReadOutput<B>;

    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
        RingReadResult::try_get(self).map(|result| result.map_err(runtime_io_error))
    }

    fn cancel(&mut self) -> Result<(), RuntimeIoError> {
        RingReadResult::cancel(self);
        Ok(())
    }
}

impl<B> RuntimeWriteResult<B> for RingWriteResult<B>
where
    B: IoWriteBuffer + Send + 'static,
{
    type Output = kimojio_stack::WriteOutput<B>;

    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
        RingWriteResult::try_get(self).map(|result| result.map_err(runtime_io_error))
    }

    fn cancel(&mut self) -> Result<(), RuntimeIoError> {
        RingWriteResult::cancel(self);
        Ok(())
    }
}

impl SocketIoRuntime for RuntimeContext<'_> {
    type Socket = RingFd;
    type ReadResult<B>
        = RingReadResult<B>
    where
        B: IoReadBuffer + Send + 'static;
    type WriteResult<B>
        = RingWriteResult<B>
    where
        B: IoWriteBuffer + Send + 'static;

    fn socket_from_owned_fd(&self, fd: OwnedFd) -> Result<Self::Socket, RuntimeIoError> {
        Ok(RingFd::from_owned(fd))
    }

    fn read(&self, fd: &Self::Socket, buf: &mut [u8]) -> Result<usize, RuntimeIoError> {
        self.socket_ring()
            .and_then(|ring| ring.read(self, fd, buf))
            .map_err(runtime_io_error)
    }

    fn write(&self, fd: &Self::Socket, buf: &[u8]) -> Result<usize, RuntimeIoError> {
        self.socket_ring()
            .and_then(|ring| ring.write(self, fd, buf))
            .map_err(runtime_io_error)
    }

    fn read_async<B>(
        &self,
        fd: &Self::Socket,
        buffer: B,
    ) -> Result<Self::ReadResult<B>, RuntimeIoError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        self.socket_ring()
            .and_then(|ring| ring.read_async(self, fd, buffer))
            .map_err(runtime_io_error)
    }

    fn write_async<B>(
        &self,
        fd: &Self::Socket,
        buffer: B,
    ) -> Result<Self::WriteResult<B>, RuntimeIoError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        self.socket_ring()
            .and_then(|ring| ring.write_async(self, fd, buffer))
            .map_err(runtime_io_error)
    }

    fn cancel_read<B>(&self, read: &mut Self::ReadResult<B>) -> Result<(), RuntimeIoError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        read.request_cancel(self).map_err(runtime_io_error)
    }

    fn cancel_write<B>(&self, write: &mut Self::WriteResult<B>) -> Result<(), RuntimeIoError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        write.request_cancel(self).map_err(runtime_io_error)
    }

    fn shutdown(&self, fd: &Self::Socket, how: Shutdown) -> Result<(), RuntimeIoError> {
        self.socket_ring()
            .and_then(|ring| ring.shutdown(self, fd, how))
            .map_err(runtime_io_error)
    }

    fn close(&self, fd: Self::Socket) -> Result<(), RuntimeIoError> {
        self.socket_ring()
            .and_then(|ring| ring.close(self, fd))
            .map_err(runtime_io_error)
    }
}

impl RuntimeContext<'_> {
    fn socket_ring(&self) -> Result<crate::Ring, RingError> {
        if let Some(ring) = self.socket_ring.borrow().as_ref() {
            return Ok(ring.clone());
        }

        let mode = if self.current_worker().is_some() {
            RingMode::WorkerLocal
        } else {
            RingMode::Shared
        };
        let ring = self.create_ring(mode)?;
        *self.socket_ring.borrow_mut() = Some(ring.clone());
        Ok(ring)
    }

    #[cfg(test)]
    pub(crate) fn socket_ring_for_test(&self) -> Result<crate::Ring, RingError> {
        self.socket_ring()
    }
}

impl From<RingError> for RuntimeIoError {
    fn from(error: RingError) -> Self {
        runtime_io_error(error)
    }
}

fn runtime_io_error(error: RingError) -> RuntimeIoError {
    match error {
        RingError::Io(error) => RuntimeIoError::Io(error),
        RingError::NoCurrentWorker => RuntimeIoError::runtime(RuntimeIoErrorKind::NoCurrentWorker),
        RingError::WrongWorker { .. } => RuntimeIoError::runtime(RuntimeIoErrorKind::WrongWorker),
        RingError::WrongRuntime => RuntimeIoError::runtime(RuntimeIoErrorKind::WrongRuntime),
        RingError::NoStackfulContext => {
            RuntimeIoError::runtime(RuntimeIoErrorKind::NoStackfulContext)
        }
        RingError::QueueFull => RuntimeIoError::runtime(RuntimeIoErrorKind::QueueFull),
        RingError::ResourceLimit => RuntimeIoError::runtime(RuntimeIoErrorKind::ResourceLimit),
        RingError::DurationOutOfRange => {
            RuntimeIoError::runtime(RuntimeIoErrorKind::DurationOutOfRange)
        }
        RingError::FdInUse => RuntimeIoError::runtime(RuntimeIoErrorKind::FdInUse),
        RingError::Closed => RuntimeIoError::runtime(RuntimeIoErrorKind::Closed),
        RingError::Canceled => RuntimeIoError::runtime(RuntimeIoErrorKind::Canceled),
    }
}
