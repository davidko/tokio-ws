#[macro_use] extern crate bitflags;
extern crate env_logger;
#[macro_use]
extern crate futures;
extern crate httparse;
#[macro_use] extern crate log;
extern crate openssl;
extern crate tokio_core;
extern crate tokio_proto;
extern crate tokio_service;
extern crate rustc_serialize as serialize;

mod http;
mod wsframe;

use futures::{future, Async, AsyncSink, Future, Poll, Sink, StartSend, Stream};
use openssl::crypto::hash::{self, hash};
use serialize::base64::{ToBase64, STANDARD};
use std::clone::Clone;
use std::collections::VecDeque;
use std::io;
use std::ops::DerefMut;
use std::str;
use tokio_core::io::{Codec, EasyBuf};
use tokio_core::io::{Io, Framed};
use wsframe::WebSocketFrame;

use tokio_proto::pipeline::ServerProto;

static MAGIC_GUID: &'static str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

struct HttpCodec;

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
    type In = WebSocketFrame;
    type Out = WebSocketFrame;

    fn decode(&mut self, buf: &mut EasyBuf) -> io::Result<Option<Self::In>> {
        info!("Decode {} bytes.", buf.len());
        /* Parse WS Frame */
        let len = buf.len();
        let mut buf = buf.drain_to(len);
        let mut mutbuf = buf.get_mut();
        //let result = ss::frame::WebSocketFrameBuilder::from_bytes(mutbuf.deref_mut());
        let result = wsframe::WebSocketFrameBuilder::from_bytes(mutbuf.deref_mut());
        if let Some(boxed_frame) = result {
            info!("WsCodec delivering good frame...");
            return Ok(Some(*boxed_frame));
        } else {
            //return Err(io::Error::new(io::ErrorKind::Other, "Could not parse WS data frame"));
            info!("WsCodec delivering no frame...");
            return Ok(None);
        }
    }

	fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>)
			 -> io::Result<()>
	{
        buf.extend(msg.to_bytes());
        Ok(())
	}
}

pub struct WsProto;

#[derive(Clone)]
pub enum WsPayload {
	Text(String),
	Binary(Vec<u8>),
}

pub struct WsTransport<T> {
    // The upstream framed transport
    upstream: T,
    pongs: VecDeque< WsPayload >, // This should be "Binary" data
}

// Transport wrapper to support control frames, taken from 
// https://github.com/tokio-rs/tokio-line/blob/master/simple/examples/ping_pong.rs
impl<T> Stream for WsTransport<T>
    where T: Stream<Item = WebSocketFrame, Error = io::Error>,
          T: Sink<SinkItem = WebSocketFrame, SinkError = io::Error>,
{
    type Item = WsPayload;
    type Error = io::Error;

    fn poll(&mut self) -> futures::Poll<Option<WsPayload>, io::Error> {
        loop {
            match try_ready!(self.upstream.poll()) {
                Some(ref msg) if msg.op_type() == wsframe::OpType::Ping => {
                    // intercept ping messages
                    let ws_payload = msg.payload();
                    self.pongs.push_back( WsPayload::Binary(ws_payload) );
                    // Try flushing the pong, only bubble up errors
                    try!(self.poll_complete());
                }
                Some(ref msg) => {
                    match msg.op_type() {
                        wsframe::OpType::Binary => {
                            return Ok(Async::Ready(Some(WsPayload::Binary(msg.payload()))));
                        }
                        wsframe::OpType::Text => {
                            return Ok(Async::Ready(Some(WsPayload::Text(String::from_utf8(msg.payload()).unwrap()))));
                        }
                        _ => {
                            return Err( io::Error::new(io::ErrorKind::Other, "Expected Binary or Text WS frame.") );
                        }
                    }
                }
                None => { return Ok(Async::Ready(None)); }
            }
        }
    }
}

impl<T> Sink for WsTransport<T>
    where T: Sink<SinkItem = WebSocketFrame, SinkError = io::Error>,
{
    type SinkItem = WsPayload;
    type SinkError = io::Error;

    fn start_send(&mut self, item: WsPayload) -> StartSend<WsPayload, io::Error> {
        // Only accept the write if there are no pending pongs
        if self.pongs.len() > 0 {
            return Ok(AsyncSink::NotReady(item));
        }

        // If there are no pending pongs, then send the item upstream
        match item {
            WsPayload::Text(string_payload) => {
                let frame = WebSocketFrame::new(
                    string_payload.as_bytes(),
                    wsframe::FrameType::Data,
                    wsframe::OpType::Text);
                self.upstream.start_send(frame).map(move |s| -> AsyncSink<WsPayload> {
                    match s {
                        AsyncSink::Ready => AsyncSink::Ready,
                        AsyncSink::NotReady(_) => AsyncSink::NotReady(WsPayload::Text(string_payload))
                    }
                })
            }
            WsPayload::Binary(bytes_payload) => {
                let frame = WebSocketFrame::new(
                    bytes_payload.as_slice(),
                    wsframe::FrameType::Data,
                    wsframe::OpType::Binary);
                self.upstream.start_send(frame).map(|s| {
                    match s {
                        AsyncSink::Ready => AsyncSink::Ready,
                        AsyncSink::NotReady(_) => AsyncSink::NotReady(WsPayload::Binary(bytes_payload))
                    }
                })
            }
        }
    }

    fn poll_complete(&mut self) -> Poll<(), io::Error> {
        while let Some(payload) = self.pongs.pop_front() {
            // Try to send the pong upstream
            match payload {
                WsPayload::Binary(b) => {
                    let pong = WebSocketFrame::new(b.as_slice(), wsframe::FrameType::Control, wsframe::OpType::Pong);
                    let res = try!(self.upstream.start_send(pong));

                    if !res.is_ready() {
                        // The upstream is not ready to accept new items
                        break;
                    }
                }
                WsPayload::Text(t) => {
                    let pong = WebSocketFrame::new(t.as_bytes(), wsframe::FrameType::Control, wsframe::OpType::Pong);
                    let res = try!(self.upstream.start_send(pong));

                    if !res.is_ready() {
                        // The upstream is not ready to accept new items
                        break;
                    }
                }
            }
        }
        // Call poll_complete on the upstream
        //
        // If there are remaining pongs to send, this call may create additional
        // capacity. One option could be to attempt to send the pongs again.
        // However, if a `start_send` returned NotReady, and this poll_complete
        // *did* create additional capacity in the upstream, then *our*
        // `poll_complete` will get called again shortly.

        self.upstream.poll_complete()
    }
}

    // When created by TcpServer, "T" is "TcpStream"
impl<T: Io + Send + 'static> ServerProto<T> for WsProto {
    /// For this protocol style, `Request` matches the codec `In` type
    type Request = WsPayload;

    /// For this protocol style, `Response` matches the codec `Out` type
    type Response = WsPayload;

    /// A bit of boilerplate to hook in the codec:
    type Transport = WsTransport<Framed<T, WsCodec>>;
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
	            let transport = WsTransport{ upstream: ws_transport, pongs: VecDeque::new() };	
                return Box::new(future::ok(transport)) as Self::BindTransport;
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
