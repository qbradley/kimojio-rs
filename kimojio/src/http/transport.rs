// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::io::IoSlice;
use std::time::Instant;

use crate::{AsyncStreamRead, AsyncStreamWrite, Errno, OwnedFdStream};

// `OwnedFdStream` already owns a 16 KiB read buffer. Boxing it would add a
// cleartext allocation, while boxing the much smaller TLS stream would not
// reduce this enum.
#[allow(clippy::large_enum_variant)]
pub(super) enum Transport {
    Plain(OwnedFdStream),
    #[cfg(feature = "tls")]
    Tls(crate::tlsstream::TlsStream),
}

impl Transport {
    pub(super) fn try_clone_fd(&self) -> Result<crate::OwnedFd, Errno> {
        match self {
            Self::Plain(stream) => stream.try_clone_fd(),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.try_clone_fd(),
        }
    }

    pub(super) async fn try_read_for_reuse(
        &mut self,
        buffer: &mut [u8],
    ) -> Result<Option<usize>, Errno> {
        match self {
            Self::Plain(stream) => stream.try_read_for_reuse(buffer),
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.try_read_for_reuse(buffer).await,
        }
    }

    pub(super) async fn write_with_progress(
        &mut self,
        buffer: &[u8],
        deadline: Option<Instant>,
        bytes_written: &mut bool,
    ) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => {
                stream
                    .write_with_progress(buffer, deadline, bytes_written)
                    .await
            }
            #[cfg(feature = "tls")]
            Self::Tls(stream) => {
                stream
                    .write_with_progress(buffer, deadline, bytes_written)
                    .await
            }
        }
    }

    pub(super) async fn writev_with_progress(
        &mut self,
        buffers: &mut [IoSlice<'_>],
        deadline: Option<Instant>,
        bytes_written: &mut bool,
    ) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => {
                stream
                    .writev_with_progress(buffers, deadline, bytes_written)
                    .await
            }
            #[cfg(feature = "tls")]
            Self::Tls(stream) => {
                stream
                    .writev_with_progress(buffers, deadline, bytes_written)
                    .await
            }
        }
    }
}

impl AsyncStreamRead for Transport {
    async fn try_read(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<Instant>,
    ) -> Result<usize, Errno> {
        match self {
            Self::Plain(stream) => stream.try_read(buffer, deadline).await,
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.try_read(buffer, deadline).await,
        }
    }

    async fn read(&mut self, buffer: &mut [u8], deadline: Option<Instant>) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => stream.read(buffer, deadline).await,
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.read(buffer, deadline).await,
        }
    }
}

impl AsyncStreamWrite for Transport {
    async fn write(&mut self, buffer: &[u8], deadline: Option<Instant>) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => stream.write(buffer, deadline).await,
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.write(buffer, deadline).await,
        }
    }

    async fn shutdown(&mut self) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => stream.shutdown().await,
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.shutdown().await,
        }
    }

    async fn close(&mut self) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => stream.close().await,
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.close().await,
        }
    }

    async fn writev<'a>(
        &'a mut self,
        buffers: &'a mut [IoSlice<'a>],
        deadline: Option<Instant>,
    ) -> Result<(), Errno> {
        match self {
            Self::Plain(stream) => stream.writev(buffers, deadline).await,
            #[cfg(feature = "tls")]
            Self::Tls(stream) => stream.writev(buffers, deadline).await,
        }
    }
}
