// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::{BTreeMap, VecDeque};

use crate::Error;

use super::frame::{Frame, FrameFlags, FramePayload, FrameType};
use super::hpack::{Header, HeaderBlockDecoder, HeaderBlockEncoder};
use super::settings::{SettingId, Settings};
use super::stream::{FlowControlWindow, Stream, StreamId};

const INITIAL_CONNECTION_WINDOW: u32 = 65_535;

pub struct ConnectionState {
    local_settings: Settings,
    peer_settings: Settings,
    streams: BTreeMap<StreamId, Stream>,
    connection_window: FlowControlWindow,
    pending_outbound: VecDeque<Frame>,
    header_encoder: HeaderBlockEncoder,
    header_decoder: HeaderBlockDecoder,
    continuation_stream: Option<StreamId>,
    preface_received: bool,
}

impl ConnectionState {
    pub fn new(local_settings: Settings) -> Result<Self, Error> {
        let mut header_decoder = HeaderBlockDecoder::new();
        header_decoder.set_max_table_size(local_settings.header_table_size as usize);

        Ok(Self {
            local_settings,
            peer_settings: Settings::default(),
            streams: BTreeMap::new(),
            connection_window: FlowControlWindow::new(INITIAL_CONNECTION_WINDOW)?,
            pending_outbound: VecDeque::new(),
            header_encoder: HeaderBlockEncoder::new(),
            header_decoder,
            continuation_stream: None,
            preface_received: false,
        })
    }

    pub fn local_settings(&self) -> Settings {
        self.local_settings
    }

    pub fn peer_settings(&self) -> Settings {
        self.peer_settings
    }

    pub fn connection_window(&self) -> FlowControlWindow {
        self.connection_window
    }

    pub fn pending_outbound_len(&self) -> usize {
        self.pending_outbound.len()
    }

    pub fn receive_preface(&mut self, preface: &[u8]) -> Result<(), Error> {
        super::validate_client_preface(preface)?;
        self.preface_received = true;
        Ok(())
    }

    pub fn preface_received(&self) -> bool {
        self.preface_received
    }

    pub fn encode_header_block(&mut self, headers: &[Header]) -> Vec<u8> {
        self.header_encoder.encode(headers)
    }

    pub fn decode_header_block(&mut self, block: &[u8]) -> Result<Vec<Header>, Error> {
        self.header_decoder
            .decode_with_limit(block, self.local_settings.max_header_list_size as usize)
    }

    pub fn track_inbound_frame(&mut self, frame: &Frame) -> Result<(), Error> {
        if let Some(stream_id) = self.continuation_stream {
            if frame.frame_type != FrameType::Continuation || frame.stream_id != stream_id.get() {
                return Err(Error::Protocol(
                    "frame received before CONTINUATION sequence completed",
                ));
            }
            if frame.flags.contains(FrameFlags::END_HEADERS) {
                self.continuation_stream = None;
            }
            return Ok(());
        }

        match frame.frame_type {
            FrameType::Headers => {
                let stream_id = StreamId::new(frame.stream_id)?;
                if !frame.flags.contains(FrameFlags::END_HEADERS) {
                    self.continuation_stream = Some(stream_id);
                }
                Ok(())
            }
            FrameType::Continuation => Err(Error::Protocol("unexpected CONTINUATION frame")),
            _ => Ok(()),
        }
    }

    pub fn open_stream(&mut self, id: StreamId) -> Result<&mut Stream, Error> {
        if !self.streams.contains_key(&id) {
            let stream = Stream::new(
                id,
                self.local_settings.initial_window_size,
                self.peer_settings.initial_window_size,
            )?;
            self.streams.insert(id, stream);
        }
        Ok(self.streams.get_mut(&id).expect("stream was just inserted"))
    }

    pub fn stream(&self, id: StreamId) -> Option<&Stream> {
        self.streams.get(&id)
    }

    pub fn stream_mut(&mut self, id: StreamId) -> Option<&mut Stream> {
        self.streams.get_mut(&id)
    }

    pub fn outbound_capacity(&self, stream_id: StreamId) -> Result<usize, Error> {
        let stream = self
            .streams
            .get(&stream_id)
            .ok_or(Error::Protocol("unknown stream"))?;
        let available = self
            .connection_window
            .available()
            .min(stream.outbound_window().available());
        Ok(available.max(0) as usize)
    }

    pub fn consume_outbound_window(
        &mut self,
        stream_id: StreamId,
        amount: usize,
    ) -> Result<(), Error> {
        let stream = self
            .streams
            .get_mut(&stream_id)
            .ok_or(Error::Protocol("unknown stream"))?;
        self.connection_window.consume(amount)?;
        stream.consume_outbound_window(amount)
    }

    pub fn queue_window_update(&mut self, stream_id: StreamId, amount: usize) -> Result<(), Error> {
        if amount == 0 {
            return Ok(());
        }
        let increment = u32::try_from(amount)
            .map_err(|_| Error::Protocol("WINDOW_UPDATE increment too large"))?;
        self.streams
            .get_mut(&stream_id)
            .ok_or(Error::Protocol("WINDOW_UPDATE for unknown stream"))?
            .increase_inbound_window(increment)?;
        self.pending_outbound
            .push_back(Frame::window_update(0, increment));
        self.pending_outbound
            .push_back(Frame::window_update(stream_id.get(), increment));
        Ok(())
    }

    pub fn receive_settings(&mut self, frame: &Frame) -> Result<(), Error> {
        self.track_inbound_frame(frame)?;
        let FramePayload::Settings(settings) = &frame.payload else {
            return Err(Error::Protocol("expected SETTINGS frame"));
        };
        if frame.flags.contains(FrameFlags::ACK) {
            if !settings.is_empty() {
                return Err(Error::Protocol("SETTINGS ack must have empty payload"));
            }
            return Ok(());
        }
        let old_initial_window = self.peer_settings.initial_window_size;
        let mut peer_settings = self.peer_settings;
        peer_settings.apply_all(settings)?;
        if settings
            .iter()
            .any(|setting| setting.id == SettingId::InitialWindowSize)
        {
            let delta = peer_settings.initial_window_size as i32 - old_initial_window as i32;
            for stream in self.streams.values_mut() {
                stream.adjust_outbound_window(delta)?;
            }
        }
        self.peer_settings = peer_settings;
        if settings
            .iter()
            .any(|setting| setting.id == SettingId::HeaderTableSize)
        {
            self.header_encoder
                .set_max_table_size(self.peer_settings.header_table_size as usize);
        }
        self.pending_outbound.push_back(Frame::settings_ack());
        Ok(())
    }

    pub fn receive_window_update(&mut self, frame: &Frame) -> Result<(), Error> {
        self.track_inbound_frame(frame)?;
        let FramePayload::WindowUpdate { increment } = frame.payload else {
            return Err(Error::Protocol("expected WINDOW_UPDATE frame"));
        };
        if frame.stream_id == 0 {
            self.connection_window.increase(increment)
        } else {
            let stream_id = StreamId::new(frame.stream_id)?;
            self.streams
                .get_mut(&stream_id)
                .ok_or(Error::Protocol("WINDOW_UPDATE for unknown stream"))?
                .increase_outbound_window(increment)
        }
    }

    pub fn queue_frame(&mut self, frame: Frame) {
        self.pending_outbound.push_back(frame);
    }

    pub fn pop_outbound(&mut self) -> Option<Frame> {
        self.pending_outbound.pop_front()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h2::settings::{Setting, SettingId};

    #[test]
    fn connection_validates_preface_and_acks_settings() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();

        connection
            .receive_preface(super::super::CLIENT_PREFACE)
            .unwrap();
        connection
            .receive_settings(&Frame::settings(vec![Setting::new(
                SettingId::InitialWindowSize,
                1024,
            )]))
            .unwrap();

        assert!(connection.preface_received());
        assert_eq!(connection.peer_settings().initial_window_size, 1024);
        assert_eq!(connection.pending_outbound_len(), 1);
        assert_eq!(connection.pop_outbound().unwrap(), Frame::settings_ack());
    }

    #[test]
    fn connection_tracks_stream_map() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();
        let stream_id = StreamId::new(1).unwrap();

        connection.open_stream(stream_id).unwrap();

        assert_eq!(connection.stream(stream_id).unwrap().id(), stream_id);
    }

    #[test]
    fn connection_applies_stream_window_updates() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();
        let stream_id = StreamId::new(1).unwrap();
        connection.open_stream(stream_id).unwrap();

        connection
            .receive_window_update(&Frame {
                frame_type: crate::h2::FrameType::WindowUpdate,
                flags: FrameFlags::EMPTY,
                stream_id: stream_id.get(),
                payload: FramePayload::WindowUpdate { increment: 1024 },
            })
            .unwrap();

        assert_eq!(
            connection
                .stream(stream_id)
                .unwrap()
                .outbound_window()
                .available(),
            65_535 + 1024
        );
    }

    #[test]
    fn connection_rejects_window_update_for_unknown_stream() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();

        assert!(
            connection
                .receive_window_update(&Frame {
                    frame_type: crate::h2::FrameType::WindowUpdate,
                    flags: FrameFlags::EMPTY,
                    stream_id: 1,
                    payload: FramePayload::WindowUpdate { increment: 1024 },
                })
                .is_err()
        );
    }

    #[test]
    fn connection_applies_peer_initial_window_update_to_existing_streams() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();
        let stream_id = StreamId::new(1).unwrap();
        connection.open_stream(stream_id).unwrap();

        connection
            .receive_settings(&Frame::settings(vec![Setting::new(
                SettingId::InitialWindowSize,
                1024,
            )]))
            .unwrap();

        assert_eq!(
            connection
                .stream(stream_id)
                .unwrap()
                .outbound_window()
                .available(),
            1024
        );
    }

    #[test]
    fn connection_uses_local_and_peer_settings_for_new_stream_windows() {
        let local_settings = Settings {
            initial_window_size: 2048,
            ..Settings::default()
        };
        let mut connection = ConnectionState::new(local_settings).unwrap();
        connection
            .receive_settings(&Frame::settings(vec![Setting::new(
                SettingId::InitialWindowSize,
                1024,
            )]))
            .unwrap();
        let stream_id = StreamId::new(1).unwrap();

        connection.open_stream(stream_id).unwrap();
        let stream = connection.stream(stream_id).unwrap();

        assert_eq!(stream.inbound_window().available(), 2048);
        assert_eq!(stream.outbound_window().available(), 1024);
        assert_eq!(connection.connection_window().available(), 65_535);
    }

    #[test]
    fn connection_applies_peer_header_table_size_to_encoder() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();
        connection
            .receive_settings(&Frame::settings(vec![Setting::new(
                SettingId::HeaderTableSize,
                0,
            )]))
            .unwrap();
        let headers = vec![Header::new("custom-key", "custom-value")];
        let first = connection.encode_header_block(&headers);
        let second = connection.encode_header_block(&headers);
        let mut peer_decoder = HeaderBlockDecoder::new();
        peer_decoder.set_max_table_size(0);

        assert_eq!(
            peer_decoder.decode_with_limit(&first, usize::MAX).unwrap(),
            headers
        );
        assert_eq!(
            peer_decoder.decode_with_limit(&second, usize::MAX).unwrap(),
            headers
        );
    }

    #[test]
    fn connection_rejects_interleaved_frames_during_continuation_sequence() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();
        let stream_id = StreamId::new(1).unwrap();
        let other_stream_id = StreamId::new(3).unwrap();
        let partial_headers = Frame {
            frame_type: FrameType::Headers,
            flags: FrameFlags::EMPTY,
            stream_id: stream_id.get(),
            payload: FramePayload::Headers(bytes::Bytes::from_static(b"partial")),
        };

        connection.track_inbound_frame(&partial_headers).unwrap();

        assert!(
            connection
                .track_inbound_frame(&Frame::data(
                    other_stream_id,
                    bytes::Bytes::from_static(b"data"),
                    false,
                ))
                .is_err()
        );
        assert!(
            connection
                .track_inbound_frame(&Frame::continuation(
                    other_stream_id,
                    bytes::Bytes::from_static(b"wrong"),
                    true,
                ))
                .is_err()
        );
        connection
            .track_inbound_frame(&Frame::continuation(
                stream_id,
                bytes::Bytes::from_static(b"done"),
                true,
            ))
            .unwrap();
        connection
            .track_inbound_frame(&Frame::data(
                other_stream_id,
                bytes::Bytes::from_static(b"data"),
                false,
            ))
            .unwrap();
    }

    #[test]
    fn connection_does_not_ack_settings_ack() {
        let mut connection = ConnectionState::new(Settings::default()).unwrap();

        connection.receive_settings(&Frame::settings_ack()).unwrap();

        assert_eq!(connection.pending_outbound_len(), 0);
    }

    #[test]
    fn connection_encodes_and_decodes_header_blocks_with_limits() {
        let settings = Settings {
            max_header_list_size: 1024,
            ..Settings::default()
        };
        let mut connection = ConnectionState::new(settings).unwrap();
        let headers = vec![Header::new(":method", "POST"), Header::new(":path", "/rpc")];

        let encoded = connection.encode_header_block(&headers);
        let decoded = connection.decode_header_block(&encoded).unwrap();

        assert_eq!(decoded, headers);
    }
}
