// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use bytes::BytesMut;

use crate::{BodyLimits, Error};

use super::hpack::Header;
use super::settings::MAX_WINDOW_SIZE;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StreamId(u32);

impl StreamId {
    pub fn new(id: u32) -> Result<Self, Error> {
        if id == 0 || id & 0x8000_0000 != 0 {
            Err(Error::Protocol("invalid HTTP/2 stream id"))
        } else {
            Ok(Self(id))
        }
    }

    pub const fn from_u31_unchecked(id: u32) -> Self {
        Self(id)
    }

    pub const fn get(self) -> u32 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    Idle,
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowControlWindow {
    available: i32,
}

impl FlowControlWindow {
    pub fn new(size: u32) -> Result<Self, Error> {
        if size > MAX_WINDOW_SIZE {
            return Err(Error::Protocol("flow-control window exceeds maximum"));
        }
        Ok(Self {
            available: size as i32,
        })
    }

    pub fn available(self) -> i32 {
        self.available
    }

    pub fn consume(&mut self, amount: usize) -> Result<(), Error> {
        let amount =
            i32::try_from(amount).map_err(|_| Error::Protocol("flow-control amount too large"))?;
        if amount > self.available {
            return Err(Error::Protocol("flow-control window exhausted"));
        }
        self.available -= amount;
        Ok(())
    }

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

#[derive(Debug, Clone)]
pub struct Stream {
    id: StreamId,
    state: StreamState,
    inbound_window: FlowControlWindow,
    outbound_window: FlowControlWindow,
    body: BytesMut,
    trailers: Option<Vec<Header>>,
    awaiting_continuation: bool,
}

impl Stream {
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
            body: BytesMut::new(),
            trailers: None,
            awaiting_continuation: false,
        })
    }

    pub fn id(&self) -> StreamId {
        self.id
    }

    pub fn state(&self) -> StreamState {
        self.state
    }

    pub fn inbound_window(&self) -> FlowControlWindow {
        self.inbound_window
    }

    pub fn outbound_window(&self) -> FlowControlWindow {
        self.outbound_window
    }

    pub fn increase_outbound_window(&mut self, amount: u32) -> Result<(), Error> {
        self.outbound_window.increase(amount)
    }

    pub fn adjust_outbound_window(&mut self, delta: i32) -> Result<(), Error> {
        self.outbound_window.adjust(delta)
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }

    pub fn trailers(&self) -> Option<&[Header]> {
        self.trailers.as_deref()
    }

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

    pub fn receive_continuation(&mut self, end_headers: bool) -> Result<(), Error> {
        if !self.awaiting_continuation {
            return Err(Error::Protocol("unexpected CONTINUATION frame"));
        }
        self.awaiting_continuation = !end_headers;
        Ok(())
    }

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
        limits.check_body_len(self.body.len().saturating_add(data.len()))?;
        self.body.extend_from_slice(data);
        if end_stream {
            self.close_remote_side();
        }
        Ok(())
    }

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

    pub fn close(&mut self) {
        self.state = StreamState::Closed;
    }

    fn close_remote_side(&mut self) {
        self.state = match self.state {
            StreamState::HalfClosedLocal => StreamState::Closed,
            _ => StreamState::HalfClosedRemote,
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
        assert_eq!(stream.body(), b"hello");
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
        assert_eq!(stream.body(), b"ok");
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
