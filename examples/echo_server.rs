
extern crate env_logger;
extern crate futures;
#[macro_use]
extern crate log;
extern crate tokio_proto;
extern crate tokio_service;
extern crate tokio_ws;

use std::io;
use tokio_proto::TcpServer;
use tokio_service::Service;

pub struct Echo;

use futures::{future, Future, BoxFuture};

impl Service for Echo {
    // These types must match the corresponding protocol types:
    type Request = tokio_ws::WsPayload;
    type Response = tokio_ws::WsPayload;

    // For non-streaming protocols, service errors are always io::Error
    type Error = io::Error;

    // The future for computing the response; box it for simplicity.
    type Future = BoxFuture<Self::Response, Self::Error>;

    // Produce a future for computing a response from a request.
    fn call(&self, req: Self::Request) -> Self::Future {
        future::ok(req).boxed()
    }
}


fn main() {
    env_logger::init().unwrap();
    println!("Server starting...");
    // Specify the localhost address
    let addr = "0.0.0.0:12345".parse().unwrap();

    // The builder requires a protocol and an address
     let server = TcpServer::new(tokio_ws::WsProto, addr);

    // We provide a way to *instantiate* the service for each new
    // connection; here, we just immediately return a new instance.
     server.serve(|| Ok(Echo));
}


