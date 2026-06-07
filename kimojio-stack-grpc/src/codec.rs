// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use prost::Message;

use crate::Error;

const HEADER_LEN: usize = 5;
const UNCOMPRESSED_FLAG: u8 = 0;

/// Encodes a prost-compatible message into one unary gRPC data frame.
pub fn encode_message<M>(message: &M, max_len: usize) -> Result<Bytes, Error>
where
    M: Message,
{
    let encoded_len = message.encoded_len();
    if encoded_len > max_len {
        return Err(Error::SizeLimit {
            limit: max_len,
            actual: encoded_len,
        });
    }
    if encoded_len > u32::MAX as usize {
        return Err(Error::SizeLimit {
            limit: u32::MAX as usize,
            actual: encoded_len,
        });
    }

    let mut bytes = BytesMut::with_capacity(HEADER_LEN + encoded_len);
    bytes.put_u8(UNCOMPRESSED_FLAG);
    bytes.put_u32(encoded_len as u32);
    message.encode(&mut bytes)?;
    Ok(bytes.freeze())
}

/// Decodes one unary gRPC data frame into a prost-compatible message.
pub fn decode_message<M>(frame: &[u8], max_len: usize) -> Result<M, Error>
where
    M: Message + Default,
{
    if frame.len() < HEADER_LEN {
        return Err(Error::Protocol("gRPC frame header is incomplete"));
    }

    let compressed_flag = frame[0];
    if compressed_flag != UNCOMPRESSED_FLAG {
        return Err(Error::UnsupportedCompression(compressed_flag));
    }

    let mut len_bytes = &frame[1..HEADER_LEN];
    let message_len = len_bytes.get_u32() as usize;
    if message_len > max_len {
        return Err(Error::SizeLimit {
            limit: max_len,
            actual: message_len,
        });
    }
    let actual_len = frame.len() - HEADER_LEN;
    if actual_len != message_len {
        return Err(Error::Protocol("gRPC frame length does not match payload"));
    }

    M::decode(&frame[HEADER_LEN..]).map_err(Error::from)
}

pub fn encode_bytes(bytes: &[u8], max_len: usize) -> Result<Bytes, Error> {
    if bytes.len() > max_len {
        return Err(Error::SizeLimit {
            limit: max_len,
            actual: bytes.len(),
        });
    }
    if bytes.len() > u32::MAX as usize {
        return Err(Error::SizeLimit {
            limit: u32::MAX as usize,
            actual: bytes.len(),
        });
    }
    let mut frame = BytesMut::with_capacity(HEADER_LEN + bytes.len());
    frame.put_u8(UNCOMPRESSED_FLAG);
    frame.put_u32(bytes.len() as u32);
    frame.extend_from_slice(bytes);
    Ok(frame.freeze())
}

pub fn decode_bytes(frame: &[u8], max_len: usize) -> Result<Bytes, Error> {
    if frame.len() < HEADER_LEN {
        return Err(Error::Protocol("gRPC frame header is incomplete"));
    }
    let compressed_flag = frame[0];
    if compressed_flag != UNCOMPRESSED_FLAG {
        return Err(Error::UnsupportedCompression(compressed_flag));
    }
    let mut len_bytes = &frame[1..HEADER_LEN];
    let message_len = len_bytes.get_u32() as usize;
    if message_len > max_len {
        return Err(Error::SizeLimit {
            limit: max_len,
            actual: message_len,
        });
    }
    let actual_len = frame.len() - HEADER_LEN;
    if actual_len != message_len {
        return Err(Error::Protocol("gRPC frame length does not match payload"));
    }
    Ok(Bytes::copy_from_slice(&frame[HEADER_LEN..]))
}
