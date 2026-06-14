// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Experimental runtime-neutral stackful capability traits.
//!
//! This module is an alpha compatibility surface used to prove that
//! `kimojio-stack` and companion runtimes can share stackful wait, scoped spawn,
//! and minimal I/O capability contracts. It is not a stable downstream API yet;
//! release notes must call out any breaking changes while the surface is being
//! shaped.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use crate::{Errno, ExternalWaitRegistration, ExternalWaiter, Runtime, RuntimeContext};

/// Explicit runtime capability queried from a runtime/context handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCapability {
    /// The context can park the current stackful coroutine on an external waiter.
    StackfulWait,
    /// The context exposes explicit ring handles for I/O operations.
    ExplicitRingIo,
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
    /// Performs the common sleep/timeout operation.
    fn sleep_for(&self, duration: Duration) -> Result<(), RuntimeIoError>;

    /// Requires explicit ring-handle I/O support.
    fn require_explicit_ring_io(&self) -> Result<(), UnsupportedCapability> {
        self.require_capability(RuntimeCapability::ExplicitRingIo)
    }
}

impl RuntimeCapability {
    fn name(self) -> &'static str {
        match self {
            Self::StackfulWait => "stackful-wait",
            Self::ExplicitRingIo => "explicit-ring-io",
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
        matches!(capability, RuntimeCapability::StackfulWait)
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
    fn sleep_for(&self, duration: Duration) -> Result<(), RuntimeIoError> {
        self.sleep(duration).map_err(RuntimeIoError::from)
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
