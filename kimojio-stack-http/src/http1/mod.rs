// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// HTTP/1.1 connection configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Http1Config {
    pub keep_alive: bool,
}

impl Default for Http1Config {
    fn default() -> Self {
        Self { keep_alive: true }
    }
}
