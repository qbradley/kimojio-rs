// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Shared configuration for unary gRPC clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientConfig {
    pub max_message_len: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
        }
    }
}
