// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::io_type::IOType;
use uuid::Uuid;

/// A wrapper containing a trace event with context information.
pub struct EventEnvelope {
    /// The index of the thread that generated this event.
    pub thread_index: u8,
    /// The index of the task that generated this event.
    pub task_index: u16,
    /// The trace event.
    pub event: Events,
}

/// Identifies the higher-level TLS operation that caused a TCP read or write.
#[cfg(feature = "tls")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsOperation {
    /// The TCP I/O was performed during the TLS handshake.
    Handshake,
    /// The TCP I/O was performed on behalf of an SSL read (decrypting incoming data).
    SslRead,
    /// The TCP I/O was performed on behalf of an SSL write (encrypting outgoing data).
    SslWrite,
    /// The TCP I/O was performed during TLS shutdown.
    Shutdown,
}

#[cfg(feature = "tls")]
impl std::fmt::Display for TlsOperation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlsOperation::Handshake => write!(f, "handshake"),
            TlsOperation::SslRead => write!(f, "ssl_read"),
            TlsOperation::SslWrite => write!(f, "ssl_write"),
            TlsOperation::Shutdown => write!(f, "shutdown"),
        }
    }
}

/// Trace events emitted by the runtime for debugging and performance analysis.
pub enum Events {
    TaskScheduled {
        activity_id: Uuid,
        tenant_id: Uuid,
        tag: u32,
        io: bool,
    },
    TaskRunEnd {
        activity_id: Uuid,
        start_time: u64,
        tag: u32,
        cpu: u16,
        action: u8,
        complete: bool,
    },
    RingEnterEnd {
        submissions: u64,
        in_flight_io: u64,
        start_time: u64,
        completions: u64,
        tag: u32,
        task_count: u32,
        want: bool,
        iopoll: bool,
    },
    AsyncWaitStart {
        activity_id: Uuid,
        tag: u32,
    },
    AsyncWaitEnd {
        activity_id: Uuid,
        tag: u32,
    },
    IoStart {
        activity_id: Uuid,
        fd: i32,
        tag: u32,
        io_type: IOType,
    },
    IoEnd {
        activity_id: Uuid,
        result: i32,
        tag: u32,
    },
    IoError {
        activity_id: Uuid,
        tag: u32,
        error: i32,
    },
    #[cfg(feature = "tls")]
    TlsError {
        activity_id: Uuid,
        code: u64,
    },
    /// A TlsStream was created with its underlying socket.
    #[cfg(feature = "tls")]
    TlsStreamCreated {
        activity_id: Uuid,
        fd: i32,
        is_client: bool,
    },
    /// A TLS handshake has started.
    #[cfg(feature = "tls")]
    TlsHandshakeStarted {
        activity_id: Uuid,
        is_client: bool,
    },
    /// A TLS handshake completed successfully.
    #[cfg(feature = "tls")]
    TlsHandshakeCompleted {
        activity_id: Uuid,
        is_client: bool,
    },
    /// A TCP read was performed on behalf of a TLS operation.
    #[cfg(feature = "tls")]
    TlsTcpRead {
        activity_id: Uuid,
        bytes: usize,
        on_behalf_of: TlsOperation,
    },
    /// A TCP write was performed on behalf of a TLS operation.
    #[cfg(feature = "tls")]
    TlsTcpWrite {
        activity_id: Uuid,
        bytes: usize,
        on_behalf_of: TlsOperation,
    },
    /// An SSL-level read completed (application data decrypted).
    #[cfg(feature = "tls")]
    TlsSslRead {
        activity_id: Uuid,
        bytes: usize,
    },
    /// An SSL-level write completed (application data encrypted).
    #[cfg(feature = "tls")]
    TlsSslWrite {
        activity_id: Uuid,
        bytes: usize,
    },
    /// A TLS shutdown has started.
    #[cfg(feature = "tls")]
    TlsShutdownStarted {
        activity_id: Uuid,
    },
    /// A TLS shutdown completed successfully.
    #[cfg(feature = "tls")]
    TlsShutdownCompleted {
        activity_id: Uuid,
    },
    /// A TLS stream was closed (socket released).
    #[cfg(feature = "tls")]
    TlsStreamClosed {
        activity_id: Uuid,
        cause: Option<i32>,
    },
    FutureCanceled {
        activity_id: Uuid,
    },
}

/// A trait for receiving trace events from the runtime.
///
/// Implement this trait to collect and process runtime trace events.
pub trait TraceConfiguration: Send {
    /// Called when a trace event is emitted.
    fn trace(&self, event: EventEnvelope);
}
