// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub mod connection;
pub mod frame;
mod hpack;
pub mod settings;
pub mod stream;

pub use connection::ConnectionState;
pub use frame::{Frame, FrameFlags, FramePayload, FrameType};
pub use hpack::Header;
pub use settings::{Setting, SettingId, Settings};
pub use stream::{FlowControlWindow, Stream, StreamId, StreamState};

/// HTTP/2 client connection preface.
pub const CLIENT_PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HTTP/2 connection settings used by the stackful connection core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct H2Config {
    pub initial_stream_window: u32,
    pub initial_connection_window: u32,
    pub max_frame_size: u32,
    pub max_concurrent_streams: u32,
}

impl Default for H2Config {
    fn default() -> Self {
        Self {
            initial_stream_window: 65_535,
            initial_connection_window: 65_535,
            max_frame_size: 16_384,
            max_concurrent_streams: 100,
        }
    }
}

pub fn validate_client_preface(bytes: &[u8]) -> Result<(), crate::Error> {
    if bytes == CLIENT_PREFACE {
        Ok(())
    } else {
        Err(crate::Error::Protocol("invalid HTTP/2 client preface"))
    }
}
