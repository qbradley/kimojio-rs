// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::hint::black_box;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};
use kimojio_stack::channel::{bounded, unbounded};
use kimojio_stack::{Mutex, Runtime, RuntimeContext, Semaphore};
use rustix::pipe::pipe;

fn run_stackful(f: impl FnOnce(&RuntimeContext<'_>) -> Duration) -> Duration {
    let mut runtime = Runtime::new();
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
    targets = bench_runtime, bench_scheduler, bench_sync, bench_io
);
criterion_main!(benches);
