//! Websockets Filters

use std::fmt;
use std::str::FromStr;

use base64;
use futures::{ Async, AsyncSink, Future, Poll, Sink, StartSend, Stream};
use http;
use http::header::HeaderValue;
use sha1::{Digest, Sha1};
use tungstenite::protocol;
use tokio_tungstenite::WebSocketStream;

use ::error::Kind;
use ::filter::{Filter, FilterClone, One};
use ::reject::{Rejection};
use ::reply::{ReplySealed, Response};
use super::{body, header};

/// Creates a Websocket Filter.
///
/// The passed function is called with each successful Websocket accepted.
///
/// # Note
///
/// This filter combines multiple filters internally, so you don't need them:
///
/// - Method must be `GET`
/// - Header `connection` must be `upgrade`
/// - Header `upgrade` must be `websocket`
/// - Header `sec-websocket-version` must be `13`
/// - Header `sec-websocket-key` must be set.
///
/// If the filters are met, yields a `Ws` which will reply with:
///
/// - Status of `101 Switching Protocols`
/// - Header `connection: upgrade`
/// - Header `upgrade: websocket`
/// - Header `sec-websocket-accept` with the hash value of the received key.
pub fn ws<F, U>(fun: F) -> impl FilterClone<Extract=One<Ws>, Error=Rejection>
where
    F: Fn(WebSocket) -> U + Clone + Send + 'static,
    U: Future<Item=(), Error=()> + Send + 'static,
{
    ws_new(move || {
        let fun = fun.clone();
        move |sock| {
            let fut = fun(sock);
            ::hyper::rt::spawn(fut);
        }
    })
}

/// Creates a Websocket Filter, with a supplied factory function.
///
/// The factory function is called once for each accepted `WebSocket`. The
/// factory should return a new function that is ready to handle the
/// `WebSocket`.
fn ws_new<F1, F2>(factory: F1) -> impl FilterClone<Extract=One<Ws>, Error=Rejection>
where
    F1: Fn() -> F2 + Clone + Send + 'static,
    F2: Fn(WebSocket) + Send + 'static,
{
    ::get(header::if_value("connection", connection_has_upgrade)
        .and(header::exact_ignore_case("upgrade", "websocket"))
        .and(header::exact("sec-websocket-version", "13"))
        .and(header::header::<Accept>("sec-websocket-key"))
        .and(body::body())
        .map(move |accept: Accept, body: ::hyper::Body| {
            let fun = factory();
            let fut = body.on_upgrade()
                .map(move |upgraded| {
                    trace!("websocket upgrade complete");

                    let io = WebSocketStream::from_raw_socket(upgraded, protocol::Role::Server);

                    fun(WebSocket {
                        inner: io,
                    });
                })
                .map_err(|err| debug!("ws upgrade error: {}", err));
            ::hyper::rt::spawn(fut);

            Ws {
                accept,
            }
        }))
}

fn connection_has_upgrade(value: &HeaderValue) -> Option<()> {
    trace!("header connection has upgrade? value={:?}", value);

    value
        .to_str()
        .ok()
        .and_then(|s| {
            for opt in s.split(", ") {
                if opt.eq_ignore_ascii_case("upgrade") {
                    return Some(());
                }
            }
            None
        })
}

/// A [`Reply`](::Reply) that returns the websocket handshake response.
pub struct Ws {
    accept: Accept,
}

impl ReplySealed for Ws {
    fn into_response(self) -> Response {
        http::Response::builder()
            .status(101)
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-accept", self.accept.0.as_str())
            .body(Default::default())
            .unwrap()
    }
}

impl fmt::Debug for Ws {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Ws")
            .finish()
    }
}

/// A websocket `Stream` and `Sink`, provided to `ws` filters.
pub struct WebSocket {
    inner: WebSocketStream<::hyper::upgrade::Upgraded>,
}

impl Stream for WebSocket {
    type Item = Message;
    type Error = ::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        loop {
            let msg = match self.inner.poll() {
                Ok(Async::Ready(Some(item))) => item,
                Ok(Async::Ready(None)) => return Ok(Async::Ready(None)),
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Err(::tungstenite::Error::ConnectionClosed(frame)) => {
                    trace!("websocket closed: {:?}", frame);
                    return Ok(Async::Ready(None));
                },
                Err(e) => {
                    debug!("websocket poll error: {}", e);
                    return Err(Kind::Ws(e).into());
                }
            };

            match msg {
                msg @ protocol::Message::Text(..) |
                msg @ protocol::Message::Binary(..) => {
                    return Ok(Async::Ready(Some(Message {
                        inner: msg,
                    })));
                },
                protocol::Message::Ping(payload) => {
                    trace!("websocket client ping: {:?}", payload);
                    // Pings are just suggestions, so *try* to send a pong back,
                    // but if we're backed up, no need to do any fancy buffering
                    // or anything.
                    let _ = self.inner.start_send(protocol::Message::Pong(payload));
                }
                protocol::Message::Pong(payload) => {
                    trace!("websocket client pong: {:?}", payload);
                }
            }
        }
    }
}

impl Sink for WebSocket {
    type SinkItem = Message;
    type SinkError = ::Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        match self.inner.start_send(item.inner) {
            Ok(AsyncSink::Ready) => Ok(AsyncSink::Ready),
            Ok(AsyncSink::NotReady(inner)) => Ok(AsyncSink::NotReady(Message {
                inner,
            })),
            Err(e) => {
                debug!("websocket start_send error: {}", e);
                Err(Kind::Ws(e).into())
            }
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.poll_complete()
            .map_err(|e| {
                debug!("websocket poll_complete error: {}", e);
                Kind::Ws(e).into()
            })
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        self.inner.close()
            .map_err(|e| {
                debug!("websocket close error: {}", e);
                Kind::Ws(e).into()
            })
    }
}

impl fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("WebSocket")
            .finish()
    }
}

/// A WebSocket message.
///
/// Only repesents Text and Binary messages.
///
/// This will likely become a `non-exhaustive` enum in the future, once that
/// language feature has stabilized.
#[derive(Debug)]
pub struct Message {
    inner: protocol::Message,
}

impl Message {
    /// Construct a new Text `Message`.
    pub fn text<S: Into<String>>(s: S) -> Message {
        Message {
            inner: protocol::Message::text(s),
        }
    }

    /// Construct a new Binary `Message`.
    pub fn binary<V: Into<Vec<u8>>>(v: V) -> Message {
        Message {
            inner: protocol::Message::binary(v),
        }
    }

    /// Returns true if this message is a Text message.
    pub fn is_text(&self) -> bool {
        self.inner.is_text()
    }

    /// Returns true if this message is a Binary message.
    pub fn is_binary(&self) -> bool {
        self.inner.is_binary()
    }

    /// Try to get a reference to the string text, if this is a Text message.
    pub fn to_str(&self) -> Result<&str, ()> {
        match self.inner {
            protocol::Message::Text(ref s) => Ok(s),
            _ => Err(())
        }
    }

    /// Return the bytes of this message.
    pub fn as_bytes(&self) -> &[u8] {
        match self.inner {
            protocol::Message::Text(ref s) => s.as_bytes(),
            protocol::Message::Binary(ref v) => v,
            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
struct Accept(String);

impl FromStr for Accept {
    type Err = ::never::Never;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut sha1 = Sha1::default();
        sha1.input(s.as_bytes());
        sha1.input(b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11");
        Ok(Accept(base64::encode(&sha1.result())))
    }
}
