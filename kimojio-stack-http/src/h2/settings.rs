// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! HTTP/2 settings and validation helpers.

use crate::Error;

/// Default maximum HTTP/2 frame payload size.
pub const DEFAULT_MAX_FRAME_SIZE: u32 = 16_384;
/// Largest frame size permitted by RFC 9113.
pub const MAX_ALLOWED_FRAME_SIZE: u32 = 16_777_215;
/// Largest flow-control window permitted by HTTP/2.
pub const MAX_WINDOW_SIZE: u32 = 2_147_483_647;

/// HTTP/2 SETTINGS identifiers supported by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum SettingId {
    /// SETTINGS_HEADER_TABLE_SIZE.
    HeaderTableSize = 0x1,
    /// SETTINGS_ENABLE_PUSH.
    EnablePush = 0x2,
    /// SETTINGS_MAX_CONCURRENT_STREAMS.
    MaxConcurrentStreams = 0x3,
    /// SETTINGS_INITIAL_WINDOW_SIZE.
    InitialWindowSize = 0x4,
    /// SETTINGS_MAX_FRAME_SIZE.
    MaxFrameSize = 0x5,
    /// SETTINGS_MAX_HEADER_LIST_SIZE.
    MaxHeaderListSize = 0x6,
}

impl SettingId {
    /// Converts a raw SETTINGS identifier.
    pub fn from_u16(id: u16) -> Option<Self> {
        match id {
            0x1 => Some(Self::HeaderTableSize),
            0x2 => Some(Self::EnablePush),
            0x3 => Some(Self::MaxConcurrentStreams),
            0x4 => Some(Self::InitialWindowSize),
            0x5 => Some(Self::MaxFrameSize),
            0x6 => Some(Self::MaxHeaderListSize),
            _ => None,
        }
    }
}

/// One HTTP/2 setting value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setting {
    /// Setting identifier.
    pub id: SettingId,
    /// Raw setting value.
    pub value: u32,
}

impl Setting {
    /// Creates one setting value.
    pub const fn new(id: SettingId, value: u32) -> Self {
        Self { id, value }
    }
}

/// Effective HTTP/2 connection settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// HPACK dynamic table size.
    pub header_table_size: u32,
    /// Whether server push is enabled by the peer.
    pub enable_push: bool,
    /// Maximum concurrent streams.
    pub max_concurrent_streams: u32,
    /// Initial per-stream flow-control window.
    pub initial_window_size: u32,
    /// Maximum frame payload size.
    pub max_frame_size: u32,
    /// Maximum decompressed header-list size.
    pub max_header_list_size: u32,
}

impl Settings {
    /// Applies one setting after validating its value.
    pub fn apply(&mut self, setting: Setting) -> Result<(), Error> {
        match setting.id {
            SettingId::HeaderTableSize => self.header_table_size = setting.value,
            SettingId::EnablePush => match setting.value {
                0 => self.enable_push = false,
                1 => self.enable_push = true,
                _ => return Err(Error::Protocol("SETTINGS_ENABLE_PUSH must be 0 or 1")),
            },
            SettingId::MaxConcurrentStreams => self.max_concurrent_streams = setting.value,
            SettingId::InitialWindowSize => {
                if setting.value > MAX_WINDOW_SIZE {
                    return Err(Error::Protocol(
                        "SETTINGS_INITIAL_WINDOW_SIZE exceeds maximum",
                    ));
                }
                self.initial_window_size = setting.value;
            }
            SettingId::MaxFrameSize => {
                if !(DEFAULT_MAX_FRAME_SIZE..=MAX_ALLOWED_FRAME_SIZE).contains(&setting.value) {
                    return Err(Error::Protocol("SETTINGS_MAX_FRAME_SIZE is outside range"));
                }
                self.max_frame_size = setting.value;
            }
            SettingId::MaxHeaderListSize => self.max_header_list_size = setting.value,
        }
        Ok(())
    }

    /// Applies settings in order.
    pub fn apply_all(&mut self, settings: &[Setting]) -> Result<(), Error> {
        for &setting in settings {
            self.apply(setting)?;
        }
        Ok(())
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            header_table_size: 4096,
            enable_push: true,
            max_concurrent_streams: u32::MAX,
            initial_window_size: 65_535,
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_header_list_size: u32::MAX,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_apply_updates_and_ack_state() {
        let mut settings = Settings::default();

        settings
            .apply_all(&[
                Setting::new(SettingId::EnablePush, 0),
                Setting::new(SettingId::InitialWindowSize, 1024),
                Setting::new(SettingId::MaxFrameSize, 32 * 1024),
            ])
            .unwrap();

        assert!(!settings.enable_push);
        assert_eq!(settings.initial_window_size, 1024);
        assert_eq!(settings.max_frame_size, 32 * 1024);
    }

    #[test]
    fn settings_reject_invalid_values() {
        let mut settings = Settings::default();

        assert!(
            settings
                .apply(Setting::new(
                    SettingId::InitialWindowSize,
                    MAX_WINDOW_SIZE + 1
                ))
                .is_err()
        );
        assert!(
            settings
                .apply(Setting::new(
                    SettingId::MaxFrameSize,
                    DEFAULT_MAX_FRAME_SIZE - 1
                ))
                .is_err()
        );
    }
}
