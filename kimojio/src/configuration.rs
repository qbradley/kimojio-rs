// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
//! Configuration is used to configure the per thread behavior
//! of the uring runtime.
//!

use std::time::Duration;

/// Configuration options for the io_uring runtime.
///
/// Use the builder pattern to customize runtime behavior.
#[derive(Default)]
pub struct Configuration {
    /// Adapter around a set of trace buffers which the data path threads will
    /// write trace events to at runtime for debugging and analysis purposes
    pub(crate) trace_buffer_manager: Option<Box<dyn crate::TraceConfiguration>>,

    /// Should the event processing loop busy poll and not block in the kernel
    /// if there are pending IOs. Busy polling avoids the cost of a thread
    /// wakeup when an IO completes, but burns a CPU core instead. Offers
    /// a latency vs. CPU utilization tradeoff. If using this feature, keep it
    /// limited to a small handful of threads, which should be pinned to
    /// specific CPU cores. This is a power user feature that should only be
    /// used with careful testing and experimentation.
    pub(crate) busy_poll: BusyPoll,

    /// Optional clock provider for the runtime. When set, the runtime uses
    /// this clock for all timing operations (sleep, deadlines, etc.).
    #[cfg(feature = "virtual-clock")]
    pub(crate) clock: Option<std::rc::Rc<dyn crate::clock::Clock>>,
}

/// Controls whether the event loop busy-polls for I/O completion.
///
/// Busy polling can reduce latency at the cost of CPU usage.
#[derive(Debug, PartialEq, Eq, Clone, Copy, Default)]
pub enum BusyPoll {
    /// Never busy poll; always suspend the thread until I/O completes.
    #[default]
    Never,

    /// Always busy poll; never suspend the thread.
    Always,

    /// Busy poll for the specified duration, then suspend.
    Until(Duration),
}

impl From<Option<Duration>> for BusyPoll {
    fn from(value: Option<Duration>) -> Self {
        match value {
            None => BusyPoll::Never,
            Some(d) => BusyPoll::Until(d),
        }
    }
}

impl Configuration {
    /// Creates a new `Configuration` with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the trace buffer manager for runtime event tracing.
    pub fn set_trace_buffer_manager(
        mut self,
        trace_buffer_manager: Box<dyn crate::TraceConfiguration>,
    ) -> Self {
        self.trace_buffer_manager = Some(trace_buffer_manager);
        self
    }

    /// Sets the busy polling behavior for the event loop.
    pub fn set_busy_poll(mut self, busy_poll: BusyPoll) -> Self {
        self.busy_poll = busy_poll;
        self
    }

    /// Sets the clock provider for the runtime.
    ///
    /// When a [`VirtualClock`](crate::clock::VirtualClock) is provided, all timing
    /// operations (sleep, deadlines) use virtual time instead of real system time.
    /// This enables deterministic, instant-resolving timeout tests.
    ///
    /// # Examples
    ///
    /// ```rust,no_run
    /// use std::time::Instant;
    /// use kimojio::configuration::Configuration;
    /// use kimojio::clock::VirtualClock;
    ///
    /// let clock = VirtualClock::new(Instant::now());
    /// let config = Configuration::new().set_clock(clock);
    /// ```
    #[cfg(feature = "virtual-clock")]
    pub fn set_clock(mut self, clock: impl crate::clock::Clock) -> Self {
        self.clock = Some(std::rc::Rc::new(clock));
        self
    }
}

#[cfg(test)]
mod test {
    use crate::configuration::BusyPoll;

    #[test]
    fn configuration_default_test() {
        let config = crate::Configuration::new();
        assert_eq!(config.busy_poll, BusyPoll::Never);
        assert!(config.trace_buffer_manager.is_none());
    }
}
