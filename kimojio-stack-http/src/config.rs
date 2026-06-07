// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Shared HTTP parser, buffering, and body limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpConfig {
    pub max_start_line_len: usize,
    pub max_header_count: usize,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    pub read_buffer_size: usize,
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            max_start_line_len: 8 * 1024,
            max_header_count: 100,
            max_header_bytes: 64 * 1024,
            max_body_bytes: 4 * 1024 * 1024,
            read_buffer_size: 16 * 1024,
        }
    }
}
