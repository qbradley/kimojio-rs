// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use crate::operation::{self, OperationKind, OperationWork};
use crate::pool::PoolInner;
use crate::tls::BoxedTlsStream;
use crate::{OperationError, OperationResult, PoolStatsSnapshot};

/// Callback invoked when a submitted TLS operation completes.
pub type CompletionCallback<T> = Box<dyn FnOnce(OperationResult<T>) + Send + 'static>;

type StreamJob = Box<dyn FnOnce(Arc<StreamState>) + Send + 'static>;

/// User-facing TLS stream handle created by a pool.
#[derive(Clone)]
pub struct TlsStream {
    state: Arc<StreamState>,
}

impl TlsStream {
    pub(crate) fn new(pool: Arc<PoolInner>) -> Self {
        Self {
            state: Arc::new(StreamState {
                pool,
                tls: Mutex::new(None),
                queue: Mutex::new(StreamQueue::default()),
            }),
        }
    }

    pub(crate) fn from_tls(pool: Arc<PoolInner>, tls: BoxedTlsStream) -> Self {
        Self {
            state: Arc::new(StreamState {
                pool,
                tls: Mutex::new(Some(tls)),
                queue: Mutex::new(StreamQueue::default()),
            }),
        }
    }

    /// Submits a runtime-independent operation for this stream.
    ///
    /// Later phases wire this low-level operation path to TLS reads and writes.
    pub fn submit_operation<T, F>(
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
            OperationKind::Generic,
            operation_size,
            Box::new(operation),
            callback,
        ))
    }

    /// Submits a read work item for this stream.
    pub fn submit_read<F>(
        &self,
        buffer_len: usize,
        read: F,
        callback: CompletionCallback<Vec<u8>>,
    ) -> OperationResult<()>
    where
        F: FnOnce() -> OperationResult<Vec<u8>> + Send + 'static,
    {
        self.submit_work(OperationWork::new(
            OperationKind::Read,
            buffer_len,
            Box::new(read),
            callback,
        ))
    }

    /// Reads up to `buffer_len` decrypted bytes and reports them through `callback`.
    pub fn read(
        &self,
        buffer_len: usize,
        callback: CompletionCallback<Vec<u8>>,
    ) -> OperationResult<()> {
        let state = Arc::clone(&self.state);
        self.submit_read(buffer_len, move || state.read_tls(buffer_len), callback)
    }

    /// Submits a write work item for this stream.
    pub fn submit_write<F>(
        &self,
        byte_len: usize,
        write: F,
        callback: CompletionCallback<usize>,
    ) -> OperationResult<()>
    where
        F: FnOnce() -> OperationResult<usize> + Send + 'static,
    {
        self.submit_work(OperationWork::new(
            OperationKind::Write,
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

    fn submit_work<T>(&self, work: OperationWork<T>) -> OperationResult<()>
    where
        T: Send + 'static,
    {
        self.state.submit(Box::new(move |state| {
            start_operation(state, work);
        }))
    }
}

pub(crate) struct StreamState {
    pool: Arc<PoolInner>,
    tls: Mutex<Option<BoxedTlsStream>>,
    queue: Mutex<StreamQueue>,
}

impl StreamState {
    fn submit(self: &Arc<Self>, job: StreamJob) -> OperationResult<()> {
        if self.pool_is_shutdown() {
            return Err(OperationError::Shutdown);
        }

        self.pool.stats.record_submitted();
        let start = {
            let mut queue = self.queue.lock().expect("stream queue poisoned");
            if queue.in_flight {
                queue.pending.push_back(job);
                self.pool.stats.record_queued(None);
                None
            } else {
                queue.in_flight = true;
                Some(job)
            }
        };

        if let Some(job) = start {
            job(Arc::clone(self));
        }
        Ok(())
    }

    fn finish(self: &Arc<Self>) {
        let next = {
            let mut queue = self.queue.lock().expect("stream queue poisoned");
            if let Some(job) = queue.pending.pop_front() {
                Some(job)
            } else {
                queue.in_flight = false;
                None
            }
        };

        if let Some(job) = next {
            job(Arc::clone(self));
        }
    }

    fn pool_is_shutdown(&self) -> bool {
        self.pool.is_shutdown()
    }

    fn read_tls(&self, buffer_len: usize) -> OperationResult<Vec<u8>> {
        let mut guard = self.tls.lock().map_err(|_| OperationError::StatePoisoned)?;
        let tls = guard.as_mut().ok_or(OperationError::Shutdown)?;
        let mut buffer = vec![0_u8; buffer_len];
        if buffer_len == 0 {
            return Ok(buffer);
        }

        let amount = crate::tls::map_io(tls.read(&mut buffer))?;
        buffer.truncate(amount);
        Ok(buffer)
    }

    fn write_tls(&self, bytes: Vec<u8>) -> OperationResult<usize> {
        let byte_len = bytes.len();
        let mut guard = self.tls.lock().map_err(|_| OperationError::StatePoisoned)?;
        let tls = guard.as_mut().ok_or(OperationError::Shutdown)?;
        crate::tls::map_io(tls.write_all(&bytes))?;
        crate::tls::map_io(tls.flush())?;
        Ok(byte_len)
    }
}

#[derive(Default)]
struct StreamQueue {
    in_flight: bool,
    pending: VecDeque<StreamJob>,
}

fn start_operation<T>(state: Arc<StreamState>, work: OperationWork<T>)
where
    T: Send + 'static,
{
    let operation_size = work.size();
    let _kind = work.kind();
    let (operation, callback) = work.into_parts();

    match state.pool.choose_placement(operation_size) {
        crate::OperationPlacement::Immediate => {
            state.pool.stats.record_immediate();
            let success = operation::complete(operation, callback);
            record_completion(&state, None, success);
            state.finish();
        }
        crate::OperationPlacement::Background { executor } => {
            state.pool.stats.record_background_routed(executor);
            state.pool.stats.record_queued(Some(executor));
            let pool = Arc::clone(&state.pool);
            let job_state = Arc::clone(&state);
            let callback = Arc::new(Mutex::new(Some(callback)));
            let send_error_callback = Arc::clone(&callback);
            let job = Box::new(move || {
                let callback = callback
                    .lock()
                    .expect("operation callback mutex poisoned")
                    .take()
                    .expect("operation callback missing");
                let success = operation::complete(operation, callback);
                record_completion(&job_state, Some(executor), success);
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
                state.finish();
            }
        }
    }
}

fn record_completion(state: &StreamState, executor: Option<usize>, success: bool) {
    if success {
        state.pool.stats.record_completed(executor);
    } else {
        state.pool.stats.record_failed(executor);
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
    fn read_and_write_entry_points_submit_typed_work() {
        let pool = TlsPool::default();
        let stream = pool.stream();
        let (sender, receiver) = mpsc::channel();
        let write_sender = sender.clone();

        stream
            .submit_read(
                4,
                || Ok(vec![1, 2, 3, 4]),
                Box::new(move |result| {
                    sender.send(result.unwrap().len()).unwrap();
                }),
            )
            .unwrap();
        stream
            .submit_write(
                4,
                || Ok(4),
                Box::new(move |result| {
                    write_sender.send(result.unwrap()).unwrap();
                }),
            )
            .unwrap();

        assert_eq!(receiver.recv().unwrap(), 4);
        assert_eq!(receiver.recv().unwrap(), 4);
        assert_eq!(pool.stats().submitted, 2);
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
}
