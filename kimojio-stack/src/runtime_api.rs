// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Experimental runtime-neutral stackful capability traits.
//!
//! This module is an alpha compatibility surface used to prove that
//! `kimojio-stack` and companion runtimes can share stackful wait, scoped spawn,
//! and minimal I/O capability contracts. The current socket contract is scoped to
//! the operations needed by downstream stack TLS and HTTP transports: capability
//! checks, waitable sleep/timer handles, socket conversion, read/write,
//! runtime-neutral async waitable read/write, cancellation, shutdown, and close.
//! It is not a stable downstream API yet; release notes must call out any
//! breaking changes while the surface is being shaped.

use std::error::Error;
use std::fmt;
use std::os::fd::{AsFd, OwnedFd};
use std::time::Duration;

use rustix::net::Shutdown;

use crate::{
    Errno, ExternalWaitRegistration, ExternalWaiter, IoFd, IoReadBuffer, IoResult, IoWriteBuffer,
    ReadOutput, Runtime, RuntimeContext, Timeout, WaitRegistration, Waitable, WriteOutput,
};

/// Explicit runtime capability queried from a runtime/context handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCapability {
    /// The context can park the current stackful coroutine on an external waiter.
    StackfulWait,
    /// The context exposes explicit ring handles for I/O operations.
    ExplicitRingIo,
    /// The context exposes socket read/write lifecycle operations.
    SocketIo,
}

/// Runtime family for explicit capability negotiation and diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeFamily {
    /// The single-threaded `kimojio-stack` runtime.
    Stack,
    /// Another runtime family identified by a stable diagnostic name.
    Other(&'static str),
}

/// Error returned when a runtime capability is not supported by a runtime adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsupportedCapability {
    capability: &'static str,
    family: RuntimeFamily,
}

impl UnsupportedCapability {
    /// Creates an unsupported-capability error for `capability` on `family`.
    pub fn new(capability: &'static str, family: RuntimeFamily) -> Self {
        Self { capability, family }
    }

    /// Returns the unsupported capability name.
    pub fn capability(&self) -> &'static str {
        self.capability
    }

    /// Returns the runtime family that reported the unsupported capability.
    pub fn family(&self) -> RuntimeFamily {
        self.family
    }
}

impl fmt::Display for UnsupportedCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "runtime family {:?} does not support capability {}",
            self.family, self.capability
        )
    }
}

impl Error for UnsupportedCapability {}

/// Error returned by runtime-neutral I/O proof operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeIoError {
    /// The underlying io_uring operation failed.
    Io(Errno),
    /// The requested runtime capability is not supported by this adapter.
    Unsupported(UnsupportedCapability),
    /// The adapter failed for a runtime-specific reason.
    Other(&'static str),
}

impl fmt::Display for RuntimeIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "runtime I/O failed: {error}"),
            Self::Unsupported(error) => error.fmt(f),
            Self::Other(message) => f.write_str(message),
        }
    }
}

impl Error for RuntimeIoError {}

impl From<Errno> for RuntimeIoError {
    fn from(error: Errno) -> Self {
        Self::Io(error)
    }
}

/// Error returned by runtime-neutral stackful wait helpers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeWaitError {
    /// The runtime adapter does not support stackful waiting.
    Unsupported(UnsupportedCapability),
    /// No waitable was supplied to a wait-any operation.
    Empty,
    /// The current context cannot register and park a stackful coroutine.
    NoStackfulContext {
        /// Runtime family that reported the wrong/no-context condition.
        family: RuntimeFamily,
    },
}

impl fmt::Display for RuntimeWaitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(error) => error.fmt(f),
            Self::Empty => f.write_str("no runtime-neutral waitables supplied"),
            Self::NoStackfulContext { family } => write!(
                f,
                "runtime family {family:?} cannot register the current stackful wait context"
            ),
        }
    }
}

impl Error for RuntimeWaitError {}

/// Explicit capability-query surface for runtime/context adapters.
pub trait RuntimeCapabilities {
    /// Returns the runtime family represented by this context.
    fn runtime_family(&self) -> RuntimeFamily;

    /// Returns whether this context supports `capability`.
    fn supports(&self, capability: RuntimeCapability) -> bool;

    /// Returns `Ok(())` if `capability` is supported or an explicit unsupported error.
    fn require_capability(
        &self,
        capability: RuntimeCapability,
    ) -> Result<(), UnsupportedCapability> {
        if self.supports(capability) {
            Ok(())
        } else {
            Err(UnsupportedCapability::new(
                capability.name(),
                self.runtime_family(),
            ))
        }
    }
}

/// Minimal runtime-neutral stackful execution surface.
pub trait StackRuntime {
    /// Context type passed to [`StackRuntime::block_on`].
    type Context<'cx>: StackRuntimeContext + IoRuntime + 'cx
    where
        Self: 'cx;

    /// Runs `main` to completion inside this runtime.
    fn block_on<F, T>(&mut self, main: F) -> T
    where
        F: for<'cx> FnOnce(&Self::Context<'cx>) -> T;
}

/// Minimal runtime-neutral stackful context surface used by proof components.
pub trait StackRuntimeContext: StackfulWaitContext {
    /// Context type passed to spawned child stackful tasks.
    type ChildContext<'cx>: StackRuntimeContext + IoRuntime + 'cx;

    /// Cooperatively yields the current stackful task.
    fn yield_now(&self);

    /// Spawns one local scoped stackful task with its child context, waits for it, and returns its
    /// result.
    ///
    /// This preserves the local scoped semantics of `kimojio-stack`: implementations may keep the
    /// child on the current worker/thread and may support borrowed, non-`Send` captures.
    fn spawn_scoped<F, T>(&self, f: F) -> T
    where
        F: for<'cx> FnOnce(&Self::ChildContext<'cx>) -> T;

    /// Spawns one stealable/offload-eligible stackful task, waits for it, and returns its result.
    ///
    /// Runtime adapters that can execute work on another worker should map this to their explicit
    /// cross-worker spawn path. Single-threaded adapters may execute it as local scoped work.
    fn spawn_stealable_scoped<F, T>(&self, f: F) -> T
    where
        F: for<'cx> FnOnce(&Self::ChildContext<'cx>) -> T + Send + 'static,
        T: Send + 'static,
    {
        self.spawn_scoped(f)
    }
}

/// Minimal runtime-neutral I/O surface shared by stack runtime adapters.
pub trait IoRuntime: RuntimeCapabilities {
    /// Waitable sleep/timeout handle returned by [`IoRuntime::sleep_async`].
    type Sleep: RuntimeWaitable;

    /// Starts the common sleep/timeout operation and returns a runtime-neutral
    /// waitable handle.
    fn sleep_async(&self, duration: Duration) -> Result<Self::Sleep, RuntimeIoError>;

    /// Performs the common sleep/timeout operation.
    fn sleep_for(&self, duration: Duration) -> Result<(), RuntimeIoError>;

    /// Requires explicit ring-handle I/O support.
    fn require_explicit_ring_io(&self) -> Result<(), UnsupportedCapability> {
        self.require_capability(RuntimeCapability::ExplicitRingIo)
    }

    /// Requires socket read/write I/O support.
    fn require_socket_io(&self) -> Result<(), UnsupportedCapability> {
        self.require_capability(RuntimeCapability::SocketIo)
    }
}

/// Runtime-neutral socket handle abstraction used by downstream stack transports.
pub trait RuntimeSocket: AsFd {}

impl RuntimeSocket for IoFd {}

/// A runtime-neutral condition that can park stackful coroutines until ready.
///
/// This is the waitable counterpart to [`StackfulWaitContext`]. Implementations
/// store waiters produced by a runtime adapter without receiving a concrete
/// stack-core [`RuntimeContext`].
pub trait RuntimeWaitable {
    /// Returns whether this condition can be consumed without parking.
    fn is_ready(&self) -> bool;

    /// Registers `waiter` for a future wake.
    ///
    /// Returns `true` when the waiter was stored. Returning `false` means the
    /// condition became ready before registration completed and the caller should
    /// re-check readiness instead of parking.
    fn add_stackful_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool;

    /// Wraps this runtime-neutral condition as a stack-core [`Waitable`].
    fn as_stack_waitable(&self) -> RuntimeWaitableAdapter<'_>
    where
        Self: Sized,
    {
        RuntimeWaitableAdapter::new(self)
    }
}

/// Zero-allocation adapter from [`RuntimeWaitable`] to stack-core [`Waitable`].
pub struct RuntimeWaitableAdapter<'waitable> {
    waitable: &'waitable dyn RuntimeWaitable,
}

impl<'waitable> RuntimeWaitableAdapter<'waitable> {
    /// Creates an adapter for `waitable`.
    pub fn new(waitable: &'waitable dyn RuntimeWaitable) -> Self {
        Self { waitable }
    }
}

impl Waitable for RuntimeWaitableAdapter<'_> {
    fn is_ready(&self) -> bool {
        RuntimeWaitable::is_ready(self.waitable)
    }

    fn add_waiter(&self, cx: &RuntimeContext<'_>, registration: &WaitRegistration) {
        if RuntimeWaitable::is_ready(self.waitable) {
            return;
        }

        if let Some(waiter) = registration.external_waiter(cx)
            && !self.waitable.add_stackful_waiter(waiter)
            && let Some(waiter) = registration.external_waiter(cx)
        {
            wake_stackful_waiter(waiter);
        }
    }
}

/// Runtime-neutral waitable result for a pending socket read.
///
/// Result handles are waited through [`RuntimeWaitable`]. Stack-core runtimes may
/// also implement [`Waitable`] on concrete result types, but non-stack adapters do
/// not need to expose stack-core waiter compatibility just to satisfy this
/// runtime-neutral trait.
pub trait RuntimeReadResult<B>: RuntimeWaitable {
    /// Output produced when the read operation completes.
    type Output;

    /// Attempts to consume the read result without parking.
    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>>;

    /// Requests cancellation through this result handle.
    ///
    /// This direct result-level cancellation is allowed to detach the pending
    /// operation from the handle; callers should not expect a later `try_get` to
    /// return a cancellation result. Code that must keep the result handle
    /// drainable after a timeout should call [`SocketIoRuntime::cancel_read`]
    /// instead; a runtime adapter may override that method with a non-consuming
    /// cancellation path.
    fn cancel(&mut self) -> Result<(), RuntimeIoError>;
}

impl<B> RuntimeReadResult<B> for IoResult<ReadOutput<B>, B>
where
    B: IoReadBuffer + Send + 'static,
{
    type Output = ReadOutput<B>;

    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
        IoResult::try_get(self).map(|result| result.map_err(RuntimeIoError::from))
    }

    fn cancel(&mut self) -> Result<(), RuntimeIoError> {
        IoResult::cancel(self);
        Ok(())
    }
}

/// Runtime-neutral waitable result for a pending socket write.
///
/// Result handles are waited through [`RuntimeWaitable`]. Stack-core runtimes may
/// also implement [`Waitable`] on concrete result types, but non-stack adapters do
/// not need to expose stack-core waiter compatibility just to satisfy this
/// runtime-neutral trait.
pub trait RuntimeWriteResult<B>: RuntimeWaitable {
    /// Output produced when the write operation completes.
    type Output;

    /// Attempts to consume the write result without parking.
    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>>;

    /// Requests cancellation through this result handle.
    ///
    /// This direct result-level cancellation is allowed to detach the pending
    /// operation from the handle; callers should not expect a later `try_get` to
    /// return a cancellation result. Code that must keep the result handle
    /// drainable after a timeout should call [`SocketIoRuntime::cancel_write`]
    /// instead; a runtime adapter may override that method with a non-consuming
    /// cancellation path.
    fn cancel(&mut self) -> Result<(), RuntimeIoError>;
}

impl<B> RuntimeWriteResult<B> for IoResult<WriteOutput<B>, B>
where
    B: IoWriteBuffer + Send + 'static,
{
    type Output = WriteOutput<B>;

    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
        IoResult::try_get(self).map(|result| result.map_err(RuntimeIoError::from))
    }

    fn cancel(&mut self) -> Result<(), RuntimeIoError> {
        IoResult::cancel(self);
        Ok(())
    }
}

/// Runtime-neutral socket I/O operations required by downstream stack transports.
pub trait SocketIoRuntime: IoRuntime {
    /// Socket handle used by this runtime adapter.
    type Socket: RuntimeSocket;

    /// Pending read operation handle returned by [`SocketIoRuntime::read_async`].
    type ReadResult<B>: RuntimeReadResult<B, Output = ReadOutput<B>>
    where
        B: IoReadBuffer + Send + 'static;

    /// Pending write operation handle returned by [`SocketIoRuntime::write_async`].
    type WriteResult<B>: RuntimeWriteResult<B, Output = WriteOutput<B>>
    where
        B: IoWriteBuffer + Send + 'static;

    /// Converts an owned fd into this runtime's socket handle.
    fn socket_from_owned_fd(&self, fd: OwnedFd) -> Result<Self::Socket, RuntimeIoError>;

    /// Reads from `fd` into `buf`.
    fn read(&self, fd: &Self::Socket, buf: &mut [u8]) -> Result<usize, RuntimeIoError>;

    /// Writes `buf` to `fd`.
    fn write(&self, fd: &Self::Socket, buf: &[u8]) -> Result<usize, RuntimeIoError>;

    /// Starts a read into an owned buffer.
    fn read_async<B>(
        &self,
        fd: &Self::Socket,
        buffer: B,
    ) -> Result<Self::ReadResult<B>, RuntimeIoError>
    where
        B: IoReadBuffer + Send + 'static;

    /// Starts a write from an owned buffer.
    fn write_async<B>(
        &self,
        fd: &Self::Socket,
        buffer: B,
    ) -> Result<Self::WriteResult<B>, RuntimeIoError>
    where
        B: IoWriteBuffer + Send + 'static;

    /// Requests cancellation of a pending read.
    ///
    /// The default delegates to [`RuntimeReadResult::cancel`], which may detach
    /// the result handle. Runtime adapters that can keep canceled results
    /// drainable should override this method.
    fn cancel_read<B>(&self, read: &mut Self::ReadResult<B>) -> Result<(), RuntimeIoError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        read.cancel()
    }

    /// Requests cancellation of a pending write.
    ///
    /// The default delegates to [`RuntimeWriteResult::cancel`], which may detach
    /// the result handle. Runtime adapters that can keep canceled results
    /// drainable should override this method.
    fn cancel_write<B>(&self, write: &mut Self::WriteResult<B>) -> Result<(), RuntimeIoError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        write.cancel()
    }

    /// Shuts down part or all of a connected socket.
    fn shutdown(&self, fd: &Self::Socket, how: Shutdown) -> Result<(), RuntimeIoError>;
    /// Closes `fd` through the runtime.
    fn close(&self, fd: Self::Socket) -> Result<(), RuntimeIoError>;
}

impl RuntimeCapability {
    fn name(self) -> &'static str {
        match self {
            Self::StackfulWait => "stackful-wait",
            Self::ExplicitRingIo => "explicit-ring-io",
            Self::SocketIo => "socket-io",
        }
    }
}

/// A waiter that can be stored in cross-thread channel wait queues.
pub trait StackfulWaiter: Send + 'static {
    /// Returns whether this waiter is still active.
    fn is_active(&self) -> bool;

    /// Marks this waiter ready if it is still active.
    fn mark_ready(&self) -> bool;

    /// Wakes a waiter that was previously marked ready.
    fn wake_ready(self: Box<Self>) -> bool;
}

/// A cancellation guard for a registered stackful wait.
pub trait StackfulWaitRegistration {
    /// Creates a waiter associated with this registration.
    fn waiter(&self) -> Box<dyn StackfulWaiter>;
}

/// Runtime context operations needed to park stackful coroutines on external waits.
pub trait StackfulWaitContext: RuntimeCapabilities {
    /// Registers the current stackful coroutine for an external wait.
    ///
    /// Returns `None` when the context cannot park the current execution unit as a
    /// stackful coroutine, such as root runtime code that is not inside a spawned
    /// coroutine.
    fn stackful_wait_registration(&self) -> Option<Box<dyn StackfulWaitRegistration + '_>>;

    /// Parks the current stackful coroutine or advances the runtime according to
    /// the context's root/coroutine semantics.
    fn park_stackful(&self);

    /// Waits until every runtime-neutral waitable is ready.
    fn wait_all_stackful(
        &self,
        waitables: &[&dyn RuntimeWaitable],
    ) -> Result<(), RuntimeWaitError> {
        if waitables.iter().all(|waitable| waitable.is_ready()) {
            return Ok(());
        }
        self.require_stackful_wait()?;

        loop {
            if waitables.iter().all(|waitable| waitable.is_ready()) {
                return Ok(());
            }

            let Some(registration) = self.stackful_wait_registration() else {
                return Err(RuntimeWaitError::NoStackfulContext {
                    family: self.runtime_family(),
                });
            };

            let mut should_park = false;
            let mut should_recheck = false;
            for waitable in waitables {
                if !waitable.is_ready() {
                    if waitable.add_stackful_waiter(registration.waiter()) {
                        should_park = true;
                    } else {
                        should_recheck = true;
                    }
                }
            }

            if should_park && !should_recheck {
                self.park_stackful();
            }
        }
    }

    /// Waits until any runtime-neutral waitable is ready and returns its index.
    fn wait_any_stackful(
        &self,
        waitables: &[&dyn RuntimeWaitable],
    ) -> Result<usize, RuntimeWaitError> {
        if waitables.is_empty() {
            return Err(RuntimeWaitError::Empty);
        }
        if let Some(index) = first_ready_stackful(waitables) {
            return Ok(index);
        }
        self.require_stackful_wait()?;

        loop {
            if let Some(index) = first_ready_stackful(waitables) {
                return Ok(index);
            }

            let Some(registration) = self.stackful_wait_registration() else {
                return Err(RuntimeWaitError::NoStackfulContext {
                    family: self.runtime_family(),
                });
            };

            let mut should_park = false;
            let mut should_recheck = false;
            for waitable in waitables {
                if !waitable.is_ready() {
                    if waitable.add_stackful_waiter(registration.waiter()) {
                        should_park = true;
                    } else {
                        should_recheck = true;
                    }
                }
            }

            if should_park && !should_recheck {
                self.park_stackful();
            }
        }
    }

    /// Waits until one runtime-neutral waitable is ready.
    fn wait_stackful(&self, waitable: &dyn RuntimeWaitable) -> Result<(), RuntimeWaitError> {
        self.wait_all_stackful(&[waitable])
    }

    fn require_stackful_wait(&self) -> Result<(), RuntimeWaitError> {
        self.require_capability(RuntimeCapability::StackfulWait)
            .map_err(RuntimeWaitError::Unsupported)
    }
}

fn first_ready_stackful(waitables: &[&dyn RuntimeWaitable]) -> Option<usize> {
    waitables.iter().position(|waitable| waitable.is_ready())
}

fn wake_stackful_waiter(waiter: Box<dyn StackfulWaiter>) {
    if waiter.mark_ready() {
        waiter.wake_ready();
    }
}

impl<T, B> RuntimeWaitable for IoResult<T, B> {
    fn is_ready(&self) -> bool {
        Waitable::is_ready(self)
    }

    fn add_stackful_waiter(&self, waiter: Box<dyn StackfulWaiter>) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        state.add_stackful_waiter(waiter)
    }
}

impl StackfulWaiter for ExternalWaiter {
    fn is_active(&self) -> bool {
        ExternalWaiter::is_active(self)
    }

    fn mark_ready(&self) -> bool {
        ExternalWaiter::mark_ready(self)
    }

    fn wake_ready(self: Box<Self>) -> bool {
        (*self).wake_ready()
    }
}

impl StackfulWaitRegistration for ExternalWaitRegistration {
    fn waiter(&self) -> Box<dyn StackfulWaiter> {
        Box::new(ExternalWaitRegistration::waiter(self))
    }
}

impl RuntimeCapabilities for RuntimeContext<'_> {
    fn runtime_family(&self) -> RuntimeFamily {
        RuntimeFamily::Stack
    }

    fn supports(&self, capability: RuntimeCapability) -> bool {
        matches!(
            capability,
            RuntimeCapability::StackfulWait | RuntimeCapability::SocketIo
        )
    }
}

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
            let handle = scope.spawn(move |cx| f(cx));
            handle.join(self)
        })
    }
}

impl IoRuntime for RuntimeContext<'_> {
    type Sleep = Timeout;

    fn sleep_async(&self, duration: Duration) -> Result<Self::Sleep, RuntimeIoError> {
        Ok(self.timeout(duration))
    }

    fn sleep_for(&self, duration: Duration) -> Result<(), RuntimeIoError> {
        self.sleep(duration).map_err(RuntimeIoError::from)
    }
}

impl SocketIoRuntime for RuntimeContext<'_> {
    type Socket = IoFd;
    type ReadResult<B>
        = IoResult<ReadOutput<B>, B>
    where
        B: IoReadBuffer + Send + 'static;
    type WriteResult<B>
        = IoResult<WriteOutput<B>, B>
    where
        B: IoWriteBuffer + Send + 'static;

    fn socket_from_owned_fd(&self, fd: OwnedFd) -> Result<Self::Socket, RuntimeIoError> {
        Ok(IoFd::from_owned(fd))
    }

    fn read(&self, fd: &Self::Socket, buf: &mut [u8]) -> Result<usize, RuntimeIoError> {
        RuntimeContext::read(self, fd, buf).map_err(RuntimeIoError::from)
    }

    fn write(&self, fd: &Self::Socket, buf: &[u8]) -> Result<usize, RuntimeIoError> {
        RuntimeContext::write(self, fd, buf).map_err(RuntimeIoError::from)
    }

    fn read_async<B>(
        &self,
        fd: &Self::Socket,
        buffer: B,
    ) -> Result<Self::ReadResult<B>, RuntimeIoError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        Ok(RuntimeContext::read_async(self, fd, buffer))
    }

    fn write_async<B>(
        &self,
        fd: &Self::Socket,
        buffer: B,
    ) -> Result<Self::WriteResult<B>, RuntimeIoError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        Ok(RuntimeContext::write_async(self, fd, buffer))
    }

    fn cancel_read<B>(&self, read: &mut Self::ReadResult<B>) -> Result<(), RuntimeIoError>
    where
        B: IoReadBuffer + Send + 'static,
    {
        RuntimeContext::cancel_io(self, read).map_err(RuntimeIoError::from)
    }

    fn cancel_write<B>(&self, write: &mut Self::WriteResult<B>) -> Result<(), RuntimeIoError>
    where
        B: IoWriteBuffer + Send + 'static,
    {
        RuntimeContext::cancel_io(self, write).map_err(RuntimeIoError::from)
    }

    fn shutdown(&self, fd: &Self::Socket, how: Shutdown) -> Result<(), RuntimeIoError> {
        RuntimeContext::shutdown(self, fd, how).map_err(RuntimeIoError::from)
    }

    fn close(&self, fd: Self::Socket) -> Result<(), RuntimeIoError> {
        let fd = fd.into_owned().map_err(|_| {
            RuntimeIoError::Other("socket handle cannot be closed while cloned or pending")
        })?;
        RuntimeContext::close(self, fd).map_err(RuntimeIoError::from)
    }
}

impl StackfulWaitContext for RuntimeContext<'_> {
    fn stackful_wait_registration(&self) -> Option<Box<dyn StackfulWaitRegistration + '_>> {
        self.external_wait_registration()
            .map(|registration| Box::new(registration) as Box<dyn StackfulWaitRegistration>)
    }

    fn park_stackful(&self) {
        self.park();
    }
}
