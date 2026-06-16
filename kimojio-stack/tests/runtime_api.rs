// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::panic::{self, AssertUnwindSafe};

use kimojio_stack::{
    ReadOutput, Runtime, RuntimeIoError, RuntimeReadResult, RuntimeWaitable, RuntimeWriteResult,
    SocketIoRuntime, StackfulWaitContext, StackfulWaiterHandle, WriteOutput,
};
use rustix::pipe::pipe;

struct FakeResult<T> {
    output: Option<T>,
}

impl<T> FakeResult<T> {
    fn new(output: T) -> Self {
        Self {
            output: Some(output),
        }
    }
}

impl<T> RuntimeWaitable for FakeResult<T> {
    fn is_ready(&self) -> bool {
        self.output.is_some()
    }

    fn add_stackful_waiter(&self, _waiter: StackfulWaiterHandle) -> bool {
        false
    }
}

impl RuntimeReadResult<Vec<u8>> for FakeResult<ReadOutput<Vec<u8>>> {
    type Output = ReadOutput<Vec<u8>>;

    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
        self.output.take().map(Ok)
    }

    fn cancel(&mut self) -> Result<(), RuntimeIoError> {
        self.output = None;
        Ok(())
    }
}

impl RuntimeWriteResult<Vec<u8>> for FakeResult<WriteOutput<Vec<u8>>> {
    type Output = WriteOutput<Vec<u8>>;

    fn try_get(&mut self) -> Option<Result<Self::Output, RuntimeIoError>> {
        self.output.take().map(Ok)
    }

    fn cancel(&mut self) -> Result<(), RuntimeIoError> {
        self.output = None;
        Ok(())
    }
}

fn assert_runtime_read_result<R>()
where
    R: RuntimeReadResult<Vec<u8>, Output = ReadOutput<Vec<u8>>>,
{
}

fn assert_runtime_write_result<W>()
where
    W: RuntimeWriteResult<Vec<u8>, Output = WriteOutput<Vec<u8>>>,
{
}

#[test]
fn runtime_result_traits_do_not_require_stack_core_waitable() {
    assert_runtime_read_result::<FakeResult<ReadOutput<Vec<u8>>>>();
    assert_runtime_write_result::<FakeResult<WriteOutput<Vec<u8>>>>();

    let mut read = FakeResult::new(ReadOutput {
        bytes: 1,
        buffer: vec![7],
    });
    assert!(RuntimeWaitable::is_ready(&read));
    let output = RuntimeReadResult::try_get(&mut read)
        .expect("fake read should be ready")
        .unwrap();
    assert_eq!(output.bytes, 1);
    assert_eq!(output.buffer, vec![7]);
}

#[test]
fn direct_result_cancel_detaches_stack_core_handle() {
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        let (read_fd, _write_fd) = pipe().unwrap();
        let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
        let mut read = cx.read_async(&read_fd, vec![0_u8; 1]);

        RuntimeReadResult::cancel(&mut read).unwrap();

        assert!(RuntimeWaitable::is_ready(&read));
        assert!(
            panic::catch_unwind(AssertUnwindSafe(|| RuntimeReadResult::try_get(&mut read)))
                .is_err()
        );
    });
}

#[test]
fn context_cancel_keeps_stack_core_result_drainable_before_close() {
    let mut runtime = Runtime::new();

    runtime.block_on(|cx| {
        let (read_fd, write_fd) = pipe().unwrap();
        let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
        let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();
        let mut read = cx.read_async(&read_fd, vec![0_u8; 1]);

        cx.cancel_read(&mut read).unwrap();
        cx.wait_stackful(&read).unwrap();
        assert!(
            RuntimeReadResult::try_get(&mut read)
                .expect("canceled read should drain")
                .is_err()
        );

        SocketIoRuntime::close(cx, read_fd).unwrap();
        SocketIoRuntime::close(cx, write_fd).unwrap();
    });
}
