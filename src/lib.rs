//! SaltyRTC client implementation in Rust.
//!
//! The implementation is asynchronous using Tokio / Futures.
//!
//! Early prototype. More docs will follow (#26).
#![recursion_limit = "1024"]
#![cfg_attr(feature="clippy", feature(plugin))]
#![cfg_attr(feature="clippy", plugin(clippy))]

extern crate byteorder;
extern crate data_encoding;
#[macro_use]
extern crate failure;
#[macro_use]
extern crate futures;
#[macro_use]
extern crate log;
#[macro_use]
extern crate mopa;
extern crate native_tls;
extern crate rmp_serde;
extern crate rust_sodium;
extern crate rust_sodium_sys;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate tokio_core;
extern crate websocket;

// Re-exports
pub extern crate rmpv;

// Modules
mod boxes;
mod crypto_types;
pub mod errors;
mod helpers;
mod protocol;
mod send_all;
mod task;
#[cfg(test)]
mod test_helpers;

// Rust imports
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Third party imports
use futures::{stream, Future, Stream, Sink};
use futures::future::{self, Loop};
use futures::sync::mpsc;
use futures::sync::oneshot;
use native_tls::{TlsConnector};
use rmpv::Value;
use tokio_core::reactor::{Handle};
use tokio_core::net::TcpStream;
use websocket::{WebSocketError};
use websocket::client::{ClientBuilder};
use websocket::client::async::{Client, TlsStream};
use websocket::client::builder::{Url};
use websocket::ws::dataframe::{DataFrame};
use websocket::header::{WebSocketProtocol};
use websocket::message::{OwnedMessage, CloseData};

// Re-exports
pub use protocol::{Role};
pub use task::{Task, BoxedTask};

/// Cryptography-related types like public/private keys.
pub mod crypto {
    pub use crypto_types::{KeyPair, PublicKey, PrivateKey, AuthToken};
    pub use crypto_types::{public_key_from_hex_str};
}

// Internal imports
use boxes::{ByteBox};
use crypto_types::{KeyPair, PublicKey, AuthToken};
use errors::{SaltyResult, SaltyError, SignalingResult, SignalingError, BuilderError};
use helpers::libsodium_init;
use protocol::{HandleAction, Signaling, InitiatorSignaling, ResponderSignaling};
use task::{Tasks};


// Constants
const SUBPROTOCOL: &'static str = "v1.saltyrtc.org";
#[cfg(feature = "msgpack-debugging")]
const DEFAULT_MSGPACK_DEBUG_URL: &'static str = "https://msgpack.dbrgn.ch/#base64=";
const SEND_CHANNEL_BUFFER: usize = 32;
const RECV_CHANNEL_BUFFER: usize = 32;


/// A type alias for a boxed future.
pub type BoxedFuture<T, E> = Box<Future<Item = T, Error = E>>;

/// A type alias for the async websocket client type.
pub type WsClient = Client<TlsStream<TcpStream>>;


/// Wrap future in a box with type erasure.
macro_rules! boxed {
    ($future:expr) => {{
        Box::new($future) as BoxedFuture<_, _>
    }}
}


/// The builder used to create a [`SaltyClient`](struct.SaltyClient.html) instance.
pub struct SaltyClientBuilder {
    permanent_key: KeyPair,
    tasks: Vec<BoxedTask>,
    ping_interval: Option<Duration>,
}

impl SaltyClientBuilder {
    /// Instantiate a new builder.
    pub fn new(permanent_key: KeyPair) -> Self {
        SaltyClientBuilder {
            permanent_key,
            tasks: vec![],
            ping_interval: None,
        }
    }

    /// Register a [`Task`](trait.Task.html) that should be accepted by the client.
    ///
    /// When calling this method multiple times, tasks added first
    /// have the highest priority during task negotation.
    pub fn add_task(mut self, task: BoxedTask) -> Self {
        self.tasks.push(task);
        self
    }

    /// Request that the server sends a WebSocket ping message at the specified interval.
    ///
    /// Set the `interval` argument to `None` or to a zero duration to disable intervals.
    ///
    /// Note: Fractions of seconds are ignored, so if you set the duration to 13.37s,
    /// then the ping interval 13s will be requested.
    ///
    /// By default, ping messages are disabled.
    pub fn with_ping_interval(mut self, interval: Option<Duration>) -> Self {
        self.ping_interval = interval;
        self
    }

    /// Create a new SaltyRTC initiator.
    pub fn initiator(self) -> Result<SaltyClient, BuilderError> {
        let tasks = Tasks::from_vec(self.tasks).map_err(|_| BuilderError::MissingTask)?;
        let signaling = InitiatorSignaling::new(
            self.permanent_key,
            tasks,
            self.ping_interval,
        );
        Ok(SaltyClient {
            signaling: Box::new(signaling),
        })
    }

    /// Create a new SaltyRTC responder.
    pub fn responder(self, initiator_pubkey: PublicKey, auth_token: Option<AuthToken>) -> Result<SaltyClient, BuilderError> {
        let tasks = Tasks::from_vec(self.tasks).map_err(|_| BuilderError::MissingTask)?;
        let signaling = ResponderSignaling::new(
            self.permanent_key,
            initiator_pubkey,
            auth_token,
            tasks,
            self.ping_interval,
        );
        Ok(SaltyClient {
            signaling: Box::new(signaling),
        })
    }
}

/// The SaltyRTC Client instance.
///
/// To create an instance of this struct, use the
/// [`SaltyClientBuilder`](struct.SaltyClientBuilder.html).
pub struct SaltyClient {
    /// The signaling trait object.
    ///
    /// This is either an
    /// [`InitiatorSignaling`](protocol/struct.InitiatorSignaling.html) or a
    /// [`ResponderSignaling`](protocol/struct.ResponderSignaling.html)
    /// instance.
    signaling: Box<Signaling>,
}

impl SaltyClient {

    /// Return the assigned role.
    pub fn role(&self) -> Role {
        self.signaling.role()
    }

    /// Return a reference to the auth token.
    pub fn auth_token(&self) -> Option<&AuthToken> {
        self.signaling.auth_token()
    }

    /// Return a reference to the selected task.
    pub fn task(&self) -> Option<Arc<Mutex<BoxedTask>>> {
        self.signaling
            .common()
            .task
            .clone()
    }

    /// Handle an incoming message.
    fn handle_message(&mut self, bbox: ByteBox) -> SignalingResult<Vec<HandleAction>> {
        self.signaling.handle_message(bbox)
    }

    /// Encrypt a task message.
    pub fn encrypt_task_message(&mut self, val: Value) -> SaltyResult<Vec<u8>> {
        self.signaling
            .encode_task_message(val)
            .map(|bbox: ByteBox| bbox.into_bytes())
            .map_err(|e: SignalingError| match e {
                SignalingError::Crypto(msg) => SaltyError::Crypto(msg),
                SignalingError::Decode(msg) => SaltyError::Decode(msg),
                SignalingError::Protocol(msg) => SaltyError::Protocol(msg),
                SignalingError::Crash(msg) => SaltyError::Crash(msg),
                other => SaltyError::Crash(format!("Unexpected signaling error: {}", other)),
            })
    }

    /// Encrypt a close message for the peer.
    pub fn encrypt_close_message(&mut self, reason: CloseCode) -> SaltyResult<Vec<u8>> {
        self.signaling
            .encode_close_message(reason)
            .map(|bbox: ByteBox| bbox.into_bytes())
            .map_err(|e: SignalingError| match e {
                SignalingError::Crypto(msg) => SaltyError::Crypto(msg),
                SignalingError::Decode(msg) => SaltyError::Decode(msg),
                SignalingError::Protocol(msg) => SaltyError::Protocol(msg),
                SignalingError::Crash(msg) => SaltyError::Crash(msg),
                other => SaltyError::Crash(format!("Unexpected signaling error: {}", other)),
            })
    }
}


/// Close codes used by SaltyRTC.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum CloseCode {
    WsGoingAway,
    WsProtocolError,
    PathFull,
    ProtocolError,
    InternalError,
    Handover,
    DroppedByInitiator,
    InitiatorCouldNotDecrypt,
    NoSharedTask,
    InvalidKey,
}

impl CloseCode {
    fn as_number(&self) -> u16 {
        use CloseCode::*;
        match *self {
            WsGoingAway => 1001,
            WsProtocolError => 1002,
            PathFull => 3000,
            ProtocolError => 3001,
            InternalError => 3002,
            Handover => 3003,
            DroppedByInitiator => 3004,
            InitiatorCouldNotDecrypt => 3005,
            NoSharedTask => 3006,
            InvalidKey => 3007,
        }
    }

    fn from_number(code: u16) -> Option<CloseCode> {
        use CloseCode::*;
        match code {
            1001 => Some(WsGoingAway),
            1002 => Some(WsProtocolError),
            3000 => Some(PathFull),
            3001 => Some(ProtocolError),
            3002 => Some(InternalError),
            3003 => Some(Handover),
            3004 => Some(DroppedByInitiator),
            3005 => Some(InitiatorCouldNotDecrypt),
            3006 => Some(NoSharedTask),
            3007 => Some(InvalidKey),
            _ => None,
        }
    }
}

impl fmt::Display for CloseCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?} ({})", self, self.as_number())
    }
}


/// Wrapper type for decoded form of WebSocket message types that we want to handle.
#[derive(Debug)]
enum WsMessageDecoded {
    /// We got bytes that we decoded into a ByteBox.
    ByteBox(ByteBox),
    /// We got a ping message.
    Ping(Vec<u8>),
    /// We got a message type that we want to ignore.
    Ignore,
}


/// Connect to the specified SaltyRTC server.
///
/// This function returns a boxed future. The future must be run in a Tokio
/// reactor core for something to actually happen.
///
/// The future completes once the server connection is established.
/// It returns the async websocket client instance.
pub fn connect(
    url: &str, // TODO: Derive from SaltyClient instance
    tls_config: Option<TlsConnector>,
    handle: &Handle,
    salty: Rc<RefCell<SaltyClient>>,
) -> SaltyResult<BoxedFuture<WsClient, SaltyError>> {
    // Initialize libsodium
    libsodium_init()?;

    // Parse URL
    let ws_url = match Url::parse(url) {
        Ok(b) => b,
        Err(e) => return Err(SaltyError::Decode(format!("Could not parse URL: {}", e))),
    };

    // Initialize WebSocket client
    let future = ClientBuilder::from_url(&ws_url)
        .add_protocol(SUBPROTOCOL)
        .async_connect_secure(tls_config, handle)
        .map_err(|e: WebSocketError| SaltyError::Network(match e.cause() {
            Some(cause) => format!("Could not connect to server: {}: {}", e, cause),
            None => format!("Could not connect to server: {}", e),
        }))
        .and_then(|(client, headers)| {
            // Verify that the correct subprotocol was chosen
            trace!("Websocket server headers: {:?}", headers);
            match headers.get::<WebSocketProtocol>() {
                Some(proto) if proto.len() == 1 && proto[0] == SUBPROTOCOL => {
                    Ok(client)
                },
                Some(proto) => {
                    error!("More than one chosen protocol: {:?}", proto);
                    Err(SaltyError::Protocol("More than one websocket subprotocol chosen by server".into()))
                },
                None => {
                    error!("No protocol chosen by server");
                    Err(SaltyError::Protocol("Websocket subprotocol not accepted by server".into()))
                },
            }
        })
        .map(move |client| {
            let role = salty
                .deref()
                .try_borrow()
                .map(|s| s.role().to_string())
                .unwrap_or_else(|_| "Unknown".to_string());
            info!("Connected to server as {}", role);
            client
        });

    Ok(boxed!(future))
}

/// Decode a websocket `OwnedMessage` and wrap it into a `WsMessageDecoded`.
fn decode_ws_message(msg: OwnedMessage) -> SaltyResult<WsMessageDecoded> {
    let decoded = match msg {
        OwnedMessage::Binary(bytes) => {
            debug!("Incoming binary message ({} bytes)", bytes.len());

            // Parse into ByteBox
            let bbox = ByteBox::from_slice(&bytes)
                .map_err(|e| SaltyError::Protocol(e.to_string()))?;
            trace!("ByteBox: {:?}", bbox);

            WsMessageDecoded::ByteBox(bbox)
        },
        OwnedMessage::Ping(payload) => {
            debug!("Incoming ping message");
            WsMessageDecoded::Ping(payload)
        },
        OwnedMessage::Close(close_data) => {
            match close_data {
                Some(data) => {
                    let close_code = CloseCode::from_number(data.status_code);
                    match close_code {
                        Some(code) if data.reason.is_empty() =>
                            info!("Server closed connection with close code {}", code),
                        Some(code) =>
                            info!("Server closed connection with close code {} ({})", code, data.reason),
                        None if data.reason.is_empty() =>
                            info!("Server closed connection with unknown close code {}", data.status_code),
                        None =>
                            info!("Server closed connection with unknown close code {} ({})", data.status_code, data.reason),
                    }
                },
                None => info!("Server closed connection without close code"),
            };
            return Err(SaltyError::Network("Server message stream ended".into()));
        },
        other => {
            warn!("Skipping non-binary message: {:?}", other);
            WsMessageDecoded::Ignore
        },
    };
    Ok(decoded)
}

/// An action in our pipeline.
///
/// This is used to enable early-return inside the pipeline. If a step returns a `Future`,
/// it should be passed directly to the `loop_fn`.
enum PipelineAction {
    /// We got a ByteBox to handle.
    ByteBox((WsClient, ByteBox)),
    /// Immediately pass on this future in the next step.
    Future(BoxedFuture<Loop<WsClient, WsClient>, SaltyError>),
}

/// Preprocess a `WsMessageDecoded`.
///
/// Here pings and ignored messages are handled.
fn preprocess_ws_message((decoded, client): (WsMessageDecoded, WsClient)) -> SaltyResult<PipelineAction> {
    // Unwrap byte box, handle ping messages
    let bbox = match decoded {
        WsMessageDecoded::ByteBox(bbox) => bbox,
        WsMessageDecoded::Ping(payload) => {
            let pong = OwnedMessage::Pong(payload);
            let outbox = stream::iter_ok::<_, WebSocketError>(vec![pong]);
            let future = send_all::new(client, outbox)
                .map_err(move |e| SaltyError::Network(format!("Could not send pong message: {}", e)))
                .map(|(client, _)| {
                    debug!("Sent pong message");
                    Loop::Continue(client)
                });
            let action = PipelineAction::Future(boxed!(future));
            return Ok(action);
        },
        WsMessageDecoded::Ignore => {
            debug!("Ignoring message");
            let action = PipelineAction::Future(boxed!(future::ok(Loop::Continue(client))));
            return Ok(action);
        },
    };
    Ok((PipelineAction::ByteBox((client, bbox))))
}

/// Do the server and peer handshake.
///
/// This function returns a boxed future. The future must be run in a Tokio
/// reactor core for something to actually happen.
///
/// The future completes once the peer handshake is done, or if an error occurs.
/// It returns the async websocket client instance.
pub fn do_handshake(
    client: WsClient,
    salty: Rc<RefCell<SaltyClient>>,
) -> BoxedFuture<WsClient, SaltyError> {

    let role = salty
        .deref()
        .try_borrow()
        .map(|s| s.role().to_string())
        .unwrap_or_else(|_| "Unknown".to_string());
    info!("Connected to server as {}", role);

    // Main loop
    let main_loop = future::loop_fn(client, move |client| {

        let salty = Rc::clone(&salty);

        // Take the next incoming message
        client.into_future()

            // Map errors to our custom error type
            .map_err(|(e, _)| SaltyError::Network(format!("Could not receive message from server: {}", e)))

            // Process incoming messages and convert them to a `WsMessageDecoded`.
            .and_then(|(msg_option, client)| {
                let decoded = match msg_option {
                    Some(msg) => decode_ws_message(msg),
                    None => return Err(SaltyError::Network("Server message stream ended without close message".into())),
                };
                decoded.map(|decoded| (decoded, client))
            })

            // Preprocess messages, handle things like ping/pong and ignored messages
            .and_then(preprocess_ws_message)

            // Process received signaling message
            .and_then(move |pipeline_action| {
                let (client, bbox) = match pipeline_action {
                    PipelineAction::ByteBox(x) => x,
                    PipelineAction::Future(f) => return f,
                };

                // Handle message bytes
                let handle_actions = match salty.deref().try_borrow_mut() {
                    Ok(mut s) => match s.handle_message(bbox) {
                        Ok(actions) => actions,
                        Err(e) => return boxed!(future::err(e.into())),
                    },
                    Err(e) => return boxed!(future::err(SaltyError::Crash(
                        format!("Could not get mutable reference to SaltyClient: {}", e)
                    ))),
                };

                // Extract messages that should be sent back to the server
                let mut messages = vec![];
                let mut handshake_done = false;
                for action in handle_actions {
                    match action {
                        HandleAction::Reply(bbox) => messages.push(OwnedMessage::Binary(bbox.into_bytes())),
                        HandleAction::HandshakeDone => handshake_done = true,
                        HandleAction::TaskMessage(_) => return boxed!(future::err(
                            SaltyError::Crash("Received task message during handshake".into())
                        )),
                        HandleAction::TaskClose(_) => return boxed!(future::err(
                            SaltyError::Crash("Received close message during handshake".into())
                        ))
                    }
                }

                macro_rules! loop_action {
                    ($client:expr) => {
                        match handshake_done {
                            false => Loop::Continue($client),
                            true => Loop::Break($client),
                        }
                    }
                };

                // If there are enqueued messages, send them
                if messages.is_empty() {
                    boxed!(future::ok(loop_action!(client)))
                } else {
                    for message in &messages {
                        debug!("Sending {} bytes", message.size());
                    }
                    let outbox = stream::iter_ok::<_, WebSocketError>(messages);
                    let future = send_all::new(client, outbox)
                        .map_err(move |e| SaltyError::Network(format!("Could not send message: {}", e)))
                        .map(move |(client, _)| {
                            trace!("Sent all messages");
                            loop_action!(client)
                        });
                    boxed!(future)
                }
            })
    });

    boxed!(main_loop)
}

pub fn task_loop(
    client: WsClient,
    salty: Rc<RefCell<SaltyClient>>,
) -> Result<(Arc<Mutex<BoxedTask>>, BoxedFuture<(((), ()), ()), SaltyError>), SaltyError> {
    let task_name = salty
        .deref()
        .try_borrow()
        .ok()
        .and_then(|salty| salty.task())
        .and_then(|task| match task.lock() {
            Ok(t) => Some(t.name()),
            Err(_) => None,
        })
        .unwrap_or("Unknown".into());
    info!("Starting task loop for task {}", task_name);

    let salty = Rc::clone(&salty);

    // Split websocket connection into sink/stream
    let (ws_sink, ws_stream) = client.split();

    // Create communication channels
    // TODO: Use unbounded channels and `unbounded_send`!
    let (outgoing_tx, outgoing_rx) = mpsc::channel::<Value>(SEND_CHANNEL_BUFFER);
    let (raw_outgoing_tx, raw_outgoing_rx) = mpsc::channel::<OwnedMessage>(SEND_CHANNEL_BUFFER);
    let (incoming_tx, incoming_rx) = mpsc::channel::<Value>(RECV_CHANNEL_BUFFER);
    let (disconnect_tx, disconnect_rx) = oneshot::channel::<Option<CloseCode>>();

    // Stream future for processing incoming websocket messages
    let reader = ws_stream

        // Map errors to our custom error type
        // TODO: Take a look at `sink_from_err`
        .map_err(|e| SaltyError::Network(format!("Could not receive message from server: {}", e)))

        // Decode messages
        .and_then(decode_ws_message)

        // Wrap errors in a result type
        .map_err(|e| Err(e))

        // Handle each incoming message.
        //
        // The closure passed to `for_each` must return:
        //
        // * `future::ok(())` to continue processing the stream
        // * `future::err(Ok(()))` to stop the loop without an error
        // * `future::err(Err(_))` to stop the loop with an error
        .for_each({
            let salty = Rc::clone(&salty);
            let raw_outgoing_tx = raw_outgoing_tx.clone();
            move |msg: WsMessageDecoded| {
                let raw_outgoing_tx = raw_outgoing_tx.clone();
                match msg {
                    WsMessageDecoded::ByteBox(bbox) => {
                        trace!("Got binary websocket msg: {:?}", bbox);

                        // Handle message bytes
                        let handle_actions = match salty.deref().try_borrow_mut() {
                            Ok(mut s) => match s.handle_message(bbox) {
                                Ok(actions) => actions,
                                Err(e) => return boxed!(future::err(Err(e.into()))),
                            },
                            Err(e) => return boxed!(future::err(Err(
                                SaltyError::Crash(format!("Could not get mutable reference to SaltyClient: {}", e))
                            ))),
                        };

                        // Extract messages that should be sent back to the server
                        let mut out_messages = vec![];
                        let mut in_messages = vec![];
                        for action in handle_actions {
                            match action {
                                HandleAction::Reply(bbox) => out_messages.push(OwnedMessage::Binary(bbox.into_bytes())),
                                HandleAction::TaskMessage(val) => in_messages.push(val),
                                HandleAction::TaskClose(reason) => {
                                    // Get access to SaltyClient
                                    let salty = match salty.try_borrow_mut() {
                                        Ok(salty) => salty,
                                        Err(e) => return boxed!(future::err(Err(
                                            SaltyError::Crash(format!("Could not mutably borrow SaltyRTC instance: {}", e))
                                        ))),
                                    };

                                    // Get access to Task
                                    let task = match salty.task() {
                                        Some(task) => task,
                                        None => return boxed!(future::err(Err(
                                            SaltyError::Crash(format!("Task not set"))
                                        )))
                                    };

                                    // Notify task about closing message
                                    match task.lock() {
                                        Ok(ref mut t) => t.close(reason),
                                        Err(_) => {},
                                    };

                                    // Return a `future::err(Ok(_))` to stop processing the stream.
                                    return boxed!(future::err(Ok(())))
                                },
                                HandleAction::HandshakeDone => return boxed!(future::err(Err(
                                    SaltyError::Crash("Got HandleAction::HandshakeDone in task loop".into())
                                ))),
                            }
                        }

                        // Handle queued messages
                        let out_future = if out_messages.is_empty() {
                            boxed!(future::ok(()))
                        } else {
                            let msg_count = out_messages.len();
                            let outbox = stream::iter_ok::<_, Result<(), SaltyError>>(out_messages);
                            let future = raw_outgoing_tx
                                .sink_map_err(|e| Err(SaltyError::Network(format!("Sink error: {}", e))))
                                .send_all(outbox)
                                .map(move |_| debug!("Sent {} messages", msg_count));
                            boxed!(future)
                        };

                        let in_future = if in_messages.is_empty() {
                            boxed!(future::ok(()))
                        } else {
                            let msg_count = in_messages.len();
                            let inbox = stream::iter_ok::<_, Result<(), SaltyError>>(in_messages);
                            let future = incoming_tx
                                .clone()
                                .sink_map_err(|e| Err(SaltyError::Crash(format!("Channel error: {}", e))))
                                .send_all(inbox)
                                .map(move |_| debug!("Received {} task messages", msg_count));
                            boxed!(future)
                        };

                        boxed!(out_future.join(in_future).map(|_| ()))
                    },
                    WsMessageDecoded::Ping(payload) => {
                        let pong = OwnedMessage::Pong(payload);
                        let future = raw_outgoing_tx
                            .send(pong)
                            .map(|_| debug!("Enqueued pong message"))
                            .map_err(|e| Err(SaltyError::Network(format!("Could not enqueue pong message: {}", e))));
                        boxed!(future)
                    },
                    WsMessageDecoded::Ignore => boxed!(future::ok(())),
                }
            }
        })

        .or_else(|res| match res {
            Ok(_) => boxed!(future::ok(())),
            Err(e) => boxed!(future::err(e))
        })

        .select(
            disconnect_rx
                .and_then({
                    let raw_outgoing_tx = raw_outgoing_tx.clone();
                    move |reason_opt: Option<CloseCode>| {
                        let close = OwnedMessage::Close(Some(CloseData {
                            status_code: reason_opt.map(|cc| cc.as_number()).unwrap_or(1001),
                            reason: reason_opt.map(|cc| cc.to_string()).unwrap_or("".to_string()),
                        }));
                        raw_outgoing_tx
                            .send(close)
                            .map(|_| debug!("Sent close message"))
                            .or_else(|e| {
                                warn!("Could not enqueue close message: {}", e);
                                future::ok(())
                            })
                    }
                })
                .or_else(|_| {
                    warn!("Waiting for disconnect_rx failed");
                    future::ok(())
                })
        )

        .map(|_| ())
        .map_err(|(e, _next)| e);

    // Transform future that sends values from the outgoing channel to the raw outgoing channel
    let transformer = outgoing_rx

        // Encode and encrypt values
        .and_then({
            let salty = Rc::clone(&salty);
            move |val: Value| {
                // Encrypt message
                // TODO: Can we do something about the errors here?
                match salty.deref().try_borrow_mut() {
                    Ok(mut s) => match s.encrypt_task_message(val) {
                        Ok(bytes) => future::ok(OwnedMessage::Binary(bytes)),
                        Err(_) => future::err(())
                    },
                    Err(_) => future::err(()),
                }
            }
        })

        // Forward to raw queue
        .forward(raw_outgoing_tx.sink_map_err(|_| ()))

        // Ignore stream/sink
        .map(|(_, _)| ())

        // Map error types
        .map_err(|_| SaltyError::Crash("TODO: read error".into()));

    // Sink future for sending messages from the raw outgoing channel through the WebSocket
    let writer = ws_sink

        // Map sink errors
        .sink_map_err(|e| SaltyError::Crash(format!("TODO sink error: {:?}", e)))

        // Forward all messages from the channel receiver to the sink
        .send_all(
            raw_outgoing_rx
                .map_err(|_| SaltyError::Crash(format!("TODO receiver error")))
        )

        // Ignore sink
        .map(|_| ());

    // The task loop is finished when all futures are resolved.
    let task_loop = boxed!(reader.join(transformer).join(writer));

    // Get reference to task
    let task = match salty.try_borrow_mut() {
        Ok(salty) => salty
            .task()
            .ok_or(SaltyError::Crash("Task not set".into()))?,
        Err(e) => return Err(
            SaltyError::Crash(format!("Could not mutably borrow SaltyRTC instance: {}", e))
        ),
    };

    // Notify task that it can now take over
    task.lock()
        .map_err(|e| SaltyError::Crash(format!("Could not lock task mutex: {}", e)))?
        .start(outgoing_tx, incoming_rx, disconnect_tx);

    // Return reference to task and the task loop future
    Ok((task, task_loop))
}
