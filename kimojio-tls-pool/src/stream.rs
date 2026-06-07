// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use openssl::ssl::ErrorCode;
use rustix::event::{PollFd, PollFlags, poll};
use std::collections::VecDeque;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::operation::{self, CompletionStatus, OperationKind, OperationWork};
use crate::pool::{PoolInner, ReadinessInterest};
use crate::tls::BoxedTlsStream;
use crate::{OperationError, OperationResult, PoolStatsSnapshot};

/// Callback invoked when a submitted TLS operation completes.
pub type CompletionCallback<T> = Box<dyn FnOnce(OperationResult<T>) + Send + 'static>;

type StreamJob = Box<dyn FnOnce(Arc<StreamState>) -> DispatchResult + Send + 'static>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchResult {
    Completed,
    Pending,
}

enum TlsAttempt<T> {
    Ready(OperationResult<T>),
    WouldBlock(ReadinessInterest),
}

/// User-facing TLS stream handle created by a pool.
#[derive(Clone)]
pub struct TlsStream {
    state: Arc<StreamState>,
}

impl TlsStream {
    #[cfg(test)]
    pub(crate) fn new(pool: Arc<PoolInner>) -> Self {
        Self {
            state: Arc::new(StreamState {
                pool,
                tls: Mutex::new(None),
                queue: Mutex::new(StreamQueue::default()),
                stats: StreamStats::default(),
            }),
        }
    }

    pub(crate) fn from_tls(pool: Arc<PoolInner>, tls: BoxedTlsStream) -> Self {
        Self {
            state: Arc::new(StreamState {
                pool,
                tls: Mutex::new(Some(tls)),
                queue: Mutex::new(StreamQueue::default()),
                stats: StreamStats::default(),
            }),
        }
    }

    /// Submits a custom runtime-independent operation for this stream.
    ///
    /// Use [`TlsStream::read`] and [`TlsStream::write`] for OpenSSL-backed TLS
    /// I/O. This lower-level entry point is intended for tests and adapters that
    /// need the same placement, callback, and same-stream serialization behavior.
    #[cfg(test)]
    pub(crate) fn submit_operation<T, F>(
        &self,
        operation_size: usize,
        operation: F,
        callback: CompletionCallback<T>,
    ) -> OperationResult<()>
    where
        T: Send + 'static,
        F: FnOnce() -> OperationResult<T> + Send + 'static,
    {
        self.submit_work(OperationWork::new(
            #[cfg(test)]
            OperationKind::Generic,
            operation_size,
            Box::new(operation),
            callback,
        ))
    }

    /// Reads up to `buffer_len` decrypted bytes and reports them through `callback`.
    pub fn read(
        &self,
        buffer_len: usize,
        callback: CompletionCallback<Vec<u8>>,
    ) -> OperationResult<()> {
        let max = self.state.pool.config().max_read_len();
        if buffer_len > max {
            callback(Err(OperationError::ReadTooLarge {
                requested: buffer_len,
                max,
            }));
            return Ok(());
        }
        let state = Arc::clone(&self.state);
        let fd = state.raw_fd()?;
        let callback = Arc::new(Mutex::new(Some(callback)));
        state.submit(Box::new(move |state| {
            schedule_read_after_readiness(state, fd, buffer_len, callback)
        }))
    }

    /// Submits a write work item for this stream.
    fn submit_write<F>(
        &self,
        byte_len: usize,
        write: F,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()>
    where
        F: FnOnce() -> OperationResult<usize> + Send + 'static,
    {
        let fd = self.state.raw_fd()?;
        self.submit_work(OperationWork::new(
            OperationKind::Write { fd },
            byte_len,
            Box::new(write),
            callback,
        ))
    }

    /// Writes all bytes and reports the plaintext byte count through `callback`.
    pub fn write(
        &self,
        bytes: Vec<u8>,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()> {
        let byte_len = bytes.len();
        let state = Arc::clone(&self.state);
        self.submit_write(byte_len, move || state.write_tls(bytes), callback)
    }

    /// Returns a snapshot of the parent pool statistics.
    pub fn stats(&self) -> PoolStatsSnapshot {
        self.state.pool.stats.snapshot()
    }

    /// Returns stream-local operation statistics.
    pub fn stream_stats(&self) -> StreamStatsSnapshot {
        self.state.stats.snapshot()
    }

    fn submit_work<T>(&self, work: OperationWork<T>) -> OperationResult<()>
    where
        T: Send + 'static,
    {
        self.state
            .submit(Box::new(move |state| start_operation(state, work)))
    }
}

pub(crate) struct StreamState {
    pool: Arc<PoolInner>,
    tls: Mutex<Option<BoxedTlsStream>>,
    queue: Mutex<StreamQueue>,
    stats: StreamStats,
}

impl StreamState {
    fn submit(self: &Arc<Self>, job: StreamJob) -> OperationResult<()> {
        if self.pool_is_shutdown() {
            return Err(OperationError::Shutdown);
        }

        self.pool.stats.record_submitted();
        self.stats.record_submitted();
        let start = {
            let mut queue = self.queue.lock().expect("stream queue poisoned");
            if queue.in_flight {
                queue.pending.push_back(job);
                self.pool.stats.record_stream_queued();
                self.stats.record_queued();
                None
            } else {
                queue.in_flight = true;
                Some(job)
            }
        };

        if let Some(job) = start {
            self.drain_ready(job);
        }
        Ok(())
    }

    fn finish(self: &Arc<Self>) {
        if let Some(job) = self.next_or_mark_idle() {
            self.drain_ready(job);
        }
    }

    fn drain_ready(self: &Arc<Self>, first: StreamJob) {
        let mut next = Some(first);
        while let Some(job) = next {
            match job(Arc::clone(self)) {
                DispatchResult::Pending => return,
                DispatchResult::Completed => {
                    next = self.next_or_mark_idle();
                }
            }
        }
    }

    fn next_or_mark_idle(&self) -> Option<StreamJob> {
        let mut queue = self.queue.lock().expect("stream queue poisoned");
        if let Some(job) = queue.pending.pop_front() {
            Some(job)
        } else {
            queue.in_flight = false;
            None
        }
    }

    fn pool_is_shutdown(&self) -> bool {
        self.pool.is_shutdown()
    }

    fn read_tls_once(&self, buffer_len: usize) -> TlsAttempt<Vec<u8>> {
        let max = self.pool.config().max_read_len();
        if buffer_len > max {
            return TlsAttempt::Ready(Err(OperationError::ReadTooLarge {
                requested: buffer_len,
                max,
            }));
        }
        let mut guard = match self.tls.lock() {
            Ok(guard) => guard,
            Err(_) => return TlsAttempt::Ready(Err(OperationError::StatePoisoned)),
        };
        let Some(tls) = guard.as_mut() else {
            return TlsAttempt::Ready(Err(OperationError::Shutdown));
        };
        if let Err(error) = crate::tls::set_nonblocking_stream(tls) {
            return TlsAttempt::Ready(Err(error));
        }
        let mut buffer = vec![0_u8; buffer_len];
        if buffer_len == 0 {
            return TlsAttempt::Ready(Ok(buffer));
        }

        match tls.ssl_read(&mut buffer) {
            Ok(amount) => {
                buffer.truncate(amount);
                TlsAttempt::Ready(Ok(buffer))
            }
            Err(error) => match ssl_wait_interest(&error) {
                Some(interest) => TlsAttempt::WouldBlock(interest),
                None => TlsAttempt::Ready(map_ssl_error(error)),
            },
        }
    }

    fn write_tls(&self, bytes: Vec<u8>) -> OperationResult<usize> {
        let byte_len = bytes.len();
        let mut guard = self.tls.lock().map_err(|_| OperationError::StatePoisoned)?;
        let tls = guard.as_mut().ok_or(OperationError::Shutdown)?;
        crate::tls::set_blocking_stream(tls)?;

        let mut written = 0;
        while written < byte_len {
            match tls.ssl_write(&bytes[written..]) {
                Ok(0) => {
                    return Err(OperationError::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "TLS write returned zero bytes",
                    )));
                }
                Ok(amount) => written += amount,
                Err(error) => {
                    if let Some(interest) = ssl_wait_interest(&error) {
                        wait_for_fd(crate::tls::raw_fd(tls), interest)?;
                    } else {
                        return map_ssl_error(error);
                    }
                }
            }
        }
        Ok(byte_len)
    }

    fn raw_fd(&self) -> OperationResult<std::os::fd::RawFd> {
        let guard = self.tls.lock().map_err(|_| OperationError::StatePoisoned)?;
        let tls = guard.as_ref().ok_or(OperationError::Shutdown)?;
        Ok(crate::tls::raw_fd(tls))
    }
}

#[derive(Default)]
struct StreamQueue {
    in_flight: bool,
    pending: VecDeque<StreamJob>,
}

#[derive(Default, Debug)]
struct StreamStats {
    submitted: AtomicUsize,
    queued: AtomicUsize,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl StreamStats {
    fn record_submitted(&self) {
        self.submitted.fetch_add(1, Ordering::Relaxed);
    }

    fn record_queued(&self) {
        self.queued.fetch_add(1, Ordering::Relaxed);
    }

    fn record_started(&self) {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.max_active.fetch_max(active, Ordering::AcqRel);
    }

    fn record_finished(&self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }

    fn snapshot(&self) -> StreamStatsSnapshot {
        StreamStatsSnapshot {
            submitted: self.submitted.load(Ordering::Relaxed),
            queued: self.queued.load(Ordering::Relaxed),
            active: self.active.load(Ordering::Acquire),
            max_active: self.max_active.load(Ordering::Acquire),
        }
    }
}

/// Point-in-time stream-local operation statistics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamStatsSnapshot {
    /// Operations submitted to this stream.
    pub submitted: usize,
    /// Operations queued behind an already in-flight same-stream operation.
    pub queued: usize,
    /// Currently active operations for this stream.
    pub active: usize,
    /// Maximum simultaneous active operations observed for this stream.
    pub max_active: usize,
}

fn start_operation<T>(state: Arc<StreamState>, work: OperationWork<T>) -> DispatchResult
where
    T: Send + 'static,
{
    let operation_size = work.size();
    let kind = work.kind();
    let (operation, callback) = work.into_parts();

    match kind {
        OperationKind::Read { fd } => schedule_after_readiness(
            state,
            fd,
            ReadinessInterest::Read,
            operation_size,
            operation,
            callback,
        ),
        OperationKind::Write { .. } => {
            start_generic_operation(state, operation_size, kind, operation, callback)
        }
        #[cfg(test)]
        OperationKind::Generic => {
            start_generic_operation(state, operation_size, kind, operation, callback)
        }
    }
}

fn start_generic_operation<T>(
    state: Arc<StreamState>,
    operation_size: usize,
    kind: OperationKind,
    operation: crate::operation::OperationFn<T>,
    callback: CompletionCallback<T>,
) -> DispatchResult
where
    T: Send + 'static,
{
    match state.pool.choose_placement(kind, operation_size) {
        crate::OperationPlacement::Immediate => {
            state.pool.stats.record_immediate();
            state.stats.record_started();
            let status = operation::complete(operation, callback);
            state.stats.record_finished();
            record_completion(&state, None, status);
            DispatchResult::Completed
        }
        crate::OperationPlacement::Background { executor } => {
            state.pool.stats.record_background_routed(executor);
            state.pool.stats.record_executor_queued(executor);
            let pool = Arc::clone(&state.pool);
            let job_state = Arc::clone(&state);
            let callback = Arc::new(Mutex::new(Some(callback)));
            let send_error_callback = Arc::clone(&callback);
            let job = Box::new(move || {
                job_state.stats.record_started();
                let callback = callback
                    .lock()
                    .expect("operation callback mutex poisoned")
                    .take()
                    .expect("operation callback missing");
                let status = operation::complete(operation, callback);
                job_state.stats.record_finished();
                record_completion(&job_state, Some(executor), status);
                job_state.finish();
            });

            if let Err(error) = pool.send_to_executor(executor, operation_size, job) {
                state.pool.stats.record_failed(None);
                if let Some(callback) = send_error_callback
                    .lock()
                    .expect("operation callback mutex poisoned")
                    .take()
                {
                    callback(Err(error));
                }
                DispatchResult::Completed
            } else {
                DispatchResult::Pending
            }
        }
    }
}

fn schedule_after_readiness<T>(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    interest: ReadinessInterest,
    operation_size: usize,
    operation: crate::operation::OperationFn<T>,
    callback: CompletionCallback<T>,
) -> DispatchResult
where
    T: Send + 'static,
{
    let pool = Arc::clone(&state.pool);
    let executor = match pool.choose_placement(OperationKind::Read { fd }, operation_size) {
        crate::OperationPlacement::Immediate => 0,
        crate::OperationPlacement::Background { executor } => executor,
    };
    let job_state = Arc::clone(&state);
    let callback = Arc::new(Mutex::new(Some(callback)));
    let readiness_error_callback = Arc::clone(&callback);
    let job = Box::new(move || {
        job_state.stats.record_started();
        let callback = callback
            .lock()
            .expect("operation callback mutex poisoned")
            .take()
            .expect("operation callback missing");
        let status = operation::complete(operation, callback);
        job_state.stats.record_finished();
        record_completion(&job_state, Some(executor), status);
        job_state.finish();
    });
    let shutdown_state = Arc::clone(&state);
    let shutdown_callback = Arc::clone(&readiness_error_callback);
    let shutdown = Box::new(move || {
        shutdown_state.pool.stats.record_failed(None);
        if let Some(callback) = shutdown_callback
            .lock()
            .expect("operation callback mutex poisoned")
            .take()
        {
            callback(Err(OperationError::Shutdown));
        }
        shutdown_state.finish();
    });

    if let Err(error) =
        pool.send_to_executor_when_ready(fd, interest, executor, operation_size, job, shutdown)
    {
        state.pool.stats.record_failed(None);
        if let Some(callback) = readiness_error_callback
            .lock()
            .expect("operation callback mutex poisoned")
            .take()
        {
            callback(Err(error));
        }
        DispatchResult::Completed
    } else {
        DispatchResult::Pending
    }
}

fn schedule_read_after_readiness(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    buffer_len: usize,
    callback: Arc<Mutex<Option<CompletionCallback<Vec<u8>>>>>,
) -> DispatchResult {
    let executor = match state
        .pool
        .choose_placement(OperationKind::Read { fd }, buffer_len)
    {
        crate::OperationPlacement::Immediate => 0,
        crate::OperationPlacement::Background { executor } => executor,
    };
    let pool = Arc::clone(&state.pool);
    let job_state = Arc::clone(&state);
    let job_callback = Arc::clone(&callback);
    let job = Box::new(move || {
        attempt_read_on_executor(job_state, fd, buffer_len, job_callback, executor);
    });
    let shutdown_state = Arc::clone(&state);
    let shutdown_callback = Arc::clone(&callback);
    let shutdown = Box::new(move || {
        shutdown_state.pool.stats.record_failed(None);
        if let Some(callback) = shutdown_callback
            .lock()
            .expect("read callback mutex poisoned")
            .take()
        {
            callback(Err(OperationError::Shutdown));
        }
        shutdown_state.finish();
    });

    if pool
        .send_to_executor_when_ready(
            fd,
            ReadinessInterest::Read,
            executor,
            buffer_len,
            job,
            shutdown,
        )
        .is_err()
    {
        state.pool.stats.record_failed(None);
        if let Some(callback) = callback
            .lock()
            .expect("read callback mutex poisoned")
            .take()
        {
            callback(Err(OperationError::Shutdown));
        }
        DispatchResult::Completed
    } else {
        DispatchResult::Pending
    }
}

fn attempt_read_on_executor(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    buffer_len: usize,
    callback: Arc<Mutex<Option<CompletionCallback<Vec<u8>>>>>,
    executor: usize,
) {
    state.stats.record_started();
    match state.read_tls_once(buffer_len) {
        TlsAttempt::Ready(result) => {
            state.stats.record_finished();
            let success = result.is_ok();
            if let Some(callback) = callback
                .lock()
                .expect("read callback mutex poisoned")
                .take()
            {
                let status = deliver_result(result, callback);
                record_completion(&state, Some(executor), status);
            } else if success {
                state.pool.stats.record_completed(Some(executor));
            } else {
                state.pool.stats.record_failed(Some(executor));
            }
            state.finish();
        }
        TlsAttempt::WouldBlock(_interest) => {
            state.stats.record_finished();
            let result = schedule_read_after_readiness(state, fd, buffer_len, callback);
            debug_assert_eq!(result, DispatchResult::Pending);
        }
    }
}

fn deliver_result<T>(
    result: OperationResult<T>,
    callback: CompletionCallback<T>,
) -> CompletionStatus {
    let success = result.is_ok();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| callback(result))) {
        Ok(()) if success => CompletionStatus::Succeeded,
        Ok(()) => CompletionStatus::Failed,
        Err(_) => CompletionStatus::Panicked,
    }
}

fn map_ssl_error<T>(error: openssl::ssl::Error) -> OperationResult<T> {
    match error.code() {
        ErrorCode::WANT_READ | ErrorCode::WANT_WRITE => Err(OperationError::Io(
            std::io::Error::from(std::io::ErrorKind::WouldBlock),
        )),
        ErrorCode::SYSCALL => match error.into_io_error() {
            Ok(error) => Err(OperationError::Io(error)),
            Err(error) => Err(OperationError::Handshake(error.to_string())),
        },
        ErrorCode::SSL => Err(OperationError::Handshake(error.to_string())),
        _ => Err(OperationError::Handshake(error.to_string())),
    }
}

fn ssl_wait_interest(error: &openssl::ssl::Error) -> Option<ReadinessInterest> {
    match error.code() {
        ErrorCode::WANT_READ => Some(ReadinessInterest::Read),
        ErrorCode::WANT_WRITE => Some(ReadinessInterest::Write),
        _ => None,
    }
}

fn wait_for_fd(fd: std::os::fd::RawFd, interest: ReadinessInterest) -> OperationResult<()> {
    let flags = match interest {
        ReadinessInterest::Read => PollFlags::IN,
        ReadinessInterest::Write => PollFlags::OUT,
    };
    // SAFETY: callers only pass fds owned by the stream being operated on, and
    // the borrow is limited to this poll call.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let mut fds = [PollFd::from_borrowed_fd(borrowed, flags)];
    poll(&mut fds, None).map(|_| ()).map_err(|error| {
        OperationError::Io(std::io::Error::from_raw_os_error(error.raw_os_error()))
    })
}

fn record_completion(state: &StreamState, executor: Option<usize>, status: CompletionStatus) {
    match status {
        CompletionStatus::Succeeded => state.pool.stats.record_completed(executor),
        CompletionStatus::Failed | CompletionStatus::Panicked => {
            state.pool.stats.record_failed(executor);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::{PlacementMode, PoolConfig, TlsPool};

    #[test]
    fn callback_is_invoked_once_for_success() {
        let pool = TlsPool::default();
        let stream = pool.stream();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let (sender, receiver) = mpsc::channel();

        stream
            .submit_operation(
                1,
                || Ok(7),
                Box::new(move |result| {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        assert_eq!(receiver.recv().unwrap(), 7);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn callback_is_invoked_once_for_failure() {
        let pool = TlsPool::default();
        let stream = pool.stream();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let (sender, receiver) = mpsc::channel();

        stream
            .submit_operation::<(), _>(
                1,
                || Err(OperationError::Busy),
                Box::new(move |result| {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    sender
                        .send(matches!(result, Err(OperationError::Busy)))
                        .unwrap();
                }),
            )
            .unwrap();

        assert!(receiver.recv().unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(pool.stats().failed, 1);
    }

    #[test]
    fn same_stream_operations_are_serialized() {
        let pool =
            TlsPool::new(PoolConfig::new(2).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        let stream = pool.stream();
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let first_started = Arc::new(Barrier::new(2));
        let release_first = Arc::new(Barrier::new(2));
        let (sender, receiver) = mpsc::channel();

        let active_first = Arc::clone(&active);
        let max_first = Arc::clone(&max_active);
        let first_started_job = Arc::clone(&first_started);
        let release_first_job = Arc::clone(&release_first);
        let sender_first = sender.clone();
        stream
            .submit_operation(
                32 * 1024,
                move || {
                    let now = active_first.fetch_add(1, Ordering::SeqCst) + 1;
                    max_first.fetch_max(now, Ordering::SeqCst);
                    first_started_job.wait();
                    release_first_job.wait();
                    active_first.fetch_sub(1, Ordering::SeqCst);
                    Ok(1)
                },
                Box::new(move |result| {
                    sender_first.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        first_started.wait();

        let active_second = Arc::clone(&active);
        let max_second = Arc::clone(&max_active);
        let sender_second = sender;
        stream
            .submit_operation(
                32 * 1024,
                move || {
                    let now = active_second.fetch_add(1, Ordering::SeqCst) + 1;
                    max_second.fetch_max(now, Ordering::SeqCst);
                    active_second.fetch_sub(1, Ordering::SeqCst);
                    Ok(2)
                },
                Box::new(move |result| {
                    sender_second.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        thread::sleep(Duration::from_millis(10));
        assert_eq!(max_active.load(Ordering::SeqCst), 1);

        release_first.wait();
        assert_eq!(receiver.recv().unwrap(), 1);
        assert_eq!(receiver.recv().unwrap(), 2);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
        assert_eq!(pool.stats().queued, 3);
    }

    #[test]
    fn accepted_queued_background_work_gets_shutdown_callback() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        let stream = pool.stream();
        let (release_sender, release_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();
        let first_result = result_sender.clone();

        stream
            .submit_operation(
                32 * 1024,
                move || {
                    release_receiver.recv().unwrap();
                    Ok(1)
                },
                Box::new(move |result| {
                    first_result.send(result.map_err(|_| "err")).unwrap();
                }),
            )
            .unwrap();

        stream
            .submit_operation(
                32 * 1024,
                || Ok(2),
                Box::new(move |result| {
                    result_sender.send(result.map_err(|_| "err")).unwrap();
                }),
            )
            .unwrap();

        pool.shutdown();
        release_sender.send(()).unwrap();

        assert_eq!(result_receiver.recv().unwrap(), Ok(1));
        assert_eq!(result_receiver.recv().unwrap(), Err("err"));
        assert_eq!(stream.stream_stats().active, 0);
    }

    #[test]
    fn immediate_callback_panic_does_not_wedge_stream() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::ImmediateOnly))
                .unwrap();
        let stream = pool.stream();

        stream
            .submit_operation(1, || Ok(()), Box::new(|_| panic!("callback panic")))
            .unwrap();

        let (sender, receiver) = mpsc::channel();
        stream
            .submit_operation(
                1,
                || Ok(7),
                Box::new(move |result| {
                    sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        assert_eq!(receiver.recv().unwrap(), 7);
        assert_eq!(stream.stream_stats().active, 0);
        assert_eq!(pool.stats().failed, 1);
    }

    #[test]
    fn background_callback_panic_does_not_stop_worker() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        let stream = pool.stream();

        stream
            .submit_operation(32 * 1024, || Ok(()), Box::new(|_| panic!("callback panic")))
            .unwrap();

        let (sender, receiver) = mpsc::channel();
        stream
            .submit_operation(
                32 * 1024,
                || Ok(9),
                Box::new(move |result| {
                    sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        assert_eq!(receiver.recv_timeout(Duration::from_secs(1)).unwrap(), 9);
        assert_eq!(stream.stream_stats().active, 0);
        assert_eq!(pool.stats().failed, 1);
    }

    #[test]
    fn operation_panic_reports_error_and_continues() {
        let pool = TlsPool::default();
        let stream = pool.stream();
        let (sender, receiver) = mpsc::channel();

        stream
            .submit_operation::<(), _>(
                1,
                || panic!("operation panic"),
                Box::new(move |result| {
                    sender
                        .send(matches!(result, Err(OperationError::Panic("operation"))))
                        .unwrap();
                }),
            )
            .unwrap();

        assert!(receiver.recv().unwrap());
        assert_eq!(stream.stream_stats().active, 0);
        assert_eq!(pool.stats().failed, 1);
    }

    #[test]
    fn read_rejects_oversized_buffer_before_tls_access() {
        let pool = TlsPool::new(
            PoolConfig::new(1)
                .with_placement_mode(PlacementMode::ImmediateOnly)
                .with_max_read_len(8),
        )
        .unwrap();
        let stream = pool.stream();
        let (sender, receiver) = mpsc::channel();

        stream
            .read(
                9,
                Box::new(move |result| {
                    sender
                        .send(matches!(
                            result,
                            Err(OperationError::ReadTooLarge {
                                requested: 9,
                                max: 8
                            })
                        ))
                        .unwrap();
                }),
            )
            .unwrap();

        assert!(receiver.recv().unwrap());
    }

    #[test]
    fn in_flight_background_job_survives_visible_handle_drop() {
        let pool =
            TlsPool::new(PoolConfig::new(1).with_placement_mode(PlacementMode::BackgroundOnly))
                .unwrap();
        let stream = pool.stream();
        let (release_sender, release_receiver) = mpsc::channel();
        let (result_sender, result_receiver) = mpsc::channel();

        stream
            .submit_operation(
                32 * 1024,
                move || {
                    release_receiver.recv().unwrap();
                    Ok(42)
                },
                Box::new(move |result| {
                    result_sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        drop(stream);
        drop(pool);
        release_sender.send(()).unwrap();

        assert_eq!(
            result_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            42
        );
    }
}
