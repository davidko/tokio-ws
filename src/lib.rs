extern crate env_logger;
extern crate futures;
extern crate httparse;
#[macro_use]
extern crate log;
extern crate openssl;
extern crate tokio_core;
extern crate tokio_proto;
extern crate tokio_service;
extern crate rustc_serialize as serialize;
extern crate simple_stream as ss;

mod http;

use futures::{future, Future, Sink, Stream};
use openssl::crypto::hash::{self, hash};
use serialize::base64::{ToBase64, STANDARD};
use ss::frame::Frame;
use ss::frame::FrameBuilder;
//use std::fmt::Write;
use std::io;
use std::ops::DerefMut;
use std::str;
use tokio_core::io::{Codec, EasyBuf};

use tokio_proto::pipeline::ServerProto;

static MAGIC_GUID: &'static str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

pub struct HttpCodec;

impl Codec for HttpCodec {
    type In = http::Request;
    type Out = http::Response;

    fn decode(&mut self, buf: &mut EasyBuf) -> io::Result<Option<Self::In>> {
        info!("Trying to decode HTTP frame...");
        let mut request = http::Request::new();
        match request.parse(buf.as_slice()) {
            Ok(httparse::Status::Complete(_)) => Ok(Some(request)),
            Ok(httparse::Status::Partial) => Ok(None),
            _ => Err(io::Error::new(io::ErrorKind::Other, "Could not parse HTTP request"))
        }
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> io::Result<()> {
        if msg.serialize(buf).is_err() {
            return Err(io::Error::new(io::ErrorKind::Other, "Could not serialize HTTP response."));
        }
        let buf = buf.clone();
        info!("Sending message: {}", String::from_utf8(buf).unwrap());
        Ok(())
    }
}

pub struct WsCodec;

impl WsCodec {
    pub fn new() -> WsCodec {
        WsCodec{}
    }
}

impl Codec for WsCodec {
    type In = Vec<u8>;
    type Out = Vec<u8>;

    fn decode(&mut self, buf: &mut EasyBuf) -> io::Result<Option<Self::In>> {
        info!("Decode {} bytes.", buf.len());
        /* Parse WS Frame */
        let len = buf.len();
        let mut buf = buf.drain_to(len);
        let mut mutbuf = buf.get_mut();
        let result = ss::frame::WebSocketFrameBuilder::from_bytes(mutbuf.deref_mut());
        if let Some(boxed_frame) = result {
            info!("WsCodec delivering good frame...");
            return Ok(Some(boxed_frame.payload()));
        } else {
            //return Err(io::Error::new(io::ErrorKind::Other, "Could not parse WS data frame"));
            info!("WsCodec delivering no frame...");
            return Ok(None);
        }
    }

	fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>)
			 -> io::Result<()>
	{
        let frame = ss::frame::WebSocketFrame::new(
            msg.as_slice(),
            ss::frame::FrameType::Data,
            ss::frame::OpType::Binary
            );
        buf.extend(frame.to_bytes());
        Ok(())
	}
}

pub struct WsProto;

use tokio_core::io::{Io, Framed};

    // When created by TcpServer, "T" is "TcpStream"
impl<T: Io + Send + 'static> ServerProto<T> for WsProto {
    /// For this protocol style, `Request` matches the codec `In` type
    type Request = Vec<u8>;

    /// For this protocol style, `Response` matches the coded `Out` type
    type Response = Vec<u8>;

    /// A bit of boilerplate to hook in the codec:
    type Transport = Framed<T, WsCodec>;
    type BindTransport = Box<Future<Item = Self::Transport, 
                                    Error = io::Error>>;


    fn bind_transport(&self, io: T) -> Self::BindTransport {
        let transport = io.framed(HttpCodec); 

        let handshake = transport.into_future()
            .map_err(|(e, _)| e)
            .and_then(|(frame, transport)| {
                /* This should be an HTTP GET request frame */
                if frame.is_none() {
                    let err = io::Error::new(io::ErrorKind::Other, 
                        "Invalid WebSocket handshake: Expected HTTP frame.");
                    return Box::new(future::err(err)) as Box<Future<Item=_, Error=io::Error>>;
                }
                /* Get the Sec-WebSocket-Key header */
                let frame = frame.unwrap();
                let key = match frame.headers.iter().find(|h| { 
                    info!("Searching for header... {}", h.0);
                    h.0=="Sec-WebSocket-Key" 
                }) {
                    Some(tuple) => tuple.1,
                    None => {
                        info!("Could not find Sec-WebSocket-Key header!");
                        let err = io::Error::new(io::ErrorKind::Other, 
                            "Invalid WebSocket handshake: No Sec-WebSocket-Key header found.");
                        return Box::new(future::err(err));
                    }
                };

                /* Formulate the Server's response:
                   https://tools.ietf.org/html/rfc6455#section-1.3 */

                /* First, formulate the "Sec-WebSocket-Accept" header field */
                let mut concat_key = String::new();
                concat_key.push_str(str::from_utf8(key).unwrap());
                concat_key.push_str(MAGIC_GUID);
                let output = hash(hash::Type::SHA1, concat_key.as_bytes());
                /* Form the HTTP response */
                let mut response = http::Response::new();
                response.code = Some("101".to_string());
                response.reason = Some("Switching Protocols".to_string());
                response.headers.insert("Upgrade".to_string(), b"websocket".to_vec());
                response.headers.insert("Connection".to_string(), b"Upgrade".to_vec());
                response.headers.insert("Sec-WebSocket-Accept".to_string(), output.to_base64(STANDARD).as_bytes().to_vec());
                return Box::new(transport.send(response));
            }).and_then(|transport| {
                let socket = transport.into_inner();
                let ws_transport = socket.framed(WsCodec);
                return Box::new(future::ok(ws_transport)) as Self::BindTransport;
            });

        Box::new(handshake)
        // transport : Framed<Self: TcpStream, C: WsCodec>
        //let err = io::Error::new(io::ErrorKind::Other, "blah");
        //Box::new(future::err(err)) as Self::BindTransport
    }
}


#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
    }
}
