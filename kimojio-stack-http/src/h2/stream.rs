// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP/2 stream state and flow-control primitives.
//!
//! These types back the higher-level HTTP/2 client/server connections. They are
//! public for tests and custom protocol integration that needs to inspect stream
//! state transitions or flow-control accounting.

use crate::{BodyLimits, Error};

use super::hpack::Header;
use super::settings::MAX_WINDOW_SIZE;

/// Nonzero 31-bit HTTP/2 stream identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u32);

impl StreamId {
    /// Validates and creates a stream ID.
    pub fn new(id: u32) -> Result<Self, Error> {
        if id == 0 || id & 0x8000_0000 != 0 {
            Err(Error::Protocol("invalid HTTP/2 stream id"))
        } else {
            Ok(Self(id))
        }
    }

    /// Creates a stream ID without validation.
    ///
    /// Callers must ensure `id` is nonzero and fits in 31 bits.
    pub const fn from_u31_unchecked(id: u32) -> Self {
        Self(id)
    }

    /// Returns the raw stream ID.
    pub const fn get(self) -> u32 {
        self.0
    }
}

/// HTTP/2 stream state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// No headers have been exchanged.
    Idle,
    /// Both sides may send frames.
    Open,
    /// Local side has ended its stream.
    HalfClosedLocal,
    /// Remote side has ended its stream.
    HalfClosedRemote,
    /// Stream is closed.
    Closed,
}

/// Signed HTTP/2 flow-control window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowControlWindow {
    available: i32,
}

impl FlowControlWindow {
    /// Creates a window with an initial size.
    pub fn new(size: u32) -> Result<Self, Error> {
        if size > MAX_WINDOW_SIZE {
            return Err(Error::Protocol("flow-control window exceeds maximum"));
        }
        Ok(Self {
            available: size as i32,
        })
    }

    /// Returns currently available credit.
    pub fn available(self) -> i32 {
        self.available
    }

    /// Consumes outbound or inbound credit.
    pub fn consume(&mut self, amount: usize) -> Result<(), Error> {
        let amount =
            i32::try_from(amount).map_err(|_| Error::Protocol("flow-control amount too large"))?;
        if amount > self.available {
            return Err(Error::Protocol("flow-control window exhausted"));
        }
        self.available -= amount;
        Ok(())
    }

    /// Increases credit from a WINDOW_UPDATE increment.
    pub fn increase(&mut self, amount: u32) -> Result<(), Error> {
        if amount == 0 {
            return Err(Error::Protocol("WINDOW_UPDATE increment must be non-zero"));
        }
        let amount =
            i32::try_from(amount).map_err(|_| Error::Protocol("flow-control amount too large"))?;
        let next = self
            .available
            .checked_add(amount)
            .ok_or(Error::Protocol("flow-control window overflow"))?;
        if next > MAX_WINDOW_SIZE as i32 {
            return Err(Error::Protocol("flow-control window exceeds maximum"));
        }
        self.available = next;
        Ok(())
    }

    /// Adjusts credit after a peer initial-window setting change.
    pub fn adjust(&mut self, delta: i32) -> Result<(), Error> {
        let next = self
            .available
            .checked_add(delta)
            .ok_or(Error::Protocol("flow-control window overflow"))?;
        if next > MAX_WINDOW_SIZE as i32 {
            return Err(Error::Protocol("flow-control window exceeds maximum"));
        }
        self.available = next;
        Ok(())
    }
}

/// State for one HTTP/2 stream.
#[derive(Debug, Clone)]
pub struct Stream {
    id: StreamId,
    state: StreamState,
    inbound_window: FlowControlWindow,
    outbound_window: FlowControlWindow,
    received_body_len: usize,
    trailers: Option<Vec<Header>>,
    awaiting_continuation: bool,
}

impl Stream {
    /// Creates a stream with local and peer initial windows.
    pub fn new(
        id: StreamId,
        inbound_initial_window: u32,
        outbound_initial_window: u32,
    ) -> Result<Self, Error> {
        Ok(Self {
            id,
            state: StreamState::Idle,
            inbound_window: FlowControlWindow::new(inbound_initial_window)?,
            outbound_window: FlowControlWindow::new(outbound_initial_window)?,
            received_body_len: 0,
            trailers: None,
            awaiting_continuation: false,
        })
    }

    /// Returns the stream ID.
    pub fn id(&self) -> StreamId {
        self.id
    }

    /// Returns the current stream state.
    pub fn state(&self) -> StreamState {
        self.state
    }

    /// Returns inbound flow-control state.
    pub fn inbound_window(&self) -> FlowControlWindow {
        self.inbound_window
    }

    /// Returns outbound flow-control state.
    pub fn outbound_window(&self) -> FlowControlWindow {
        self.outbound_window
    }

    /// Increases inbound window credit.
    pub fn increase_inbound_window(&mut self, amount: u32) -> Result<(), Error> {
        self.inbound_window.increase(amount)
    }

    /// Increases outbound window credit.
    pub fn increase_outbound_window(&mut self, amount: u32) -> Result<(), Error> {
        self.outbound_window.increase(amount)
    }

    /// Consumes outbound window credit.
    pub fn consume_outbound_window(&mut self, amount: usize) -> Result<(), Error> {
        self.outbound_window.consume(amount)
    }

    /// Adjusts outbound credit after peer SETTINGS changes.
    pub fn adjust_outbound_window(&mut self, delta: i32) -> Result<(), Error> {
        self.outbound_window.adjust(delta)
    }

    /// Returns received body bytes counted against body limits.
    pub fn received_body_len(&self) -> usize {
        self.received_body_len
    }

    /// Returns terminal trailers, if received.
    pub fn trailers(&self) -> Option<&[Header]> {
        self.trailers.as_deref()
    }

    /// Applies inbound HEADERS state transition.
    pub fn receive_headers(&mut self, end_headers: bool, end_stream: bool) -> Result<(), Error> {
        if self.awaiting_continuation {
            return Err(Error::Protocol(
                "HEADERS received before header block completed",
            ));
        }
        match self.state {
            StreamState::Idle => {
                self.state = if end_stream {
                    StreamState::HalfClosedRemote
                } else {
                    StreamState::Open
                };
            }
            StreamState::Open | StreamState::HalfClosedLocal if end_stream => {
                self.close_remote_side();
            }
            StreamState::HalfClosedLocal => {}
            _ => return Err(Error::Protocol("HEADERS not valid for stream state")),
        }
        self.awaiting_continuation = !end_headers;
        Ok(())
    }

    /// Applies outbound HEADERS state transition.
    pub fn send_headers(&mut self, end_stream: bool) -> Result<(), Error> {
        match self.state {
            StreamState::Idle => {
                self.state = if end_stream {
                    StreamState::HalfClosedLocal
                } else {
                    StreamState::Open
                };
            }
            StreamState::Open | StreamState::HalfClosedRemote if end_stream => {
                self.close_local_side();
            }
            StreamState::Open | StreamState::HalfClosedRemote => {}
            _ => {
                return Err(Error::Protocol(
                    "outbound HEADERS not valid for stream state",
                ));
            }
        }
        Ok(())
    }

    /// Applies inbound CONTINUATION state transition.
    pub fn receive_continuation(&mut self, end_headers: bool) -> Result<(), Error> {
        if !self.awaiting_continuation {
            return Err(Error::Protocol("unexpected CONTINUATION frame"));
        }
        self.awaiting_continuation = !end_headers;
        Ok(())
    }

    /// Applies inbound DATA accounting and state transition.
    pub fn receive_data(
        &mut self,
        data: &[u8],
        end_stream: bool,
        limits: BodyLimits,
    ) -> Result<(), Error> {
        match self.state {
            StreamState::Open | StreamState::HalfClosedLocal => {}
            _ => return Err(Error::Protocol("DATA not valid for stream state")),
        }
        if self.awaiting_continuation {
            return Err(Error::Protocol(
                "DATA received before header block completed",
            ));
        }
        self.inbound_window.consume(data.len())?;
        let new_len = self.received_body_len.saturating_add(data.len());
        limits.check_body_len(new_len)?;
        self.received_body_len = new_len;
        if end_stream {
            self.close_remote_side();
        }
        Ok(())
    }

    /// Applies outbound DATA state transition.
    pub fn send_data(&mut self, end_stream: bool) -> Result<(), Error> {
        match self.state {
            StreamState::Open | StreamState::HalfClosedRemote => {}
            _ => return Err(Error::Protocol("outbound DATA not valid for stream state")),
        }
        if end_stream {
            self.close_local_side();
        }
        Ok(())
    }

    /// Records inbound trailers and closes the remote side.
    pub fn receive_trailers(&mut self, headers: Vec<Header>) -> Result<(), Error> {
        if self.awaiting_continuation {
            return Err(Error::Protocol(
                "trailers received before header block completed",
            ));
        }
        match self.state {
            StreamState::Open | StreamState::HalfClosedLocal => {
                self.trailers = Some(headers);
                self.close_remote_side();
                Ok(())
            }
            _ => Err(Error::Protocol("trailers not valid for stream state")),
        }
    }

    /// Marks the stream closed.
    pub fn close(&mut self) {
        self.state = StreamState::Closed;
    }

    fn close_remote_side(&mut self) {
        self.state = match self.state {
            StreamState::HalfClosedLocal => StreamState::Closed,
            _ => StreamState::HalfClosedRemote,
        };
    }

    fn close_local_side(&mut self) {
        self.state = match self.state {
            StreamState::HalfClosedRemote => StreamState::Closed,
            _ => StreamState::HalfClosedLocal,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_transitions_from_headers_to_data_end_stream() {
        let mut stream = Stream::new(StreamId::new(1).unwrap(), 65_535, 65_535).unwrap();

        stream.receive_headers(true, false).unwrap();
        assert_eq!(stream.state(), StreamState::Open);
        stream
            .receive_data(b"hello", true, BodyLimits::new(1024))
            .unwrap();

        assert_eq!(stream.state(), StreamState::HalfClosedRemote);
        assert_eq!(stream.received_body_len(), b"hello".len());
    }

    #[test]
    fn flow_control_window_rejects_over_consumption() {
        let mut window = FlowControlWindow::new(5).unwrap();

        assert!(window.consume(6).is_err());
        window.consume(5).unwrap();
        assert_eq!(window.available(), 0);
        window.increase(5).unwrap();
        assert_eq!(window.available(), 5);
    }

    #[test]
    fn flow_control_window_update_can_remain_negative_after_settings_shrink() {
        let mut window = FlowControlWindow::new(100).unwrap();

        window.adjust(-150).unwrap();
        window.increase(25).unwrap();

        assert_eq!(window.available(), -25);
    }

    #[test]
    fn stream_body_limit_is_bounded() {
        let mut stream = Stream::new(StreamId::new(1).unwrap(), 65_535, 65_535).unwrap();
        stream.receive_headers(true, false).unwrap();

        assert!(
            stream
                .receive_data(b"too-large", false, BodyLimits::new(2))
                .is_err()
        );
    }

    #[test]
    fn stream_rejects_continuation_without_pending_header_block() {
        let mut stream = Stream::new(StreamId::new(1).unwrap(), 65_535, 65_535).unwrap();

        assert!(stream.receive_continuation(true).is_err());
    }

    #[test]
    fn stream_rejects_data_before_header_block_completes() {
        let mut stream = Stream::new(StreamId::new(1).unwrap(), 65_535, 65_535).unwrap();
        stream.receive_headers(false, false).unwrap();

        assert!(
            stream
                .receive_data(b"early", false, BodyLimits::new(1024))
                .is_err()
        );

        stream.receive_continuation(true).unwrap();
        stream
            .receive_data(b"ok", false, BodyLimits::new(1024))
            .unwrap();
        assert_eq!(stream.received_body_len(), b"ok".len());
    }

    #[test]
    fn stream_end_stream_closes_when_local_side_already_closed() {
        let mut stream = Stream::new(StreamId::new(1).unwrap(), 65_535, 65_535).unwrap();
        stream.state = StreamState::HalfClosedLocal;

        stream
            .receive_data(b"done", true, BodyLimits::new(1024))
            .unwrap();

        assert_eq!(stream.state(), StreamState::Closed);
    }

    #[test]
    fn stream_half_closed_local_accepts_non_terminal_response_headers() {
        let mut stream = Stream::new(StreamId::new(1).unwrap(), 65_535, 65_535).unwrap();
        stream.state = StreamState::HalfClosedLocal;

        stream.receive_headers(true, false).unwrap();

        assert_eq!(stream.state(), StreamState::HalfClosedLocal);
    }
}
