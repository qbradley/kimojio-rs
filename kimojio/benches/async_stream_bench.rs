// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use criterion::{Criterion, criterion_group, criterion_main};

use kimojio::pipe::bipipe;
use kimojio::{AsyncStreamRead, AsyncStreamWrite, BufferSlice, OwnedFdStream};

pub fn benchmark(c: &mut Criterion) {
    c.bench_function("async_stream", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (client, server) = bipipe();
                let mut stream1 = OwnedFdStream::new(client);
                let mut stream2 = OwnedFdStream::new(server);

                let start = Instant::now();
                for iter in 0..iters {
                    stream1.write(iter.to_le_bytes(), None).await.0.unwrap();
                    stream2.read([0; 8], None).await.0.unwrap();
                }
                dur_copy.set(start.elapsed());

                stream1.shutdown().await.unwrap();
                stream2.shutdown().await.unwrap();
            });
            duration.get()
        });
    });

    c.bench_function("manual_channel_stream", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (client, server) = bipipe();
                let mut stream1 = OwnedFdStream::new(client);
                let mut stream2 = OwnedFdStream::new(server);

                let mut buffers = vec![vec![0; 4], vec![0; 4], vec![0; 8]];
                let mut response = [0; 128];
                let start = Instant::now();
                for iter in 0..iters {
                    let id = 1u32;
                    let length = 12u32;
                    buffers[0].copy_from_slice(&length.to_le_bytes());
                    buffers[1].copy_from_slice(&id.to_le_bytes());
                    buffers[2].copy_from_slice(&iter.to_le_bytes());
                    let (result, returned) = stream1.writev(buffers, None).await;
                    buffers = returned;
                    result.unwrap();

                    let (result, returned) =
                        stream2.read(BufferSlice::new(response, 0..4), None).await;
                    response = returned.into_inner();
                    result.unwrap();
                    let length = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    let (result, returned) = stream2
                        .read(BufferSlice::new(response, 0..length), None)
                        .await;
                    response = returned.into_inner();
                    result.unwrap();
                    let id = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    assert_eq!(id, 1);
                    assert_eq!(&iter.to_le_bytes(), &response[4..length]);
                }
                dur_copy.set(start.elapsed());

                stream1.shutdown().await.unwrap();
                stream2.shutdown().await.unwrap();
            });
            duration.get()
        });
    });

    c.bench_function("manual_read_all_macro_channel_stream", |b| {
        b.iter_custom(|iters| {
            let duration = Rc::new(Cell::new(Duration::new(1, 1)));
            let dur_copy = duration.clone();

            kimojio::run(0, async move {
                let (client, server) = bipipe();
                let mut stream1 = OwnedFdStream::new(client);
                let mut stream2 = OwnedFdStream::new(server);

                let start = Instant::now();
                for iter in 0..iters {
                    let mut buffer = [0; 128];
                    // let mut buffer = BufferView::new(&mut buffer);
                    // buffer.set(&iter.to_le_bytes());
                    let id = 1u32;
                    // buffer.prefix(&id.to_le_bytes());
                    let length: i32 = 12;
                    // buffer.prefix(&length.to_le_bytes());
                    buffer[0..4].copy_from_slice(&length.to_le_bytes());
                    buffer[4..8].copy_from_slice(&id.to_le_bytes());
                    buffer[8..16].copy_from_slice(&iter.to_le_bytes());

                    stream1
                        .write(BufferSlice::new(buffer, 0..16), None)
                        .await
                        .0
                        .unwrap();

                    let mut response = [0; 128];
                    let (result, returned) =
                        stream2.read(BufferSlice::new(response, 0..4), None).await;
                    response = returned.into_inner();
                    result.unwrap();
                    let length = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    let (result, returned) = stream2
                        .read(BufferSlice::new(response, 0..length), None)
                        .await;
                    response = returned.into_inner();
                    result.unwrap();
                    let id = u32::from_le_bytes(response[0..4].try_into().unwrap()) as usize;
                    assert_eq!(id, 1);
                    let response = &response[4..length];
                    assert_eq!(&iter.to_le_bytes(), response);
                }
                dur_copy.set(start.elapsed());

                stream1.shutdown().await.unwrap();
                stream2.shutdown().await.unwrap();
            });
            duration.get()
        });
    });
}

criterion_group!(
    name = benches;
    config = Criterion::default().warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(4));
    targets=benchmark
);
criterion_main!(benches);
