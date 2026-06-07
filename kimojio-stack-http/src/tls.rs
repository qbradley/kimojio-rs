// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use kimojio_stack_tls::TlsStream;

use crate::StackTransport;

/// Converts an established stack TLS stream into an HTTP transport.
pub fn transport(stream: TlsStream) -> StackTransport {
    StackTransport::tls(stream)
}
