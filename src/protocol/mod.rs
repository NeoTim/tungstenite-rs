//! Generic WebSocket protocol implementation

mod frame;
mod message;

pub use self::message::Message;

use self::message::{IncompleteMessage, IncompleteMessageType};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::mem::replace;

use error::{Error, Result};
use self::frame::{Frame, FrameSocket};
use self::frame::coding::{OpCode, Data as OpData, Control as OpCtl, CloseCode};

/// Indicates a Client or Server role of the websocket
#[derive(Debug, Clone, Copy)]
pub enum Role {
    /// This socket is a server
    Server,
    /// This socket is a client
    Client,
}

/// WebSocket input-output stream
pub struct WebSocket<Stream> {
    /// Server or client?
    role: Role,
    /// The underlying socket.
    socket: FrameSocket<Stream>,
    /// The state of processing, either "active" or "closing".
    state: WebSocketState,
    /// Receive: an incomplete message being processed.
    incomplete: Option<IncompleteMessage>,
    /// Send: a data send queue.
    send_queue: VecDeque<Frame>,
    /// Send: an OOB pong message.
    pong: Option<Frame>,
}

impl<Stream> WebSocket<Stream>
    where Stream: Read + Write
{

    /// Convert a raw socket into a WebSocket without performing a handshake.
    pub fn from_raw_socket(stream: Stream, role: Role) -> Self {
        WebSocket::from_frame_socket(FrameSocket::new(stream), role)
    }

    /// Convert a raw socket into a WebSocket without performing a handshake.
    pub fn from_partially_read(stream: Stream, part: Vec<u8>, role: Role) -> Self {
        WebSocket::from_frame_socket(FrameSocket::from_partially_read(stream, part), role)
    }

    /// Read a message from stream, if possible.
    pub fn read_message(&mut self) -> Result<Message> {
        loop {
            self.send_pending()?; // FIXME
            if let Some(message) = self.read_message_frame()? {
                debug!("Received message {}", message);
                return Ok(message)
            }
        }
    }

    /// Send a message to stream, if possible.
    pub fn write_message(&mut self, message: Message) -> Result<()> {
        let frame = {
            let opcode = match message {
                Message::Text(_) => OpData::Text,
                Message::Binary(_) => OpData::Binary,
            };
            Frame::message(message.into_data(), OpCode::Data(opcode), true)
        };
        self.send_queue.push_back(frame);
        self.send_pending()
    }

    /// Close the connection.
    pub fn close(&mut self) -> Result<()> {
        match self.state {
            WebSocketState::Active => {
                self.state = WebSocketState::ClosedByUs;
                // TODO
            }
            _ => {
                // already closed, nothing to do
            }
        }
        Ok(())
    }

    /// Convert a frame socket into a WebSocket.
    fn from_frame_socket(socket: FrameSocket<Stream>, role: Role) -> Self {
        WebSocket {
            role: role,
            socket: socket,
            state: WebSocketState::Active,
            incomplete: None,
            send_queue: VecDeque::new(),
            pong: None,
        }
    }

    /// Try to decode one message frame. May return None.
    fn read_message_frame(&mut self) -> Result<Option<Message>> {
        if let Some(mut frame) = self.socket.read_frame()? {

            // MUST be 0 unless an extension is negotiated that defines meanings
            // for non-zero values.  If a nonzero value is received and none of
            // the negotiated extensions defines the meaning of such a nonzero
            // value, the receiving endpoint MUST _Fail the WebSocket
            // Connection_.
            if frame.has_rsv1() || frame.has_rsv2() || frame.has_rsv3() {
                return Err(Error::Protocol("Reserved bits are non-zero".into()))
            }

            match self.role {
                Role::Server => {
                    if frame.is_masked() {
                        // A server MUST remove masking for data frames received from a client
                        // as described in Section 5.3. (RFC 6455)
                        frame.remove_mask()
                    } else {
                        // The server MUST close the connection upon receiving a
                        // frame that is not masked. (RFC 6455)
                        return Err(Error::Protocol("Received an unmasked frame from client".into()))
                    }
                }
                Role::Client => {
                    if frame.is_masked() {
                        // A client MUST close a connection if it detects a masked frame. (RFC 6455)
                        return Err(Error::Protocol("Received a masked frame from server".into()))
                    }
                }
            }

            match frame.opcode() {

                OpCode::Control(ctl) => {
                    (match ctl {
                        // All control frames MUST have a payload length of 125 bytes or less
                        // and MUST NOT be fragmented. (RFC 6455)
                        _ if !frame.is_final() => {
                            Err(Error::Protocol("Fragmented control frame".into()))
                        }
                        _ if frame.payload().len() > 125 => {
                            Err(Error::Protocol("Control frame too big".into()))
                        }
                        OpCtl::Close => {
                            self.do_close(frame.into_close()?)
                        }
                        OpCtl::Reserved(i) => {
                            Err(Error::Protocol(format!("Unknown control frame type {}", i).into()))
                        }
                        OpCtl::Ping | OpCtl::Pong if !self.state.is_active() => {
                            // No ping processing while closing.
                            Ok(())
                        }
                        OpCtl::Ping => {
                            self.do_ping(frame.into_data())
                        }
                        OpCtl::Pong => {
                            self.do_pong(frame.into_data())
                        }
                    }).map(|_| None)
                }

                OpCode::Data(_) if !self.state.is_active() => {
                    // No data processing while closing.
                    Ok(None)
                }

                OpCode::Data(data) => {
                    let fin = frame.is_final();
                    match data {
                        OpData::Continue => {
                            if let Some(ref mut msg) = self.incomplete {
                                // TODO if msg too big
                                msg.extend(frame.into_data())?;
                            } else {
                                return Err(Error::Protocol("Continue frame but nothing to continue".into()))
                            }
                            if fin {
                                Ok(Some(replace(&mut self.incomplete, None).unwrap().complete()?))
                            } else {
                                Ok(None)
                            }
                        }
                        c if self.incomplete.is_some() => {
                            Err(Error::Protocol(
                                format!("Received {} while waiting for more fragments", c).into()
                            ))
                        }
                        OpData::Text | OpData::Binary => {
                            let msg = {
                                let message_type = match data {
                                    OpData::Text => IncompleteMessageType::Text,
                                    OpData::Binary => IncompleteMessageType::Binary,
                                    _ => panic!("Bug: message is not text nor binary"),
                                };
                                let mut m = IncompleteMessage::new(message_type);
                                m.extend(frame.into_data())?;
                                m
                            };
                            if fin {
                                Ok(Some(msg.complete()?))
                            } else {
                                self.incomplete = Some(msg);
                                Ok(None)
                            }
                        }
                        OpData::Reserved(i) => {
                            Err(Error::Protocol(format!("Unknown data frame type {}", i).into()))
                        }
                    }
                }

            } // match opcode

        } else {
            //Ok(None) // TODO handle EOF?
            Err(Error::Protocol("Connection reset without closing handshake".into()))
        }
    }

    /// Received a close frame.
    fn do_close(&mut self, close: Option<(CloseCode, String)>) -> Result<()> {
        match self.state {
            WebSocketState::Active => {
                self.state = WebSocketState::ClosedByPeer;
                let reply = if let Some((code, _)) = close {
                    if code.is_allowed() {
                        Frame::close(Some((CloseCode::Normal, "")))
                    } else {
                        Frame::close(Some((CloseCode::Protocol, "Protocol violation")))
                    }
                } else {
                    Frame::close(None)
                };
                self.send_queue.push_back(reply);
            }
            WebSocketState::ClosedByPeer => {
                // It is already closed, just ignore.
            }
            WebSocketState::ClosedByUs => {
                // We received a reply.
                match self.role {
                    Role::Client => {
                        // Client waits for the server to close the connection.
                    }
                    Role::Server => {
                        // Server closes the connection.
                        // TODO
                    }
                }
            }
        }
        //unimplemented!()
        Ok(())
    }

    /// Received a ping frame.
    fn do_ping(&mut self, ping: Vec<u8>) -> Result<()> {
        // If an endpoint receives a Ping frame and has not yet sent Pong
        // frame(s) in response to previous Ping frame(s), the endpoint MAY
        // elect to send a Pong frame for only the most recently processed Ping
        // frame. (RFC 6455)
        // We do exactly that, keeping a "queue" from one and only Pong frame.
        self.pong = Some(Frame::pong(ping));
        Ok(())
    }

    /// Received a pong frame.
    fn do_pong(&mut self, _: Vec<u8>) -> Result<()> {
        // A Pong frame MAY be sent unsolicited.  This serves as a
        // unidirectional heartbeat.  A response to an unsolicited Pong frame is
        // not expected. (RFC 6455)
        // Due to this, we just don't check pongs right now.
        // TODO: check if there was a reply to our ping at all...
        Ok(())
    }

    /// Flush the pending send queue.
    fn send_pending(&mut self) -> Result<()> {
        // Upon receipt of a Ping frame, an endpoint MUST send a Pong frame in
        // response, unless it already received a Close frame. It SHOULD
        // respond with Pong frame as soon as is practical. (RFC 6455)
        if let Some(pong) = replace(&mut self.pong, None) {
            self.send_one_frame(pong)?;
        }
        // If we have any unsent frames, send them.
        while let Some(data) = self.send_queue.pop_front() {
            self.send_one_frame(data)?;
        }
        Ok(())
    }

    /// Send a single pending frame.
    fn send_one_frame(&mut self, mut frame: Frame) -> Result<()> {
        match self.role {
            Role::Server => {
            }
            Role::Client => {
                // 5.  If the data is being sent by the client, the frame(s) MUST be
                // masked as defined in Section 5.3. (RFC 6455)
                frame.set_mask();
            }
        }
        self.socket.write_frame(frame)?;
        Ok(())
    }

}

/// The current connection state.
enum WebSocketState {
    Active,
    ClosedByUs,
    ClosedByPeer,
}

impl WebSocketState {
    /// Tell if we're allowed to process normal messages.
    fn is_active(&self) -> bool {
        match *self {
            WebSocketState::Active => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{WebSocket, Role, Message};

    use std::io;
    use std::io::Cursor;

    struct WriteMoc<Stream>(Stream);

    impl<Stream> io::Write for WriteMoc<Stream> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<Stream: io::Read> io::Read for WriteMoc<Stream> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.0.read(buf)
        }
    }


    #[test]
    fn receive_messages() {
        let incoming = Cursor::new(vec![
            0x01, 0x07,
            0x48, 0x65, 0x6c, 0x6c, 0x6f, 0x2c, 0x20,
            0x80, 0x06,
            0x57, 0x6f, 0x72, 0x6c, 0x64, 0x21,
            0x82, 0x03,
            0x01, 0x02, 0x03,
        ]);
        let mut socket = WebSocket::from_raw_socket(WriteMoc(incoming), Role::Client);
        assert_eq!(socket.read_message().unwrap(), Message::Text("Hello, World!".into()));
        assert_eq!(socket.read_message().unwrap(), Message::Binary(vec![0x01, 0x02, 0x03]));
    }

}