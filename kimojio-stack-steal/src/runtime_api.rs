// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;

use kimojio_stack::{IoRuntime, RuntimeIoError, StackRuntime, StackRuntimeContext};

use crate::{RingError, Runtime, RuntimeContext};

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
    fn sleep_for(&self, duration: Duration) -> Result<(), RuntimeIoError> {
        self.create_worker_ring()
            .sleep(self, duration)
            .map_err(runtime_io_error)
    }
}

fn runtime_io_error(error: RingError) -> RuntimeIoError {
    match error {
        RingError::Io(error) => RuntimeIoError::Io(error),
        RingError::WrongWorker { .. } => {
            RuntimeIoError::Other("worker-local ring used from a different worker")
        }
        RingError::WrongRuntime => {
            RuntimeIoError::Other("worker-local ring used from a different runtime")
        }
        RingError::QueueFull => RuntimeIoError::Other("ring queue is full"),
        RingError::ResourceLimit => RuntimeIoError::Other("shared ring helper limit reached"),
        RingError::DurationOutOfRange => RuntimeIoError::Other("duration is out of range"),
        RingError::Closed => RuntimeIoError::Other("ring is closed"),
        RingError::Canceled => RuntimeIoError::Other("ring operation was canceled"),
    }
}
