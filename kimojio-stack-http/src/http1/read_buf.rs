// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack::RuntimeSocket;

use crate::{Error, HttpRuntime, RuntimeStackTransport};

pub(super) const DEFAULT_READ_CHUNK_SIZE: usize = 16 * 1024;

pub(super) fn read_more<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    amount: usize,
) -> Result<usize, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    let start = read_buf.len();
    let amount = amount.max(1);
    read_buf.resize(start + amount, 0);
    match transport.read(cx, &mut read_buf[start..]) {
        Ok(read) => {
            read_buf.truncate(start + read);
            Ok(read)
        }
        Err(error) => {
            read_buf.truncate(start);
            Err(error)
        }
    }
}

pub(super) fn read_into_empty<R, S>(
    cx: &R,
    transport: &mut RuntimeStackTransport<S>,
    read_buf: &mut Vec<u8>,
    amount: usize,
) -> Result<usize, Error>
where
    R: HttpRuntime<S>,
    S: RuntimeSocket,
{
    debug_assert!(read_buf.is_empty());
    let amount = amount.max(1);
    read_buf.resize(amount, 0);
    match transport.read(cx, read_buf) {
        Ok(read) => {
            read_buf.truncate(read);
            Ok(read)
        }
        Err(error) => {
            read_buf.clear();
            Err(error)
        }
    }
}
