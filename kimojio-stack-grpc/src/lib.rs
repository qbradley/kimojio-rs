// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Unary-first gRPC foundations for `kimojio-stack`.

pub mod client;
pub mod codec;
pub mod error;
pub mod metadata;
pub mod server;
pub mod status;

pub use codec::{decode_message, encode_message};
pub use error::{Error, ErrorKind};
pub use metadata::Metadata;
pub use status::{Status, StatusCode};

#[cfg(test)]
mod tests {
    use prost::Message;

    use super::{Status, StatusCode, decode_message, encode_message};

    #[derive(Clone, PartialEq, Message)]
    struct TestMessage {
        #[prost(string, tag = "1")]
        value: String,
    }

    #[test]
    fn unary_frame_round_trips_prost_message() {
        let message = TestMessage {
            value: "hello".to_string(),
        };

        let frame = encode_message(&message, 1024).unwrap();
        let decoded = decode_message::<TestMessage>(&frame, 1024).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn status_round_trips_grpc_code_header_value() {
        let status = Status::new(StatusCode::Unavailable, "temporarily unavailable");

        assert_eq!(status.code().as_grpc_code(), 14);
        assert_eq!(
            StatusCode::from_grpc_code(14),
            Some(StatusCode::Unavailable)
        );
        assert_eq!(status.message(), "temporarily unavailable");
    }
}
