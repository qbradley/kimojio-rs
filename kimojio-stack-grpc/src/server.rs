// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

/// Shared configuration for unary gRPC servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerConfig {
    pub max_message_len: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            max_message_len: 4 * 1024 * 1024,
        }
    }
}
