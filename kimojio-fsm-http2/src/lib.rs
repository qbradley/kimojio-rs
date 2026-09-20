//! Synchronous HTTP/2 protocol components.

mod api;
mod engine;
mod error;
pub use api::*;
pub use engine::{Client, Config, Connection, Server};
#[allow(dead_code)]
mod head;
#[allow(dead_code)]
mod hpack;
mod huffman_table;
mod limits;
// The private donor layer retains independently testable codec entry points.
#[allow(dead_code, unused_imports)]
mod server;

pub use error::*;
pub use limits::*;
pub use server::ServerError;
pub use server::h2::headers::H2RawHeader;
pub use server::h2::headers::{
    H2HeaderBlockDecoder, H2HeaderBlockEncoder, H2HeaderField, H2RawHeaderRef,
};
pub use server::h2::wire::{H2ErrorCode, H2ErrorScope, H2HpackError, H2ProtocolError};

/// Borrowed semantic header pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Header<'a> {
    pub name: &'a str,
    pub value: &'a str,
}

impl<'a> Header<'a> {
    pub const fn new(name: &'a str, value: &'a str) -> Self {
        Self { name, value }
    }
}
