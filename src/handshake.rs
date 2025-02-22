// Copyright (c) 2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

//! Websocket [handshake] codecs.
//!
//! [handshake]: https://tools.ietf.org/html/rfc6455#section-4

use bytes::BytesMut;
use crate::extension::{Param, Extension};
use http::StatusCode;
use rand::Rng;
use sha1::Sha1;
use smallvec::SmallVec;
use std::{borrow::{Borrow, Cow}, io, fmt, str};
use tokio_codec::{Decoder, Encoder};

const SOKETTO_VERSION: &str = env!("CARGO_PKG_VERSION");

// Handshake codec ////////////////////////////////////////////////////////////////////////////////

// Defined in RFC6455 and used to generate the `Sec-WebSocket-Accept` header
// in the server handshake response.
const KEY: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

// How many HTTP headers do we support during parsing?
const MAX_NUM_HEADERS: usize = 32;

// Some HTTP headers we need to check during parsing.
const SEC_WEBSOCKET_EXTENSIONS: &str = "Sec-WebSocket-Extensions";
const SEC_WEBSOCKET_PROTOCOL: &str = "Sec-WebSocket-Protocol";

// Handshake client (initiator) ///////////////////////////////////////////////////////////////////

/// Client handshake codec.
#[derive(Debug)]
pub struct Client<'a> {
    host: Cow<'a, str>,
    resource: Cow<'a, str>,
    origin: Option<Cow<'a, str>>,
    nonce: String,
    protocols: SmallVec<[Cow<'a, str>; 4]>,
    extensions: SmallVec<[Box<dyn Extension + Send>; 4]>
}

impl<'a> Client<'a> {
    /// Create a new client handshake coded for some host and resource.
    pub fn new<H, R>(host: H, resource: R) -> Self
    where
        H: Into<Cow<'a, str>>,
        R: Into<Cow<'a, str>>
    {
        let mut buf = [0; 16];
        rand::thread_rng().fill(&mut buf);
        let nonce = base64::encode(&buf);
        Client {
            host: host.into(),
            resource: resource.into(),
            origin: None,
            nonce,
            protocols: SmallVec::new(),
            extensions: SmallVec::new()
        }
    }

    /// Get a reference to the nonce created.
    pub fn ws_key(&self) -> &str {
        &self.nonce
    }

    /// Set the handshake origin header.
    pub fn set_origin(&mut self, o: impl Into<Cow<'a, str>>) -> &mut Self {
        self.origin = Some(o.into());
        self
    }

    /// Add a protocol to be included in the handshake.
    pub fn add_protocol(&mut self, p: impl Into<Cow<'a, str>>) -> &mut Self {
        self.protocols.push(p.into());
        self
    }

    /// Add an extension to be included in the handshake.
    pub fn add_extension(&mut self, e: Box<dyn Extension + Send>) -> &mut Self {
        self.extensions.push(e);
        self
    }

    /// Get back all extensions.
    pub fn drain_extensions(&mut self) -> impl Iterator<Item = Box<dyn Extension + Send>> {
        self.extensions.drain()
    }
}

impl<'a> Encoder for Client<'a> {
    type Item = ();
    type Error = Error;

    // Encode client handshake request.
    fn encode(&mut self, _: Self::Item, buf: &mut BytesMut) -> Result<(), Self::Error> {
        buf.extend_from_slice(b"GET ");
        buf.extend_from_slice(self.resource.as_bytes());
        buf.extend_from_slice(b" HTTP/1.1");
        buf.extend_from_slice(b"\r\nHost: ");
        buf.extend_from_slice(self.host.as_bytes());
        buf.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: upgrade");
        buf.extend_from_slice(b"\r\nSec-WebSocket-Key: ");
        buf.extend_from_slice(self.nonce.as_bytes());
        if let Some(o) = &self.origin {
            buf.extend_from_slice(b"\r\nOrigin: ");
            buf.extend_from_slice(o.as_bytes())
        }
        if let Some((last, prefix)) = self.protocols.split_last() {
            buf.extend_from_slice(b"\r\nSec-WebSocket-Protocol: ");
            for p in prefix {
                buf.extend_from_slice(p.as_bytes());
                buf.extend_from_slice(b",")
            }
            buf.extend_from_slice(last.as_bytes())
        }
        append_extensions(&self.extensions, buf);
        buf.extend_from_slice(b"\r\nSec-WebSocket-Version: 13\r\n\r\n");
        Ok(())
    }
}

/// Server handshake response.
#[derive(Debug)]
pub enum Response<'a> {
    Accepted(Accepted<'a>),
    Redirect(Redirect)
}

/// The server accepted the handshake request.
#[derive(Debug)]
pub struct Accepted<'a> {
    /// The protocol (if any) the server has selected.
    protocol: Option<Cow<'a, str>>,
}

impl<'a> Accepted<'a> {
    /// The protocol the server has selected from the proposed ones.
    pub fn protocol(&self) -> Option<&str> {
        self.protocol.as_ref().map(|p| p.as_ref())
    }
}

/// The server is redirecting us to another location.
#[derive(Debug)]
pub struct Redirect {
    /// The HTTP response status code.
    status_code: u16,
    /// The location URL we should go to.
    location: String,
}

impl fmt::Display for Redirect {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "redirect: code = {}, location = \"{}\"", self.status_code, self.location)
    }
}

impl Redirect {
    /// The HTTP response status code.
    pub fn status_code(&self) -> u16 {
        self.status_code
    }

    /// The HTTP response location header.
    pub fn location(&self) -> &str {
        &self.location
    }
}

impl<'a> Decoder for Client<'a> {
    type Item = Response<'a>;
    type Error = Error;

    // Decode server handshake response.
    fn decode(&mut self, bytes: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut header_buf = [httparse::EMPTY_HEADER; MAX_NUM_HEADERS];
        let mut response = httparse::Response::new(&mut header_buf);

        let offset = match response.parse(bytes) {
            Ok(httparse::Status::Complete(off)) => off,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(e) => return Err(Error::Http(Box::new(e)))
        };

        if response.version != Some(1) {
            return Err(Error::UnsupportedHttpVersion)
        }

        match response.code {
            Some(101) => (),
            Some(code@(301 ..= 303)) | Some(code@307) | Some(code@308) => { // redirect response
                let location = with_first_header(&response.headers, "Location", |loc| {
                    Ok(String::from(std::str::from_utf8(loc)?))
                })?;
                bytes.split_to(offset); // chop off the HTTP part we have processed
                let response = Redirect { status_code: code, location };
                return Ok(Some(Response::Redirect(response)))
            }
            other => return Err(Error::UnexpectedStatusCode(other.unwrap_or(0)))
        }

        expect_ascii_header(response.headers, "Upgrade", "websocket")?;
        expect_ascii_header(response.headers, "Connection", "upgrade")?;

        let nonce: &str = self.nonce.borrow();
        with_first_header(&response.headers, "Sec-WebSocket-Accept", |theirs| {
            let mut digest = Sha1::new();
            digest.update(nonce.as_bytes());
            digest.update(KEY);
            let ours = base64::encode(&digest.digest().bytes());
            if ours.as_bytes() != theirs {
                return Err(Error::InvalidSecWebSocketAccept)
            }
            Ok(())
        })?;

        // Parse `Sec-WebSocket-Extensions` headers.

        for h in response.headers.iter().filter(|h| h.name.eq_ignore_ascii_case(SEC_WEBSOCKET_EXTENSIONS)) {
            configure_extensions(&mut self.extensions, std::str::from_utf8(h.value)?)?
        }

        // Match `Sec-WebSocket-Protocol` header.

        let their_proto = response.headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case(SEC_WEBSOCKET_PROTOCOL));

        let mut selected_proto = None;

        if let Some(tp) = their_proto {
            if let Some(p) = self.protocols.iter().find(|x| x.as_bytes() == tp.value) {
                selected_proto = Some(p.clone())
            } else {
                return Err(Error::UnsolicitedProtocol)
            }
        }

        bytes.split_to(offset); // chop off the HTTP part we have processed

        let response = Accepted { protocol: selected_proto };
        Ok(Some(Response::Accepted(response)))
    }
}

// Handshake server (responder) ///////////////////////////////////////////////////////////////////

/// Server handshake codec.
#[derive(Debug, Default)]
pub struct Server<'a> {
    protocols: SmallVec<[Cow<'a, str>; 4]>,
    extensions: SmallVec<[Box<dyn Extension + Send>; 4]>
}

impl<'a> Server<'a> {
    /// Create a new server handshake codec.
    pub fn new() -> Self {
        Server::default()
    }

    /// Add a protocol the server supports.
    pub fn add_protocol(&mut self, p: impl Into<Cow<'a, str>>) -> &mut Self {
        self.protocols.push(p.into());
        self
    }

    /// Add an extension the server supports.
    pub fn add_extension(&mut self, e: Box<dyn Extension + Send>) -> &mut Self {
        self.extensions.push(e);
        self
    }

    /// Get back all extensions.
    pub fn drain_extensions(&mut self) -> impl Iterator<Item = Box<dyn Extension + Send>> {
        self.extensions.drain()
    }
}

/// Client handshake request.
#[derive(Debug)]
pub struct Request<'a> {
    ws_key: SmallVec<[u8; 32]>,
    protocols: SmallVec<[Cow<'a, str>; 4]>
}

impl<'a> Request<'a> {
    /// A reference to the nonce.
    pub fn key(&self) -> &[u8] {
        &self.ws_key
    }

    /// The protocols the client is proposing.
    pub fn protocols(&self) -> impl Iterator<Item = &str> {
        self.protocols.iter().map(|p| p.as_ref())
    }
}

impl<'a> Decoder for Server<'a> {
    type Item = Request<'a>;
    type Error = Error;

    // Decode client request.
    fn decode(&mut self, bytes: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let mut header_buf = [httparse::EMPTY_HEADER; MAX_NUM_HEADERS];
        let mut request = httparse::Request::new(&mut header_buf);

        let offset = match request.parse(bytes) {
            Ok(httparse::Status::Complete(off)) => off,
            Ok(httparse::Status::Partial) => return Ok(None),
            Err(e) => return Err(Error::Http(Box::new(e)))
        };

        if request.method != Some("GET") {
            return Err(Error::InvalidRequestMethod)
        }
        if request.version != Some(1) {
            return Err(Error::UnsupportedHttpVersion)
        }

        // TODO: Host Validation
        with_first_header(&request.headers, "Host", |_h| Ok(()))?;

        expect_ascii_header(request.headers, "Upgrade", "websocket")?;
        expect_ascii_header(request.headers, "Connection", "upgrade")?;
        expect_ascii_header(request.headers, "Sec-WebSocket-Version", "13")?;

        let ws_key = with_first_header(&request.headers, "Sec-WebSocket-Key", |k| {
            Ok(SmallVec::from(k))
        })?;

        for h in request.headers.iter().filter(|h| h.name.eq_ignore_ascii_case(SEC_WEBSOCKET_EXTENSIONS)) {
            configure_extensions(&mut self.extensions, std::str::from_utf8(h.value)?)?
        }

        let mut protocols = SmallVec::new();
        for p in request.headers.iter().filter(|h| h.name.eq_ignore_ascii_case(SEC_WEBSOCKET_PROTOCOL)) {
            if let Some(x) = self.protocols.iter().find(|x| x.as_bytes() == p.value) {
                protocols.push(x.clone())
            }
        }

        bytes.split_to(offset); // chop off the HTTP part we have processed

        Ok(Some(Request { ws_key, protocols }))
    }
}

/// Successful handshake response the server wants to send to the client.
#[derive(Debug)]
pub struct Accept<'a> {
    key: Cow<'a, [u8]>,
    protocol: Option<Cow<'a, str>>
}

impl<'a> Accept<'a> {
    /// Create a new accept response.
    ///
    /// The `key` corresponds to the websocket key (nonce) the client has
    /// sent in its handshake request.
    pub fn new(key: impl Into<Cow<'a, [u8]>>) -> Self {
        Accept {
            key: key.into(),
            protocol: None
        }
    }

    /// Set the protocol the server selected from the proposed ones.
    pub fn set_protocol(&mut self, p: impl Into<Cow<'a, str>>) -> &mut Self {
        self.protocol = Some(p.into());
        self
    }
}

/// Error handshake response the server wants to send to the client.
#[derive(Debug)]
pub struct Reject {
    /// HTTP response status code.
    code: u16
}

impl Reject {
    /// Create a new reject response with the given HTTP status code.
    pub fn new(code: u16) -> Self {
        Reject { code }
    }
}

impl<'a> Encoder for Server<'a> {
    type Item = Result<Accept<'a>, Reject>;
    type Error = Error;

    // Encode server handshake response.
    fn encode(&mut self, answer: Self::Item, buf: &mut BytesMut) -> Result<(), Self::Error> {
        match answer {
            Ok(accept) => {
                let mut key_buf = [0; 32];
                let accept_value = {
                    let mut digest = Sha1::new();
                    digest.update(accept.key.borrow());
                    digest.update(KEY);
                    let d = digest.digest().bytes();
                    let n = base64::encode_config_slice(&d, base64::STANDARD, &mut key_buf);
                    &key_buf[.. n]
                };
                buf.extend_from_slice(b"HTTP/1.1 101 Switching Protocols");
                buf.extend_from_slice(b"\r\nServer: soketto-");
                buf.extend_from_slice(SOKETTO_VERSION.as_bytes());
                buf.extend_from_slice(b"\r\nUpgrade: websocket\r\nConnection: upgrade");
                buf.extend_from_slice(b"\r\nSec-WebSocket-Accept: ");
                buf.extend_from_slice(accept_value);
                if let Some(p) = accept.protocol {
                    buf.extend_from_slice(b"\r\nSec-WebSocket-Protocol: ");
                    buf.extend_from_slice(p.as_bytes())
                }
                append_extensions(self.extensions.iter().filter(|e| e.is_enabled()), buf);
                buf.extend_from_slice(b"\r\n\r\n")
            }
            Err(reject) => {
                buf.extend_from_slice(b"HTTP/1.1 ");
                let s = StatusCode::from_u16(reject.code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
                buf.extend_from_slice(s.as_str().as_bytes());
                buf.extend_from_slice(b" ");
                buf.extend_from_slice(s.canonical_reason().unwrap_or("N/A").as_bytes());
                buf.extend_from_slice(b"\r\n\r\n")
            }
        }
        Ok(())
    }
}

/// Check a set of headers contains a specific one.
fn expect_ascii_header(headers: &[httparse::Header], name: &str, ours: &str) -> Result<(), Error> {
    enum State {
        Init, // Start state
        Name, // Header name found
        Match // Header value matches
    }

    headers.iter()
        .filter(|h| h.name.eq_ignore_ascii_case(name))
        .fold(Ok(State::Init), |result, header| {
            if let Ok(State::Match) = result {
                return result
            }
            if str::from_utf8(header.value)?
                .split(',')
                .find(|v| v.trim().eq_ignore_ascii_case(ours))
                .is_some()
            {
                return Ok(State::Match)
            }
            Ok(State::Name)
        })
        .and_then(|state| {
            match state {
                State::Init => Err(Error::HeaderNotFound(name.into())),
                State::Name => Err(Error::UnexpectedHeader(name.into())),
                State::Match => Ok(())
            }
        })
}

/// Pick the first header with the given name and apply the given closure to it.
fn with_first_header<F, R>(headers: &[httparse::Header], name: &str, f: F) -> Result<R, Error>
where
    F: Fn(&[u8]) -> Result<R, Error>
{
    if let Some(h) = headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)) {
        f(h.value)
    } else {
        Err(Error::HeaderNotFound(name.into()))
    }
}

// Configure all extensions with parsed parameters.
fn configure_extensions(extensions: &mut [Box<dyn Extension + Send>], line: &str) -> Result<(), Error> {
    for e in line.split(',') {
        let mut ext_parts = e.split(';');
        if let Some(name) = ext_parts.next() {
            let name = name.trim();
            if let Some(ext) = extensions.iter_mut().find(|x| x.name().eq_ignore_ascii_case(name)) {
                let mut params = SmallVec::<[Param; 4]>::new();
                for p in ext_parts {
                    let mut key_value = p.split('=');
                    if let Some(key) = key_value.next().map(str::trim) {
                        let val = key_value.next().map(|v| v.trim().trim_matches('"'));
                        let mut p = Param::new(key);
                        p.set_value(val);
                        params.push(p)
                    }
                }
                ext.configure(&params).map_err(Error::Extension)?
            }
        }
    }
    Ok(())
}

// Write all extensions to the given buffer.
fn append_extensions<'a, I>(extensions: I, buf: &mut BytesMut)
where
    I: IntoIterator<Item = &'a Box<dyn Extension + Send>>
{
    let mut iter = extensions.into_iter().peekable();

    if iter.peek().is_some() {
        buf.extend_from_slice(b"\r\nSec-WebSocket-Extensions: ")
    }

    while let Some(e) = iter.next() {
        buf.extend_from_slice(e.name().as_bytes());
        for p in e.params() {
            buf.extend_from_slice(b"; ");
            buf.extend_from_slice(p.name().as_bytes());
            if let Some(v) = p.value() {
                buf.extend_from_slice(b"=");
                buf.extend_from_slice(v.as_bytes())
            }
        }
        if iter.peek().is_some() {
            buf.extend_from_slice(b", ")
        }
    }
}

// Codec error type ///////////////////////////////////////////////////////////////////////////////

/// Enumeration of possible handshake errors.
#[derive(Debug)]
pub enum Error {
    /// An I/O error has been encountered.
    Io(io::Error),
    /// An HTTP version =/= 1.1 was encountered.
    UnsupportedHttpVersion,
    /// The handshake request was not a GET request.
    InvalidRequestMethod,
    /// The HTTP response code was unexpected.
    UnexpectedStatusCode(u16),
    /// An HTTP header has not been present.
    HeaderNotFound(String),
    /// An HTTP header value was not expected.
    UnexpectedHeader(String),
    /// The Sec-WebSocket-Accept header value did not match.
    InvalidSecWebSocketAccept,
    /// The server returned an extension we did not ask for.
    UnsolicitedExtension,
    /// The server returned a protocol we did not ask for.
    UnsolicitedProtocol,
    /// An extension produced an error while encoding or decoding.
    Extension(crate::BoxError),
    /// The HTTP entity could not be parsed successfully.
    Http(crate::BoxError),
    /// UTF-8 decoding failed.
    Utf8(std::str::Utf8Error),

    #[doc(hidden)]
    __Nonexhaustive
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "i/o error: {}", e),
            Error::Http(e) => write!(f, "http parser error: {}", e),
            Error::HeaderNotFound(n) => write!(f, "header {} not found", n),
            Error::UnexpectedHeader(n) => write!(f, "header {} had unexpected value", n),
            Error::Utf8(e) => write!(f, "utf-8 decoding error: {}", e),
            Error::UnexpectedStatusCode(c) => write!(f, "unexpected response status: {}", c),
            Error::Extension(e) => write!(f, "extension error: {}", e),
            Error::UnsupportedHttpVersion => f.write_str("http version was not 1.1"),
            Error::InvalidRequestMethod => f.write_str("handshake not a GET request"),
            Error::InvalidSecWebSocketAccept => f.write_str("websocket key mismatch"),
            Error::UnsolicitedExtension => f.write_str("unsolicited extension returned"),
            Error::UnsolicitedProtocol => f.write_str("unsolicited protocol returned"),
            Error::__Nonexhaustive => f.write_str("__Nonexhaustive")
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Utf8(e) => Some(e),
            Error::Http(e) => Some(&**e),
            Error::Extension(e) => Some(&**e),
            Error::HeaderNotFound(_)
            | Error::UnexpectedHeader(_)
            | Error::UnexpectedStatusCode(_)
            | Error::UnsupportedHttpVersion
            | Error::InvalidRequestMethod
            | Error::InvalidSecWebSocketAccept
            | Error::UnsolicitedExtension
            | Error::UnsolicitedProtocol
            | Error::__Nonexhaustive => None
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<str::Utf8Error> for Error {
    fn from(e: str::Utf8Error) -> Self {
        Error::Utf8(e)
    }
}

#[cfg(test)]
mod tests {
    use super::expect_ascii_header;

    #[test]
    fn header_match() {
        let headers = &[
            httparse::Header { name: "foo", value: b"a,b,c,d" },
            httparse::Header { name: "foo", value: b"x" },
            httparse::Header { name: "foo", value: b"y, z, a" },
            httparse::Header { name: "bar", value: b"xxx" },
            httparse::Header { name: "bar", value: b"sdfsdf 423 42 424" },
            httparse::Header { name: "baz", value: b"123" }
        ];

        assert!(expect_ascii_header(headers, "foo", "a").is_ok());
        assert!(expect_ascii_header(headers, "foo", "b").is_ok());
        assert!(expect_ascii_header(headers, "foo", "c").is_ok());
        assert!(expect_ascii_header(headers, "foo", "d").is_ok());
        assert!(expect_ascii_header(headers, "foo", "x").is_ok());
        assert!(expect_ascii_header(headers, "foo", "y").is_ok());
        assert!(expect_ascii_header(headers, "foo", "z").is_ok());
        assert!(expect_ascii_header(headers, "foo", "a").is_ok());
        assert!(expect_ascii_header(headers, "bar", "xxx").is_ok());
        assert!(expect_ascii_header(headers, "bar", "sdfsdf 423 42 424").is_ok());
        assert!(expect_ascii_header(headers, "baz", "123").is_ok());
        assert!(expect_ascii_header(headers, "baz", "???").is_err());
        assert!(expect_ascii_header(headers, "???", "x").is_err());
    }
}
