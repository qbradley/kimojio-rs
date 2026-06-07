// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::HttpConfig;

/// Shared configuration for stackful HTTP servers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerConfig {
    pub http: HttpConfig,
}
