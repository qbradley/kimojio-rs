// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::Bytes;

use crate::{Error, LimitKind};

use super::settings::{Setting, SettingId};
use super::stream::StreamId;

pub const FRAME_HEADER_LEN: usize = 9;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameType {
    Data = 0x0,
    Headers = 0x1,
    RstStream = 0x3,
    Settings = 0x4,
    Ping = 0x6,
    Goaway = 0x7,
    WindowUpdate = 0x8,
    Continuation = 0x9,
}

impl FrameType {
    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x0 => Some(Self::Data),
            0x1 => Some(Self::Headers),
            0x3 => Some(Self::RstStream),
            0x4 => Some(Self::Settings),
            0x6 => Some(Self::Ping),
            0x7 => Some(Self::Goaway),
            0x8 => Some(Self::WindowUpdate),
            0x9 => Some(Self::Continuation),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameFlags(u8);

impl FrameFlags {
    pub const EMPTY: Self = Self(0);
    pub const END_STREAM: Self = Self(0x1);
    pub const ACK: Self = Self(0x1);
    pub const END_HEADERS: Self = Self(0x4);
    pub const PADDED: Self = Self(0x8);
    pub const PRIORITY: Self = Self(0x20);

    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FramePayload {
    Data(Bytes),
    Headers(Bytes),
    Continuation(Bytes),
    Settings(Vec<Setting>),
    RstStream {
        error_code: u32,
    },
    Ping([u8; 8]),
    Goaway {
        last_stream_id: u32,
        error_code: u32,
        debug_data: Bytes,
    },
    WindowUpdate {
        increment: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: FrameType,
    pub flags: FrameFlags,
    pub stream_id: u32,
    pub payload: FramePayload,
}

impl Frame {
    pub fn data(stream_id: StreamId, data: impl Into<Bytes>, end_stream: bool) -> Self {
        Self {
            frame_type: FrameType::Data,
            flags: if end_stream {
                FrameFlags::END_STREAM
            } else {
                FrameFlags::EMPTY
            },
            stream_id: stream_id.get(),
            payload: FramePayload::Data(data.into()),
        }
    }

    pub fn headers(stream_id: StreamId, block: impl Into<Bytes>, end_stream: bool) -> Self {
        let flags = if end_stream {
            FrameFlags::from_bits(FrameFlags::END_HEADERS.bits() | FrameFlags::END_STREAM.bits())
        } else {
            FrameFlags::END_HEADERS
        };
        Self {
            frame_type: FrameType::Headers,
            flags,
            stream_id: stream_id.get(),
            payload: FramePayload::Headers(block.into()),
        }
    }

    pub fn continuation(stream_id: StreamId, block: impl Into<Bytes>, end_headers: bool) -> Self {
        Self {
            frame_type: FrameType::Continuation,
            flags: if end_headers {
                FrameFlags::END_HEADERS
            } else {
                FrameFlags::EMPTY
            },
            stream_id: stream_id.get(),
            payload: FramePayload::Continuation(block.into()),
        }
    }

    pub fn settings(settings: Vec<Setting>) -> Self {
        Self {
            frame_type: FrameType::Settings,
            flags: FrameFlags::EMPTY,
            stream_id: 0,
            payload: FramePayload::Settings(settings),
        }
    }

    pub fn settings_ack() -> Self {
        Self {
            frame_type: FrameType::Settings,
            flags: FrameFlags::ACK,
            stream_id: 0,
            payload: FramePayload::Settings(Vec::new()),
        }
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        let payload = self.encode_payload()?;
        let len = payload.len();
        if len > 0x00ff_ffff {
            return Err(Error::size_limit(LimitKind::Frame, 0x00ff_ffff, len));
        }

        let mut bytes = Vec::with_capacity(FRAME_HEADER_LEN + len);
        bytes.push(((len >> 16) & 0xff) as u8);
        bytes.push(((len >> 8) & 0xff) as u8);
        bytes.push((len & 0xff) as u8);
        bytes.push(self.frame_type as u8);
        bytes.push(self.flags.bits());
        bytes.extend_from_slice(&(self.stream_id & 0x7fff_ffff).to_be_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }

    pub fn decode(bytes: &[u8], max_frame_size: u32) -> Result<(Self, usize), Error> {
        if bytes.len() < FRAME_HEADER_LEN {
            return Err(Error::Parse("incomplete HTTP/2 frame header"));
        }
        let len = ((bytes[0] as usize) << 16) | ((bytes[1] as usize) << 8) | bytes[2] as usize;
        if len > max_frame_size as usize {
            return Err(Error::size_limit(
                LimitKind::Frame,
                max_frame_size as usize,
                len,
            ));
        }
        if bytes.len() < FRAME_HEADER_LEN + len {
            return Err(Error::Parse("incomplete HTTP/2 frame payload"));
        }
        let frame_type =
            FrameType::from_u8(bytes[3]).ok_or(Error::Unsupported("unsupported HTTP/2 frame"))?;
        let flags = FrameFlags::from_bits(bytes[4]);
        let stream_id = u32::from_be_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]) & 0x7fff_ffff;
        let payload_bytes = &bytes[FRAME_HEADER_LEN..FRAME_HEADER_LEN + len];
        let payload = decode_payload(frame_type, flags, stream_id, payload_bytes)?;
        Ok((
            Self {
                frame_type,
                flags,
                stream_id,
                payload,
            },
            FRAME_HEADER_LEN + len,
        ))
    }

    fn encode_payload(&self) -> Result<Vec<u8>, Error> {
        match &self.payload {
            FramePayload::Data(data) => {
                if self.flags.contains(FrameFlags::PADDED) {
                    return Err(Error::Unsupported("padded DATA frames"));
                }
                Ok(data.to_vec())
            }
            FramePayload::Headers(data) => {
                if self.flags.contains(FrameFlags::PADDED) {
                    return Err(Error::Unsupported("padded HEADERS frames"));
                }
                if self.flags.contains(FrameFlags::PRIORITY) {
                    return Err(Error::Unsupported("priority HEADERS frames"));
                }
                Ok(data.to_vec())
            }
            FramePayload::Continuation(data) => Ok(data.to_vec()),
            FramePayload::Settings(settings) => {
                if self.flags.contains(FrameFlags::ACK) && !settings.is_empty() {
                    return Err(Error::Protocol("SETTINGS ack must have empty payload"));
                }
                let mut bytes = Vec::with_capacity(settings.len() * 6);
                for setting in settings {
                    bytes.extend_from_slice(&(setting.id as u16).to_be_bytes());
                    bytes.extend_from_slice(&setting.value.to_be_bytes());
                }
                Ok(bytes)
            }
            FramePayload::RstStream { error_code } => Ok(error_code.to_be_bytes().to_vec()),
            FramePayload::Ping(opaque) => Ok(opaque.to_vec()),
            FramePayload::Goaway {
                last_stream_id,
                error_code,
                debug_data,
            } => {
                let mut bytes = Vec::with_capacity(8 + debug_data.len());
                bytes.extend_from_slice(&(last_stream_id & 0x7fff_ffff).to_be_bytes());
                bytes.extend_from_slice(&error_code.to_be_bytes());
                bytes.extend_from_slice(debug_data);
                Ok(bytes)
            }
            FramePayload::WindowUpdate { increment } => {
                if *increment == 0 || *increment > 0x7fff_ffff {
                    return Err(Error::Protocol("invalid WINDOW_UPDATE increment"));
                }
                Ok((increment & 0x7fff_ffff).to_be_bytes().to_vec())
            }
        }
    }
}

fn decode_payload(
    frame_type: FrameType,
    flags: FrameFlags,
    stream_id: u32,
    bytes: &[u8],
) -> Result<FramePayload, Error> {
    match frame_type {
        FrameType::Data => {
            require_stream(stream_id)?;
            if flags.contains(FrameFlags::PADDED) {
                return Err(Error::Unsupported("padded DATA frames"));
            }
            Ok(FramePayload::Data(Bytes::copy_from_slice(bytes)))
        }
        FrameType::Headers => {
            require_stream(stream_id)?;
            if flags.contains(FrameFlags::PADDED) {
                return Err(Error::Unsupported("padded HEADERS frames"));
            }
            if flags.contains(FrameFlags::PRIORITY) {
                return Err(Error::Unsupported("priority HEADERS frames"));
            }
            Ok(FramePayload::Headers(Bytes::copy_from_slice(bytes)))
        }
        FrameType::Continuation => {
            require_stream(stream_id)?;
            Ok(FramePayload::Continuation(Bytes::copy_from_slice(bytes)))
        }
        FrameType::Settings => {
            require_connection(stream_id)?;
            if flags.contains(FrameFlags::ACK) {
                if !bytes.is_empty() {
                    return Err(Error::Protocol("SETTINGS ack must have empty payload"));
                }
                return Ok(FramePayload::Settings(Vec::new()));
            }
            if !bytes.len().is_multiple_of(6) {
                return Err(Error::Parse("SETTINGS payload length is invalid"));
            }
            let mut settings = Vec::with_capacity(bytes.len() / 6);
            for chunk in bytes.chunks_exact(6) {
                let id = u16::from_be_bytes([chunk[0], chunk[1]]);
                let Some(id) = SettingId::from_u16(id) else {
                    continue;
                };
                let value = u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]);
                settings.push(Setting::new(id, value));
            }
            Ok(FramePayload::Settings(settings))
        }
        FrameType::RstStream => {
            require_stream(stream_id)?;
            if bytes.len() != 4 {
                return Err(Error::Parse("RST_STREAM payload length is invalid"));
            }
            Ok(FramePayload::RstStream {
                error_code: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]),
            })
        }
        FrameType::Ping => {
            require_connection(stream_id)?;
            if bytes.len() != 8 {
                return Err(Error::Parse("PING payload length is invalid"));
            }
            let mut opaque = [0_u8; 8];
            opaque.copy_from_slice(bytes);
            Ok(FramePayload::Ping(opaque))
        }
        FrameType::Goaway => {
            require_connection(stream_id)?;
            if bytes.len() < 8 {
                return Err(Error::Parse("GOAWAY payload length is invalid"));
            }
            Ok(FramePayload::Goaway {
                last_stream_id: u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
                    & 0x7fff_ffff,
                error_code: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
                debug_data: Bytes::copy_from_slice(&bytes[8..]),
            })
        }
        FrameType::WindowUpdate => {
            if bytes.len() != 4 {
                return Err(Error::Parse("WINDOW_UPDATE payload length is invalid"));
            }
            let increment =
                u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) & 0x7fff_ffff;
            if increment == 0 {
                return Err(Error::Protocol("WINDOW_UPDATE increment must be non-zero"));
            }
            Ok(FramePayload::WindowUpdate { increment })
        }
    }
}

fn require_stream(stream_id: u32) -> Result<(), Error> {
    if stream_id == 0 {
        Err(Error::Protocol("frame requires non-zero stream id"))
    } else {
        Ok(())
    }
}

fn require_connection(stream_id: u32) -> Result<(), Error> {
    if stream_id == 0 {
        Ok(())
    } else {
        Err(Error::Protocol("frame requires connection stream id"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2::settings::DEFAULT_MAX_FRAME_SIZE;

    #[test]
    fn frame_round_trips_data_headers_continuation() {
        let stream_id = StreamId::new(1).unwrap();
        for frame in [
            Frame::data(stream_id, Bytes::from_static(b"hello"), true),
            Frame::headers(stream_id, Bytes::from_static(b"headers"), false),
            Frame::continuation(stream_id, Bytes::from_static(b"more"), true),
        ] {
            let encoded = frame.encode().unwrap();
            let (decoded, consumed) = Frame::decode(&encoded, DEFAULT_MAX_FRAME_SIZE).unwrap();

            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn frame_round_trips_control_frames() {
        let frames = vec![
            Frame::settings(vec![Setting::new(SettingId::MaxFrameSize, 32 * 1024)]),
            Frame::settings_ack(),
            Frame {
                frame_type: FrameType::Ping,
                flags: FrameFlags::ACK,
                stream_id: 0,
                payload: FramePayload::Ping(*b"12345678"),
            },
            Frame {
                frame_type: FrameType::Goaway,
                flags: FrameFlags::EMPTY,
                stream_id: 0,
                payload: FramePayload::Goaway {
                    last_stream_id: 1,
                    error_code: 0,
                    debug_data: Bytes::from_static(b"bye"),
                },
            },
            Frame {
                frame_type: FrameType::WindowUpdate,
                flags: FrameFlags::EMPTY,
                stream_id: 0,
                payload: FramePayload::WindowUpdate { increment: 1024 },
            },
            Frame {
                frame_type: FrameType::RstStream,
                flags: FrameFlags::EMPTY,
                stream_id: 1,
                payload: FramePayload::RstStream { error_code: 8 },
            },
        ];

        for frame in frames {
            let encoded = frame.encode().unwrap();
            let (decoded, consumed) = Frame::decode(&encoded, DEFAULT_MAX_FRAME_SIZE).unwrap();

            assert_eq!(consumed, encoded.len());
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn frame_decode_rejects_malformed_frames_and_size_limits() {
        assert!(Frame::decode(&[0, 0, 1, 0], DEFAULT_MAX_FRAME_SIZE).is_err());

        let frame = Frame::data(
            StreamId::new(1).unwrap(),
            Bytes::from_static(b"too-big"),
            false,
        )
        .encode()
        .unwrap();
        assert!(Frame::decode(&frame, 1).is_err());

        let bad_ping = [
            0,
            0,
            7,
            FrameType::Ping as u8,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        assert!(Frame::decode(&bad_ping, DEFAULT_MAX_FRAME_SIZE).is_err());
    }

    #[test]
    fn frame_decode_rejects_unsupported_padded_and_priority_flags() {
        let padded_data = [
            0,
            0,
            1,
            FrameType::Data as u8,
            FrameFlags::PADDED.bits(),
            0,
            0,
            0,
            1,
            0,
        ];
        let priority_headers = [
            0,
            0,
            5,
            FrameType::Headers as u8,
            FrameFlags::PRIORITY.bits(),
            0,
            0,
            0,
            1,
            0,
            0,
            0,
            0,
            0,
        ];

        assert!(Frame::decode(&padded_data, DEFAULT_MAX_FRAME_SIZE).is_err());
        assert!(Frame::decode(&priority_headers, DEFAULT_MAX_FRAME_SIZE).is_err());
    }
}
