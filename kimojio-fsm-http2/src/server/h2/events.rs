//! HTTP/2 connection events and text projections.

use crate::server::h2::headers::{
    H2Header, H2HeaderField, H2HeaderProjectionError, H2Request, ValidatedSection,
    project_h2_headers, request_from_headers, status_from_headers,
};
use crate::server::h2::wire::{H2ErrorCode, H2FrameOutcome, H2ProtocolError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct H2InitialWindowSizeChange {
    pub previous: u32,
    pub current: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2ByteStreamEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    RequestHeaders {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

pub type H2ByteStreamEventRef<'a> = H2ByteStreamEvent<&'a [u8]>;

#[derive(Debug)]
pub(crate) enum H2DriverServerEvent<'a> {
    Progress,
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    RequestHeaders {
        stream_id: u32,
        section: ValidatedSection,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: &'a [u8],
        flow_control_len: usize,
        end_stream: bool,
    },
    Trailers {
        stream_id: u32,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

impl<'a> H2DriverServerEvent<'a> {
    pub(crate) fn from_non_header(event: H2ByteStreamEventRef<'a>) -> Self {
        match event {
            H2ByteStreamEvent::Settings { .. }
            | H2ByteStreamEvent::Ping { .. }
            | H2ByteStreamEvent::WindowUpdate { .. } => Self::Progress,
            H2ByteStreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            } => Self::DiscardedData {
                stream_id,
                flow_control_len,
            },
            H2ByteStreamEvent::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            },
            H2ByteStreamEvent::Reset {
                stream_id,
                error_code,
            } => Self::Reset {
                stream_id,
                error_code,
            },
            H2ByteStreamEvent::Goaway {
                last_stream_id,
                error_code,
            } => Self::Goaway {
                last_stream_id,
                error_code,
            },
            H2ByteStreamEvent::RequestHeaders { .. } | H2ByteStreamEvent::Trailers { .. } => {
                unreachable!("header frames use the compact driver path")
            }
        }
    }
}

impl H2ByteStreamEventRef<'_> {
    pub fn into_owned(self) -> H2ByteStreamEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2ByteStreamEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ByteStreamEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ByteStreamEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            } => H2ByteStreamEvent::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ByteStreamEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ByteStreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => {
                H2ByteStreamEvent::Trailers { stream_id, headers }
            }
            Self::Reset {
                stream_id,
                error_code,
            } => H2ByteStreamEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ByteStreamEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

impl<P> H2ByteStreamEvent<P> {
    pub fn try_into_text(self) -> Result<H2StreamEvent<P>, H2HeaderProjectionError> {
        Ok(match self {
            Self::Settings {
                initial_window_size,
            } => H2StreamEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2StreamEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2StreamEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::RequestHeaders {
                stream_id,
                headers,
                end_stream,
            } => {
                let headers = project_h2_headers(headers)?;
                let request = request_from_headers(stream_id, &headers)
                    .map_err(|_| H2HeaderProjectionError::MalformedPseudoHeaders)?;
                H2StreamEvent::RequestHeaders {
                    request,
                    headers,
                    end_stream,
                }
            }
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2StreamEvent::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2StreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2StreamEvent::Trailers {
                stream_id,
                headers: project_h2_headers(headers)?,
            },
            Self::Reset {
                stream_id,
                error_code,
            } => H2StreamEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2StreamEvent::Goaway {
                last_stream_id,
                error_code,
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2StreamEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    RequestHeaders {
        request: H2Request,
        headers: Vec<H2Header>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    /// DATA ignored after a reset but still chargeable to connection flow control.
    ///
    /// Owners must debit the padding-inclusive `flow_control_len`; whether that
    /// credit is later refunded is owner-specific policy. Exhaustive matches
    /// must include this variant.
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2Header>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

/// HTTP/2 server event whose DATA payload borrows the decoded frame input.
pub type H2StreamEventRef<'a> = H2StreamEvent<&'a [u8]>;

impl H2StreamEventRef<'_> {
    /// Converts a borrowed event into the compatibility owned representation.
    pub fn into_owned(self) -> H2StreamEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2StreamEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2StreamEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2StreamEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::RequestHeaders {
                request,
                headers,
                end_stream,
            } => H2StreamEvent::RequestHeaders {
                request,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2StreamEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2StreamEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2StreamEvent::Trailers { stream_id, headers },
            Self::Reset {
                stream_id,
                error_code,
            } => H2StreamEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2StreamEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2ByteClientEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    ResponseHeaders {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2HeaderField>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

pub type H2ByteClientEventRef<'a> = H2ByteClientEvent<&'a [u8]>;

#[derive(Debug)]
pub(crate) enum H2DriverClientEvent<'a> {
    Progress,
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    ResponseHeaders {
        stream_id: u32,
        section: ValidatedSection,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: &'a [u8],
        flow_control_len: usize,
        end_stream: bool,
    },
    Trailers {
        stream_id: u32,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

impl<'a> H2DriverClientEvent<'a> {
    pub(crate) fn from_non_header(event: H2ByteClientEventRef<'a>) -> Self {
        match event {
            H2ByteClientEvent::Settings { .. }
            | H2ByteClientEvent::Ping { .. }
            | H2ByteClientEvent::WindowUpdate { .. } => Self::Progress,
            H2ByteClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            } => Self::DiscardedData {
                stream_id,
                flow_control_len,
            },
            H2ByteClientEvent::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            },
            H2ByteClientEvent::Reset {
                stream_id,
                error_code,
            } => Self::Reset {
                stream_id,
                error_code,
            },
            H2ByteClientEvent::Goaway {
                last_stream_id,
                error_code,
            } => Self::Goaway {
                last_stream_id,
                error_code,
            },
            H2ByteClientEvent::ResponseHeaders { .. } | H2ByteClientEvent::Trailers { .. } => {
                unreachable!("header frames use the compact driver path")
            }
        }
    }
}

impl H2ByteClientEventRef<'_> {
    pub fn into_owned(self) -> H2ByteClientEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2ByteClientEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ByteClientEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ByteClientEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::ResponseHeaders {
                stream_id,
                headers,
                end_stream,
            } => H2ByteClientEvent::ResponseHeaders {
                stream_id,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ByteClientEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ByteClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => {
                H2ByteClientEvent::Trailers { stream_id, headers }
            }
            Self::Reset {
                stream_id,
                error_code,
            } => H2ByteClientEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ByteClientEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}

impl<P> H2ByteClientEvent<P> {
    pub fn try_into_text(self) -> Result<H2ClientEvent<P>, H2HeaderProjectionError> {
        Ok(match self {
            Self::Settings {
                initial_window_size,
            } => H2ClientEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ClientEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ClientEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::ResponseHeaders {
                stream_id,
                headers,
                end_stream,
            } => {
                let headers = project_h2_headers(headers)?;
                let status = status_from_headers(&headers)
                    .map_err(|_| H2HeaderProjectionError::MalformedPseudoHeaders)?;
                H2ClientEvent::ResponseHeaders {
                    stream_id,
                    status,
                    headers,
                    end_stream,
                }
            }
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ClientEvent::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2ClientEvent::Trailers {
                stream_id,
                headers: project_h2_headers(headers)?,
            },
            Self::Reset {
                stream_id,
                error_code,
            } => H2ClientEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ClientEvent::Goaway {
                last_stream_id,
                error_code,
            },
        })
    }
}

pub(crate) fn projection_error(stream_id: u32) -> H2ProtocolError {
    H2ProtocolError::stream(
        stream_id,
        H2ErrorCode::ProtocolError,
        "HTTP/2 byte event cannot be represented by the text convenience view",
    )
}

pub(crate) fn project_stream_outcome<P>(
    outcome: H2FrameOutcome<H2ByteStreamEvent<P>>,
    stream_id: u32,
) -> H2FrameOutcome<H2StreamEvent<P>> {
    match outcome {
        H2FrameOutcome::Event(event) => match event.try_into_text() {
            Ok(event) => H2FrameOutcome::Event(event),
            Err(_) => H2FrameOutcome::Error(projection_error(stream_id)),
        },
        H2FrameOutcome::Ignored => H2FrameOutcome::Ignored,
        H2FrameOutcome::Error(error) => H2FrameOutcome::Error(error),
    }
}

pub(crate) fn project_client_outcome<P>(
    outcome: H2FrameOutcome<H2ByteClientEvent<P>>,
    stream_id: u32,
) -> H2FrameOutcome<H2ClientEvent<P>> {
    match outcome {
        H2FrameOutcome::Event(event) => match event.try_into_text() {
            Ok(event) => H2FrameOutcome::Event(event),
            Err(_) => H2FrameOutcome::Error(projection_error(stream_id)),
        },
        H2FrameOutcome::Ignored => H2FrameOutcome::Ignored,
        H2FrameOutcome::Error(error) => H2FrameOutcome::Error(error),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum H2ClientEvent<P = Vec<u8>> {
    Settings {
        initial_window_size: Option<H2InitialWindowSizeChange>,
    },
    Ping {
        ack: bool,
    },
    WindowUpdate {
        stream_id: u32,
        increment: u32,
    },
    ResponseHeaders {
        stream_id: u32,
        status: u16,
        headers: Vec<H2Header>,
        end_stream: bool,
    },
    Data {
        stream_id: u32,
        payload: P,
        flow_control_len: usize,
        end_stream: bool,
    },
    /// DATA ignored after a reset but still chargeable to connection flow control.
    ///
    /// Owners must debit the padding-inclusive `flow_control_len`; whether that
    /// credit is later refunded is owner-specific policy. Exhaustive matches
    /// must include this variant.
    DiscardedData {
        stream_id: u32,
        flow_control_len: usize,
    },
    Trailers {
        stream_id: u32,
        headers: Vec<H2Header>,
    },
    Reset {
        stream_id: u32,
        error_code: u32,
    },
    Goaway {
        last_stream_id: u32,
        error_code: u32,
    },
}

/// HTTP/2 client event whose DATA payload borrows the decoded frame input.
pub type H2ClientEventRef<'a> = H2ClientEvent<&'a [u8]>;

impl H2ClientEventRef<'_> {
    /// Converts a borrowed event into the compatibility owned representation.
    pub fn into_owned(self) -> H2ClientEvent {
        match self {
            Self::Settings {
                initial_window_size,
            } => H2ClientEvent::Settings {
                initial_window_size,
            },
            Self::Ping { ack } => H2ClientEvent::Ping { ack },
            Self::WindowUpdate {
                stream_id,
                increment,
            } => H2ClientEvent::WindowUpdate {
                stream_id,
                increment,
            },
            Self::ResponseHeaders {
                stream_id,
                status,
                headers,
                end_stream,
            } => H2ClientEvent::ResponseHeaders {
                stream_id,
                status,
                headers,
                end_stream,
            },
            Self::Data {
                stream_id,
                payload,
                flow_control_len,
                end_stream,
            } => H2ClientEvent::Data {
                stream_id,
                payload: payload.to_vec(),
                flow_control_len,
                end_stream,
            },
            Self::DiscardedData {
                stream_id,
                flow_control_len,
            } => H2ClientEvent::DiscardedData {
                stream_id,
                flow_control_len,
            },
            Self::Trailers { stream_id, headers } => H2ClientEvent::Trailers { stream_id, headers },
            Self::Reset {
                stream_id,
                error_code,
            } => H2ClientEvent::Reset {
                stream_id,
                error_code,
            },
            Self::Goaway {
                last_stream_id,
                error_code,
            } => H2ClientEvent::Goaway {
                last_stream_id,
                error_code,
            },
        }
    }
}
