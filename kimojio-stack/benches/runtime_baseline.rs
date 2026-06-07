// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio_stack::channel::{bounded, cross_thread, unbounded};
use kimojio_stack::{Mutex, Runtime, RuntimeContext, Semaphore};
use rustix::pipe::pipe;
use tokio::runtime::Builder as TokioRuntimeBuilder;

fn run_stackful(f: impl FnOnce(&RuntimeContext<'_>) -> Duration) -> Duration {
    let mut runtime = Runtime::new();
    runtime.block_on(f)
}

fn run_stackful_registered(
    registered_file_slots: u32,
    registered_buffer_slots: u16,
    f: impl FnOnce(&RuntimeContext<'_>) -> Duration,
) -> Duration {
    let mut runtime =
        Runtime::with_registered_resources(registered_file_slots, registered_buffer_slots);
    runtime.block_on(f)
}

fn bench_runtime(c: &mut Criterion) {
    c.bench_function("runtime/block_on_empty", |b| {
        b.iter(|| {
            let mut runtime = Runtime::new();
            runtime.block_on(|_| black_box(()));
        });
    });
}

fn bench_scheduler(c: &mut Criterion) {
    c.bench_function("scheduler/yield_now_single_task", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                cx.scope(|scope| {
                    let worker = scope.spawn(move |cx| {
                        let start = Instant::now();
                        for _ in 0..iters {
                            cx.yield_now();
                        }
                        start.elapsed()
                    });

                    worker.join(cx).unwrap()
                })
            })
        });
    });

    c.bench_function("scheduler/yield_now_two_tasks", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                cx.scope(|scope| {
                    let first_iters = iters / 2;
                    let second_iters = iters - first_iters;
                    let first = scope.spawn(move |cx| {
                        for _ in 0..first_iters {
                            cx.yield_now();
                        }
                    });
                    let second = scope.spawn(move |cx| {
                        for _ in 0..second_iters {
                            cx.yield_now();
                        }
                    });

                    let start = Instant::now();
                    first.join(cx).unwrap();
                    second.join(cx).unwrap();
                    start.elapsed()
                })
            })
        });
    });

    c.bench_function("scheduler/spawn_join", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                cx.scope(|scope| {
                    let start = Instant::now();
                    for i in 0..iters {
                        let worker = scope.spawn(move |_| i.wrapping_add(1));
                        black_box(worker.join(cx).unwrap());
                    }
                    start.elapsed()
                })
            })
        });
    });
}

fn bench_sync(c: &mut Criterion) {
    c.bench_function("sync/mutex_uncontended_lock", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let mutex = Mutex::new(0_u64);

                let start = Instant::now();
                for _ in 0..iters {
                    let mut guard = mutex.lock(cx);
                    *guard = black_box(guard.wrapping_add(1));
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("sync/semaphore_uncontended_acquire", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let semaphore = Semaphore::new(1);

                let start = Instant::now();
                for _ in 0..iters {
                    let permit = semaphore.acquire(cx);
                    black_box(&permit);
                    drop(permit);
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("sync/unbounded_channel_send_recv", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let (tx, rx) = unbounded::channel();

                let start = Instant::now();
                for i in 0..iters {
                    tx.send(i).unwrap();
                    black_box(rx.recv(cx).unwrap());
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("sync/bounded_channel_send_recv", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let (tx, rx) = bounded::channel(1);

                let start = Instant::now();
                for i in 0..iters {
                    tx.send(cx, i).unwrap();
                    black_box(rx.recv(cx).unwrap());
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("sync/bounded_channel_ping_pong", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let (ping_tx, ping_rx) = bounded::channel(1);
                let (pong_tx, pong_rx) = bounded::channel(1);

                cx.scope(|scope| {
                    let responder = scope.spawn(move |cx| {
                        for _ in 0..iters {
                            let value = ping_rx.recv(cx).unwrap();
                            pong_tx.send(cx, value).unwrap();
                        }
                    });

                    let start = Instant::now();
                    for i in 0..iters {
                        ping_tx.send(cx, i).unwrap();
                        black_box(pong_rx.recv(cx).unwrap());
                    }
                    let elapsed = start.elapsed();

                    responder.join(cx).unwrap();
                    elapsed
                })
            })
        });
    });
}

fn bench_cross_thread_channel(c: &mut Criterion) {
    c.bench_function("cross_thread/thread_ready_send_recv", |b| {
        b.iter_custom(|iters| {
            let (tx, rx) = cross_thread::thread(1);
            tx.try_send(0).unwrap();
            black_box(rx.try_recv().unwrap());

            let start = Instant::now();
            for i in 0..iters {
                tx.try_send(i).unwrap();
                black_box(rx.try_recv().unwrap());
            }
            start.elapsed()
        });
    });

    c.bench_function("cross_thread/thread_ping_pong", |b| {
        b.iter_custom(|iters| {
            let (ping_tx, ping_rx) = cross_thread::thread(1);
            let (pong_tx, pong_rx) = cross_thread::thread(1);
            let (ready_tx, ready_rx) = mpsc::channel();

            let responder = thread::spawn(move || {
                ready_tx.send(()).unwrap();
                for _ in 0..iters {
                    let value = ping_rx.recv_blocking().unwrap();
                    pong_tx.send_blocking(value).unwrap();
                }
            });

            ready_rx.recv().unwrap();
            let start = Instant::now();
            for i in 0..iters {
                ping_tx.send_blocking(i).unwrap();
                black_box(pong_rx.recv_blocking().unwrap());
            }
            let elapsed = start.elapsed();

            responder.join().unwrap();
            elapsed
        });
    });

    c.bench_function("cross_thread/stackful_cross_runtime_ping_pong", |b| {
        b.iter_custom(|iters| {
            let (ping_tx, ping_rx) = cross_thread::stackful(1);
            let (pong_tx, pong_rx) = cross_thread::stackful(1);
            let (ready_tx, ready_rx) = mpsc::channel();

            let responder = thread::spawn(move || {
                let mut runtime = Runtime::new();
                runtime.block_on(|cx| {
                    cx.scope(|scope| {
                        let responder = scope.spawn(move |cx| {
                            ready_tx.send(()).unwrap();
                            for _ in 0..iters {
                                let value = ping_rx.recv(cx).unwrap();
                                pong_tx.send(cx, value).unwrap();
                            }
                        });

                        responder.join(cx).unwrap();
                    });
                });
            });

            ready_rx.recv().unwrap();
            let elapsed = run_stackful(|cx| {
                cx.scope(|scope| {
                    let sender = scope.spawn(move |cx| {
                        let start = Instant::now();
                        for i in 0..iters {
                            ping_tx.send(cx, i).unwrap();
                            black_box(pong_rx.recv(cx).unwrap());
                        }
                        start.elapsed()
                    });

                    sender.join(cx).unwrap()
                })
            });

            responder.join().unwrap();
            elapsed
        });
    });

    c.bench_function("cross_thread/tokio_ready_send_recv", |b| {
        b.iter_custom(|iters| {
            let runtime = TokioRuntimeBuilder::new_current_thread().build().unwrap();
            runtime.block_on(async move {
                let (tx, rx) = cross_thread::tokio(1);
                tx.send(0).await.unwrap();
                black_box(rx.recv().await.unwrap());

                let start = Instant::now();
                for i in 0..iters {
                    tx.send(i).await.unwrap();
                    black_box(rx.recv().await.unwrap());
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("cross_thread/tokio_ping_pong", |b| {
        b.iter_custom(|iters| {
            let runtime = TokioRuntimeBuilder::new_current_thread().build().unwrap();
            runtime.block_on(async move {
                let (ping_tx, ping_rx) = cross_thread::tokio(1);
                let (pong_tx, pong_rx) = cross_thread::tokio(1);

                let responder = tokio::spawn(async move {
                    for _ in 0..iters {
                        let value = ping_rx.recv().await.unwrap();
                        pong_tx.send(value).await.unwrap();
                    }
                });

                let start = Instant::now();
                for i in 0..iters {
                    ping_tx.send(i).await.unwrap();
                    black_box(pong_rx.recv().await.unwrap());
                }
                let elapsed = start.elapsed();

                responder.await.unwrap();
                elapsed
            })
        });
    });
}

fn bench_io(c: &mut Criterion) {
    c.bench_function("io_uring/nop_dispatch", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                cx.nop().unwrap();

                let start = Instant::now();
                for _ in 0..iters {
                    cx.nop().unwrap();
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("io_uring/pipe_write_read", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let mut read_buffer = [0_u8; 1];
                cx.write(&write_fd, &[1]).unwrap();
                cx.read(&read_fd, &mut read_buffer).unwrap();

                let start = Instant::now();
                for i in 0..iters {
                    let byte = [i as u8];
                    cx.write(&write_fd, &byte).unwrap();
                    cx.read(&read_fd, &mut read_buffer).unwrap();
                    black_box(read_buffer[0]);
                }
                let elapsed = start.elapsed();

                cx.close(read_fd).unwrap();
                cx.close(write_fd).unwrap();
                elapsed
            })
        });
    });

    c.bench_function("io_uring/pipe_write_read_registered_fd", |b| {
        b.iter_custom(|iters| {
            run_stackful_registered(2, 0, |cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.register_fd(read_fd).unwrap();
                let write_fd = cx.register_fd(write_fd).unwrap();
                let mut read_buffer = [0_u8; 1];

                cx.write_registered_fd(&write_fd, &[1]).unwrap();
                cx.read_registered_fd(&read_fd, &mut read_buffer).unwrap();

                let start = Instant::now();
                for i in 0..iters {
                    let byte = [i as u8];
                    cx.write_registered_fd(&write_fd, &byte).unwrap();
                    cx.read_registered_fd(&read_fd, &mut read_buffer).unwrap();
                    black_box(read_buffer[0]);
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("io_uring/pipe_write_read_registered_buffer", |b| {
        b.iter_custom(|iters| {
            run_stackful_registered(0, 2, |cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let mut write_buffer = cx.register_buffer(vec![0_u8]).unwrap();
                let mut read_buffer = cx.register_buffer(vec![0_u8]).unwrap();

                cx.write_registered_buffer(&write_fd, &write_buffer)
                    .unwrap();
                cx.read_registered_buffer(&read_fd, &mut read_buffer)
                    .unwrap();

                let start = Instant::now();
                for i in 0..iters {
                    write_buffer.buffer_mut()[0] = i as u8;
                    cx.write_registered_buffer(&write_fd, &write_buffer)
                        .unwrap();
                    cx.read_registered_buffer(&read_fd, &mut read_buffer)
                        .unwrap();
                    black_box(read_buffer.buffer()[0]);
                }
                let elapsed = start.elapsed();

                cx.close(read_fd).unwrap();
                cx.close(write_fd).unwrap();
                elapsed
            })
        });
    });

    c.bench_function("io_uring/pipe_write_read_registered_fd_buffer", |b| {
        b.iter_custom(|iters| {
            run_stackful_registered(2, 2, |cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.register_fd(read_fd).unwrap();
                let write_fd = cx.register_fd(write_fd).unwrap();
                let mut write_buffer = cx.register_buffer(vec![0_u8]).unwrap();
                let mut read_buffer = cx.register_buffer(vec![0_u8]).unwrap();

                cx.write_registered_fixed(&write_fd, &write_buffer).unwrap();
                cx.read_registered_fixed(&read_fd, &mut read_buffer)
                    .unwrap();

                let start = Instant::now();
                for i in 0..iters {
                    write_buffer.buffer_mut()[0] = i as u8;
                    cx.write_registered_fixed(&write_fd, &write_buffer).unwrap();
                    cx.read_registered_fixed(&read_fd, &mut read_buffer)
                        .unwrap();
                    black_box(read_buffer.buffer()[0]);
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("io_uring/async_pipe_write_read", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let mut write_buffer = vec![0_u8];
                let mut read_buffer = vec![0_u8];

                let write = cx.write_async(&write_fd, write_buffer).get(cx).unwrap();
                write_buffer = write.buffer;
                let read = cx.read_async(&read_fd, read_buffer).get(cx).unwrap();
                read_buffer = read.buffer;

                let start = Instant::now();
                for i in 0..iters {
                    write_buffer[0] = i as u8;
                    let write = cx.write_async(&write_fd, write_buffer).get(cx).unwrap();
                    write_buffer = write.buffer;
                    let read = cx.read_async(&read_fd, read_buffer).get(cx).unwrap();
                    read_buffer = read.buffer;
                    black_box(read_buffer[0]);
                }
                let elapsed = start.elapsed();

                cx.close(read_fd).unwrap();
                cx.close(write_fd).unwrap();
                elapsed
            })
        });
    });

    c.bench_function("io_uring/async_pipe_write_read_registered_fd_buffer", |b| {
        b.iter_custom(|iters| {
            run_stackful_registered(2, 2, |cx| {
                let (read_fd, write_fd) = pipe().unwrap();
                let read_fd = cx.register_fd(read_fd).unwrap();
                let write_fd = cx.register_fd(write_fd).unwrap();
                let mut write_buffer = cx.register_buffer(vec![0_u8]).unwrap();
                let mut read_buffer = cx.register_buffer(vec![0_u8]).unwrap();

                let write = cx
                    .write_registered_fixed_async(&write_fd, write_buffer)
                    .get(cx)
                    .unwrap();
                write_buffer = write.buffer;
                let read = cx
                    .read_registered_fixed_async(&read_fd, read_buffer)
                    .get(cx)
                    .unwrap();
                read_buffer = read.buffer;

                let start = Instant::now();
                for i in 0..iters {
                    write_buffer.buffer_mut()[0] = i as u8;
                    let write = cx
                        .write_registered_fixed_async(&write_fd, write_buffer)
                        .get(cx)
                        .unwrap();
                    write_buffer = write.buffer;
                    let read = cx
                        .read_registered_fixed_async(&read_fd, read_buffer)
                        .get(cx)
                        .unwrap();
                    read_buffer = read.buffer;
                    black_box(read_buffer.buffer()[0]);
                }
                start.elapsed()
            })
        });
    });

    c.bench_function("io_uring/pipe_ping_pong_two_tasks", |b| {
        b.iter_custom(|iters| {
            run_stackful(|cx| {
                let (ping_read, ping_write) = pipe().unwrap();
                let (pong_read, pong_write) = pipe().unwrap();

                cx.scope(|scope| {
                    let responder = scope.spawn(move |cx| {
                        let mut received = [0_u8; 1];
                        for _ in 0..iters {
                            cx.read(&ping_read, &mut received).unwrap();
                            cx.write(&pong_write, &received).unwrap();
                        }
                        cx.close(ping_read).unwrap();
                        cx.close(pong_write).unwrap();
                    });

                    let mut received = [0_u8; 1];
                    let start = Instant::now();
                    for i in 0..iters {
                        let byte = [i as u8];
                        cx.write(&ping_write, &byte).unwrap();
                        cx.read(&pong_read, &mut received).unwrap();
                        black_box(received[0]);
                    }
                    let elapsed = start.elapsed();

                    cx.close(ping_write).unwrap();
                    cx.close(pong_read).unwrap();
                    responder.join(cx).unwrap();
                    elapsed
                })
            })
        });
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(4));
    targets = bench_runtime, bench_scheduler, bench_sync, bench_cross_thread_channel, bench_io
);
criterion_main!(benches);
