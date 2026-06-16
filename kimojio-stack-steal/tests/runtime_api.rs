// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::num::NonZeroUsize;
use std::time::Duration;

use kimojio_stack::{
    IoRuntime, Runtime as StackCoreRuntime, RuntimeCapabilities, RuntimeCapability,
    RuntimeReadResult, RuntimeWaitable, RuntimeWriteResult, SocketIoRuntime,
    StackRuntime as StackRuntimeTrait, StackRuntimeContext, StackfulWaitContext,
};
use kimojio_stack_steal::{Runtime as StealRuntime, RuntimeConfig, StealPolicy};
use rustix::pipe::pipe;

fn runtime_agnostic_component<R>(runtime: &mut R, explicit_ring_io_supported: bool)
where
    R: StackRuntimeTrait,
    for<'cx> R::Context<'cx>: SocketIoRuntime,
{
    runtime.block_on(|cx| {
        assert!(cx.supports(RuntimeCapability::StackfulWait));
        cx.yield_now();
        let borrowed = 41;
        assert_eq!(
            cx.spawn_scoped(|child| {
                child.yield_now();
                child.sleep_for(Duration::from_millis(0)).unwrap();
                borrowed + 1
            }),
            42
        );
        assert_eq!(
            cx.spawn_stealable_scoped(|child| {
                child.yield_now();
                7
            }),
            7
        );
        let ring_capability = cx.require_explicit_ring_io();
        assert_eq!(ring_capability.is_ok(), explicit_ring_io_supported);
        if !explicit_ring_io_supported {
            let error = ring_capability.unwrap_err();
            assert_eq!(error.capability(), "explicit-ring-io");
        }

        cx.require_socket_io().unwrap();
        if !explicit_ring_io_supported {
            cx.sleep_for(Duration::from_millis(0)).unwrap();

            let (read_fd, write_fd) = pipe().unwrap();
            let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
            let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();
            assert_eq!(cx.write(&write_fd, b"io").unwrap(), 2);
            let mut buffer = [0_u8; 2];
            assert_eq!(cx.read(&read_fd, &mut buffer).unwrap(), 2);
            assert_eq!(&buffer, b"io");

            let mut read = cx.read_async(&read_fd, vec![0_u8; 2]).unwrap();
            let mut write = cx.write_async(&write_fd, b"ok".to_vec()).unwrap();
            let mut read_done = false;
            let mut write_done = false;
            for _ in 0..128 {
                if !write_done && let Some(result) = RuntimeWriteResult::try_get(&mut write) {
                    result.unwrap();
                    write_done = true;
                }
                if !read_done && let Some(result) = RuntimeReadResult::try_get(&mut read) {
                    result.unwrap();
                    read_done = true;
                }
                if read_done && write_done {
                    break;
                }
                cx.sleep_for(Duration::from_millis(0)).unwrap();
            }
            assert!(read_done);
            assert!(write_done);
            SocketIoRuntime::close(cx, read_fd).unwrap();
            SocketIoRuntime::close(cx, write_fd).unwrap();
        }
    });
}

#[test]
fn runtime_agnostic_component_runs_on_stack_runtime() {
    let mut runtime = StackCoreRuntime::new();

    runtime_agnostic_component(&mut runtime, false);
}

#[test]
fn runtime_agnostic_component_runs_on_stealing_runtime() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime_agnostic_component(&mut runtime, true);
}

#[test]
fn direct_result_cancel_detaches_stealing_runtime_handle() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let (read_fd, _write_fd) = pipe().unwrap();
                let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                let mut read = cx.read_async(&read_fd, vec![0_u8; 1]).unwrap();

                RuntimeReadResult::cancel(&mut read).unwrap();

                assert!(RuntimeWaitable::is_ready(&read));
                assert!(RuntimeReadResult::try_get(&mut read).is_none());
            });

            worker.join(cx);
        });
    });
}

#[test]
fn context_cancel_keeps_stealing_runtime_result_drainable_before_close() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();
                let mut read = cx.read_async(&read_fd, vec![0_u8; 1]).unwrap();

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

            worker.join(cx);
        });
    });
}

#[test]
fn stealing_runtime_waits_on_runtime_neutral_socket_results() {
    let mut runtime = StealRuntime::with_config(RuntimeConfig {
        workers: NonZeroUsize::new(2).unwrap(),
        steal_policy: StealPolicy::steal_one(),
        ..RuntimeConfig::default()
    });

    runtime.block_on(|cx| {
        cx.scope(|scope| {
            let worker = scope.spawn_stealable(|cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.socket_from_owned_fd(read_fd).unwrap();
                let write_fd = cx.socket_from_owned_fd(write_fd).unwrap();

                let mut read = cx.read_async(&read_fd, vec![0_u8; 2]).unwrap();
                let mut write = cx.write_async(&write_fd, b"ok".to_vec()).unwrap();
                let waitables: [&dyn RuntimeWaitable; 2] = [&read, &write];

                cx.wait_all_stackful(&waitables).unwrap();

                let written = RuntimeWriteResult::try_get(&mut write)
                    .expect("write should be ready")
                    .unwrap();
                assert_eq!(written.bytes, 2);
                assert_eq!(written.buffer, b"ok");

                let read = RuntimeReadResult::try_get(&mut read)
                    .expect("read should be ready")
                    .unwrap();
                assert_eq!(read.bytes, 2);
                assert_eq!(&read.buffer[..read.bytes], b"ok");

                SocketIoRuntime::close(cx, read_fd).unwrap();
                SocketIoRuntime::close(cx, write_fd).unwrap();
                42
            });

            assert_eq!(worker.join(cx), 42);
        });
    });
}
