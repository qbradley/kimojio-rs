// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack::{
    ReadOutput, RuntimeIoError, RuntimeReadResult, RuntimeWaitable, RuntimeWriteResult,
    StackfulWaiter, WriteOutput,
};

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

    fn add_stackful_waiter(&self, _waiter: Box<dyn StackfulWaiter>) -> bool {
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
