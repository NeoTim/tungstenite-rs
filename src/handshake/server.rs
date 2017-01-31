use bytes::Buf;
use httparse;
use httparse::Status;

use error::{Error, Result};
use super::{Headers, Httparse, FromHttparse, convert_key, MAX_HEADERS};

/// Request from the client.
pub struct Request {
    path: String,
    headers: Headers,
}

impl Request {
    /// Parse the request from a stream.
    pub fn parse<B: Buf>(input: &mut B) -> Result<Option<Self>> {
        Request::parse_http(input)
    }
    /// Reply to the response.
    pub fn reply(&self) -> Result<Vec<u8>> {
        let key = self.headers.find_first("Sec-WebSocket-Key")
            .ok_or(Error::Protocol("Missing Sec-WebSocket-Key".into()))?;
        let reply = format!("\
        HTTP/1.1 101 Switching Protocols\r\n\
        Connection: Upgrade\r\n\
        Upgrade: websocket\r\n\
        Sec-WebSocket-Accept: {}\r\n\
        \r\n", convert_key(key)?);
        Ok(reply.into())
    }
}

impl Httparse for Request {
    fn httparse(buf: &[u8]) -> Result<Option<(usize, Self)>> {
        let mut hbuffer = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut req = httparse::Request::new(&mut hbuffer);
        Ok(match req.parse(buf)? {
            Status::Partial => None,
            Status::Complete(size) => Some((size, Request::from_httparse(req)?)),
        })
    }
}

impl<'h, 'b: 'h> FromHttparse<httparse::Request<'h, 'b>> for Request {
    fn from_httparse(raw: httparse::Request<'h, 'b>) -> Result<Self> {
        if raw.method.expect("Bug: no method in header") != "GET" {
            return Err(Error::Protocol("Method is not GET".into()));
        }
        if raw.version.expect("Bug: no HTTP version") < /*1.*/1 {
            return Err(Error::Protocol("HTTP version should be 1.1 or higher".into()));
        }
        Ok(Request {
            path: raw.path.expect("Bug: no path in header").into(),
            headers: Headers::from_httparse(raw.headers)?
        })
    }
}

#[cfg(test)]
mod tests {

    use super::Request;

    use std::io::Cursor;

    #[test]
    fn request_parsing() {
        const data: &'static [u8] = b"GET /script.ws HTTP/1.1\r\nHost: foo.com\r\n\r\n";
        let mut inp = Cursor::new(data);
        let req = Request::parse(&mut inp).unwrap().unwrap();
        assert_eq!(req.path, "/script.ws");
        assert_eq!(req.headers.find_first("Host"), Some(&b"foo.com"[..]));
    }

    #[test]
    fn request_replying() {
        const data: &'static [u8] = b"\
            GET /script.ws HTTP/1.1\r\n\
            Host: foo.com\r\n\
            Connection: upgrade\r\n\
            Upgrade: websocket\r\n\
            Sec-WebSocket-Version: 13\r\n\
            Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
            \r\n";
        let mut inp = Cursor::new(data);
        let req = Request::parse(&mut inp).unwrap().unwrap();
        let reply = req.reply().unwrap();
    }

}