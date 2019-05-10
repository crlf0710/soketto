//! Codec for dedoding/encoding websocket server handshake frames.

use bytes::BytesMut;
use crate::frame::handshake;
use crate::codec::http::{self, RequestHeaderCodec, ResponseHeaderCodec};
use tokio_io::codec::{Decoder, Encoder};

/// Codec for decoding/encoding websocket server handshake frames.
#[derive(Debug, Default)]
pub struct FrameCodec(());

impl Decoder for FrameCodec {
    type Item = handshake::client::Request;
    type Error = http::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if let Some(req) = RequestHeaderCodec::new().decode(buf)? {
            match handshake::client::Request::new(req) {
                Ok(handshake) => Ok(Some(handshake)),
                Err(invalid) => unimplemented!()
            }
        } else {
            Ok(None)
        }
    }
}

impl Encoder for FrameCodec {
    type Item = handshake::server::Response;
    type Error = http::Error;

    fn encode(&mut self, item: Self::Item, buf: &mut BytesMut) -> Result<(), Self::Error> {
        ResponseHeaderCodec::new().encode(item.as_http(), buf)
    }
}

// #[cfg(test)]
// mod test {
//     use super::FrameCodec;
//
//     #[test]
//     pub fn accept() {
//         let hf: FrameCodec = Default::default();
//         if let Ok(res) = hf.accept_val("dGhlIHNhbXBsZSBub25jZQ==".to_string()) {
//             assert!(res == "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
//         } else {
//             assert!(false);
//         }
//     }
// }
