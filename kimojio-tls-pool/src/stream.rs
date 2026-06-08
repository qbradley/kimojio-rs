// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use openssl::ssl::ErrorCode;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[cfg(test)]
use crate::operation::{self, OperationWork};
use crate::operation::{CompletionStatus, OperationKind};
use crate::pool::{PoolInner, ReadinessInterest};
use crate::tls::BoxedTlsStream;
use crate::{OperationError, OperationResult, PoolStatsSnapshot};

/// Callback invoked when a submitted TLS operation completes.
///
/// Callbacks are notifications from the thread that completed the operation.
/// They should not synchronously wait for follow-up work on the same stream,
/// because same-stream operations remain serialized until the callback returns.
pub type CompletionCallback<T> = Box<dyn FnOnce(OperationResult<T>) + Send + 'static>;

type StreamJob = Box<dyn FnOnce(Arc<StreamState>) -> DispatchResult + Send + 'static>;
type SharedCallback<T> = Arc<Mutex<Option<CompletionCallback<T>>>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DispatchResult {
    Completed,
    Pending,
}

enum TlsAttempt<T> {
    Ready(OperationResult<T>),
    WouldBlock(ReadinessInterest),
}

enum WriteBytes {
    Owned(Vec<u8>),
    Shared(Arc<[u8]>),
}

impl WriteBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Owned(bytes) => bytes,
            Self::Shared(bytes) => bytes,
        }
    }
}

enum WriteState {
    Single {
        bytes: WriteBytes,
        offset: usize,
    },
    Batch {
        chunks: Vec<Arc<[u8]>>,
        index: usize,
        offset: usize,
        written: usize,
        total_len: usize,
    },
}

impl WriteState {
    fn owned(bytes: Vec<u8>) -> Self {
        Self::Single {
            bytes: WriteBytes::Owned(bytes),
            offset: 0,
        }
    }

    fn shared(bytes: Arc<[u8]>) -> Self {
        Self::Single {
            bytes: WriteBytes::Shared(bytes),
            offset: 0,
        }
    }

    fn batch(chunks: Vec<Arc<[u8]>>) -> Self {
        let total_len = chunks.iter().map(|chunk| chunk.len()).sum();
        let chunks = chunks
            .into_iter()
            .filter(|chunk| !chunk.is_empty())
            .collect();
        Self::Batch {
            chunks,
            index: 0,
            offset: 0,
            written: 0,
            total_len,
        }
    }

    fn current_slice(&self) -> Option<&[u8]> {
        match self {
            Self::Single { bytes, offset } => {
                let bytes = bytes.as_slice();
                (*offset < bytes.len()).then_some(&bytes[*offset..])
            }
            Self::Batch {
                chunks,
                index,
                offset,
                ..
            } => chunks.get(*index).and_then(|chunk| {
                let chunk = &chunk[*offset..];
                (!chunk.is_empty()).then_some(chunk)
            }),
        }
    }

    fn advance(&mut self, amount: usize) {
        match self {
            Self::Single { offset, .. } => *offset += amount,
            Self::Batch {
                chunks,
                index,
                offset,
                written,
                ..
            } => {
                *written += amount;
                *offset += amount;
                while let Some(chunk) = chunks.get(*index) {
                    if *offset < chunk.len() {
                        break;
                    }
                    *offset = 0;
                    *index += 1;
                }
            }
        }
    }

    fn total_len(&self) -> usize {
        match self {
            Self::Single { bytes, .. } => bytes.as_slice().len(),
            Self::Batch { total_len, .. } => *total_len,
        }
    }
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
    ///
    /// Reads always attempt nonblocking TLS progress first so already-buffered
    /// plaintext can complete without executor handoff. If OpenSSL needs socket
    /// readiness, the read is parked without occupying an executor and later
    /// resumed on a pool executor.
    pub fn read(
        &self,
        buffer_len: usize,
        callback: CompletionCallback<Vec<u8>>,
    ) -> OperationResult<()> {
        let max = self.state.pool.config().max_read_len();
        if buffer_len > max {
            let callback = Arc::new(Mutex::new(Some(callback)));
            return self.state.submit(Box::new(move |state| {
                state.pool.stats.record_immediate();
                state.stats.record_started();
                state.stats.record_finished();
                let status = deliver_shared_result(
                    &callback,
                    Err(OperationError::ReadTooLarge {
                        requested: buffer_len,
                        max,
                    }),
                );
                record_completion(&state, None, status);
                DispatchResult::Completed
            }));
        }
        let state = Arc::clone(&self.state);
        let fd = state.raw_fd()?;
        let callback = Arc::new(Mutex::new(Some(callback)));
        state.submit(Box::new(move |state| {
            attempt_read_initial(state, fd, buffer_len, callback)
        }))
    }

    /// Submits a write work item for this stream.
    fn submit_write_state(
        &self,
        write_state: WriteState,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()> {
        let byte_len = write_state.total_len();
        let fd = self.state.raw_fd()?;
        let write_state = Arc::new(Mutex::new(write_state));
        let callback = Arc::new(Mutex::new(Some(callback)));
        self.state.submit(Box::new(move |state| {
            start_write_operation(state, fd, byte_len, write_state, callback)
        }))
    }

    /// Writes all bytes and reports the plaintext byte count through `callback`.
    ///
    /// Writes make nonblocking TLS progress. If the underlying transport would
    /// block, the operation is parked on the required readiness interest and
    /// resumed on a pool executor.
    pub fn write(
        &self,
        bytes: Vec<u8>,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()> {
        self.submit_write_state(WriteState::owned(bytes), callback)
    }

    /// Writes shared bytes and reports the plaintext byte count through `callback`.
    ///
    /// This avoids allocating or copying the payload for each submitted write
    /// when the same payload can be safely shared across operations.
    pub fn write_shared(
        &self,
        bytes: Arc<[u8]>,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()> {
        self.submit_write_state(WriteState::shared(bytes), callback)
    }

    /// Writes a batch of shared byte chunks and reports the total plaintext bytes.
    ///
    /// Batching amortizes callback and queueing overhead for high-throughput
    /// workloads while preserving same-stream serialization.
    pub fn write_batch(
        &self,
        chunks: Vec<Arc<[u8]>>,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()> {
        self.submit_write_state(WriteState::batch(chunks), callback)
    }

    /// Returns a snapshot of the parent pool statistics.
    pub fn stats(&self) -> PoolStatsSnapshot {
        self.state.pool.stats.snapshot()
    }

    /// Returns stream-local operation statistics.
    pub fn stream_stats(&self) -> StreamStatsSnapshot {
        self.state.stats.snapshot()
    }

    #[cfg(test)]
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

    fn raw_fd(&self) -> OperationResult<std::os::fd::RawFd> {
        let guard = self.tls.lock().map_err(|_| OperationError::StatePoisoned)?;
        let tls = guard.as_ref().ok_or(OperationError::Shutdown)?;
        Ok(crate::tls::raw_fd(tls))
    }

    fn write_tls_progress(&self, write: &mut WriteState) -> TlsAttempt<usize> {
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

        loop {
            let Some(bytes) = write.current_slice() else {
                return TlsAttempt::Ready(Ok(write.total_len()));
            };
            match tls.ssl_write(bytes) {
                Ok(0) => {
                    return TlsAttempt::Ready(Err(OperationError::Io(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "TLS write returned zero bytes",
                    ))));
                }
                Ok(amount) => write.advance(amount),
                Err(error) => match ssl_wait_interest(&error) {
                    Some(interest) => return TlsAttempt::WouldBlock(interest),
                    None => return TlsAttempt::Ready(map_ssl_error(error)),
                },
            }
        }
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

#[cfg(test)]
fn start_operation<T>(state: Arc<StreamState>, work: OperationWork<T>) -> DispatchResult
where
    T: Send + 'static,
{
    let operation_size = work.size();
    let kind = work.kind();
    let (operation, callback) = work.into_parts();

    match kind {
        OperationKind::Read { .. } => {
            unreachable!("read operations use the dedicated readiness-aware path")
        }
        OperationKind::Write { .. } => {
            start_generic_operation(state, operation_size, kind, operation, callback)
        }
        #[cfg(test)]
        OperationKind::Generic => {
            start_generic_operation(state, operation_size, kind, operation, callback)
        }
    }
}

fn start_write_operation(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    operation_size: usize,
    write: Arc<Mutex<WriteState>>,
    callback: SharedCallback<usize>,
) -> DispatchResult {
    match state
        .pool
        .choose_placement(OperationKind::Write { fd }, operation_size)
    {
        crate::OperationPlacement::Immediate => {
            state.pool.stats.record_immediate();
            attempt_write_initial(state, fd, operation_size, write, callback)
        }
        crate::OperationPlacement::Background { executor } => {
            state.pool.stats.record_background_routed(executor);
            state.pool.stats.record_executor_queued(executor);
            let pool = Arc::clone(&state.pool);
            let job_state = Arc::clone(&state);
            let send_error_callback = Arc::clone(&callback);
            let job = Box::new(move || {
                attempt_write_on_executor(
                    job_state,
                    fd,
                    operation_size,
                    write,
                    callback,
                    executor,
                    false,
                );
            });

            if let Err(error) = pool.send_to_executor(executor, operation_size, job) {
                let status = deliver_shared_result(&send_error_callback, Err(error));
                record_completion(&state, None, status);
                DispatchResult::Completed
            } else {
                DispatchResult::Pending
            }
        }
    }
}

fn attempt_write_initial(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    operation_size: usize,
    write: Arc<Mutex<WriteState>>,
    callback: SharedCallback<usize>,
) -> DispatchResult {
    state.stats.record_started();
    let attempt = {
        let mut write = write.lock().expect("write state mutex poisoned");
        state.write_tls_progress(&mut write)
    };
    state.stats.record_finished();

    match attempt {
        TlsAttempt::Ready(result) => {
            let status = deliver_shared_result(&callback, result);
            record_completion(&state, None, status);
            DispatchResult::Completed
        }
        TlsAttempt::WouldBlock(interest) => {
            match schedule_write_after_readiness(
                Arc::clone(&state),
                fd,
                interest,
                operation_size,
                Arc::clone(&write),
                Arc::clone(&callback),
            ) {
                Ok(()) => DispatchResult::Pending,
                Err(error) => {
                    let status = deliver_shared_result(&callback, Err(error));
                    record_completion(&state, None, status);
                    DispatchResult::Completed
                }
            }
        }
    }
}

fn attempt_write_on_executor(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    operation_size: usize,
    write: Arc<Mutex<WriteState>>,
    callback: SharedCallback<usize>,
    executor: usize,
    resumed_from_readiness: bool,
) {
    if resumed_from_readiness {
        state.pool.stats.record_readiness_resumed(executor);
    }
    state.stats.record_started();
    let attempt = {
        let mut write = write.lock().expect("write state mutex poisoned");
        state.write_tls_progress(&mut write)
    };
    state.stats.record_finished();

    match attempt {
        TlsAttempt::Ready(result) => {
            let status = deliver_shared_result(&callback, result);
            record_completion(&state, Some(executor), status);
            state.finish();
        }
        TlsAttempt::WouldBlock(interest) => {
            if let Err(error) = schedule_write_after_readiness(
                Arc::clone(&state),
                fd,
                interest,
                operation_size,
                Arc::clone(&write),
                Arc::clone(&callback),
            ) {
                let status = deliver_shared_result(&callback, Err(error));
                record_completion(&state, Some(executor), status);
                state.finish();
            }
        }
    }
}

fn schedule_write_after_readiness(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    interest: ReadinessInterest,
    operation_size: usize,
    write: Arc<Mutex<WriteState>>,
    callback: SharedCallback<usize>,
) -> Result<(), OperationError> {
    let pool = Arc::clone(&state.pool);
    let executor = match pool.choose_placement(OperationKind::Write { fd }, operation_size) {
        crate::OperationPlacement::Immediate => 0,
        crate::OperationPlacement::Background { executor } => executor,
    };
    let job_state = Arc::clone(&state);
    let job_callback = Arc::clone(&callback);
    let job = Box::new(move || {
        attempt_write_on_executor(
            job_state,
            fd,
            operation_size,
            write,
            job_callback,
            executor,
            true,
        );
    });
    let shutdown_state = Arc::clone(&state);
    let shutdown_callback = Arc::clone(&callback);
    let shutdown = Box::new(move || {
        let status = deliver_shared_result(&shutdown_callback, Err(OperationError::Shutdown));
        record_completion(&shutdown_state, None, status);
        shutdown_state.finish();
    });

    pool.send_to_executor_when_ready(fd, interest, executor, operation_size, job, shutdown)?;
    pool.stats.record_readiness_wait();
    Ok(())
}

#[cfg(test)]
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
                if let Some(callback) = send_error_callback
                    .lock()
                    .expect("operation callback mutex poisoned")
                    .take()
                {
                    let status = deliver_result(Err(error), callback);
                    record_completion(&state, None, status);
                } else {
                    state.pool.stats.record_failed(None);
                }
                DispatchResult::Completed
            } else {
                DispatchResult::Pending
            }
        }
    }
}

fn schedule_read_after_readiness(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    interest: ReadinessInterest,
    buffer_len: usize,
    callback: SharedCallback<Vec<u8>>,
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
        job_state.pool.stats.record_readiness_resumed(executor);
        attempt_read_on_executor(job_state, fd, buffer_len, job_callback, executor);
    });
    let shutdown_state = Arc::clone(&state);
    let shutdown_callback = Arc::clone(&callback);
    let shutdown = Box::new(move || {
        let status = deliver_shared_result(&shutdown_callback, Err(OperationError::Shutdown));
        record_completion(&shutdown_state, None, status);
        shutdown_state.finish();
    });

    match pool.send_to_executor_when_ready(fd, interest, executor, buffer_len, job, shutdown) {
        Ok(()) => {
            pool.stats.record_readiness_wait();
            DispatchResult::Pending
        }
        Err(error) => {
            let status = deliver_shared_result(&callback, Err(error));
            record_completion(&state, None, status);
            DispatchResult::Completed
        }
    }
}

fn attempt_read_initial(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    buffer_len: usize,
    callback: SharedCallback<Vec<u8>>,
) -> DispatchResult {
    state.pool.stats.record_immediate();
    state.stats.record_started();
    match state.read_tls_once(buffer_len) {
        TlsAttempt::Ready(result) => {
            state.stats.record_finished();
            let status = deliver_shared_result(&callback, result);
            record_completion(&state, None, status);
            DispatchResult::Completed
        }
        TlsAttempt::WouldBlock(interest) => {
            state.stats.record_finished();
            schedule_read_after_readiness(state, fd, interest, buffer_len, callback)
        }
    }
}

fn attempt_read_on_executor(
    state: Arc<StreamState>,
    fd: std::os::fd::RawFd,
    buffer_len: usize,
    callback: SharedCallback<Vec<u8>>,
    executor: usize,
) {
    state.stats.record_started();
    match state.read_tls_once(buffer_len) {
        TlsAttempt::Ready(result) => {
            state.stats.record_finished();
            let status = deliver_shared_result(&callback, result);
            record_completion(&state, Some(executor), status);
            state.finish();
        }
        TlsAttempt::WouldBlock(interest) => {
            state.stats.record_finished();
            let result = schedule_read_after_readiness(
                Arc::clone(&state),
                fd,
                interest,
                buffer_len,
                callback,
            );
            if result == DispatchResult::Completed {
                state.finish();
            }
        }
    }
}

fn deliver_shared_result<T>(
    callback: &SharedCallback<T>,
    result: OperationResult<T>,
) -> CompletionStatus {
    let success = result.is_ok();
    if let Some(callback) = callback
        .lock()
        .expect("operation callback mutex poisoned")
        .take()
    {
        deliver_result(result, callback)
    } else if success {
        CompletionStatus::Succeeded
    } else {
        CompletionStatus::Failed
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
