// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::HttpConfig;

/// Shared configuration for stackful HTTP clients.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientConfig {
    pub http: HttpConfig,
}
