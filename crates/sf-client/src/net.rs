//! Network bridge: a background thread owns the WebSocket on a small
//! single-threaded tokio runtime; Bevy systems talk to it over channels
//! and never block.

use std::sync::mpsc::{channel, Receiver};
use std::sync::Mutex;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio_tungstenite::tungstenite::Message;

use sf_proto::codec::{decode, encode};
use sf_proto::messages::{ClientMsg, ServerMsg};

pub enum NetEvent {
    Msg(ServerMsg),
    Closed(String),
}

pub struct NetHandle {
    out: UnboundedSender<ClientMsg>,
    inbox: Mutex<Receiver<NetEvent>>,
}

impl NetHandle {
    pub fn send(&self, m: ClientMsg) {
        let _ = self.out.send(m);
    }

    /// Non-blocking: everything that arrived since the last call.
    pub fn drain(&self) -> Vec<NetEvent> {
        let mut v = Vec::new();
        if let Ok(rx) = self.inbox.lock() {
            while let Ok(e) = rx.try_recv() {
                v.push(e);
            }
        }
        v
    }
}

/// Connect and immediately send `initial` (Hello, then Create/Join) once
/// the socket is up. Dropping the returned handle closes the connection.
pub fn connect(addr: String, initial: Vec<ClientMsg>) -> NetHandle {
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();
    let (in_tx, in_rx) = channel::<NetEvent>();
    std::thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(e) => {
                let _ = in_tx.send(NetEvent::Closed(format!("runtime: {e}")));
                return;
            }
        };
        rt.block_on(async move {
            let ws = match tokio_tungstenite::connect_async(addr.as_str()).await {
                Ok((ws, _)) => ws,
                Err(e) => {
                    let _ = in_tx.send(NetEvent::Closed(format!("connect {addr}: {e}")));
                    return;
                }
            };
            let (mut tx, mut rx) = ws.split();
            for m in initial {
                if tx.send(Message::Text(encode(&m))).await.is_err() {
                    let _ = in_tx.send(NetEvent::Closed("connection lost".into()));
                    return;
                }
            }
            loop {
                tokio::select! {
                    out = out_rx.recv() => match out {
                        Some(m) => {
                            if tx.send(Message::Text(encode(&m))).await.is_err() {
                                let _ = in_tx.send(NetEvent::Closed("connection lost".into()));
                                return;
                            }
                        }
                        // NetHandle dropped: polite close.
                        None => {
                            let _ = tx.send(Message::Close(None)).await;
                            return;
                        }
                    },
                    frame = rx.next() => match frame {
                        Some(Ok(Message::Text(t))) => match decode::<ServerMsg>(&t) {
                            Ok(m) => {
                                if in_tx.send(NetEvent::Msg(m)).is_err() {
                                    return;
                                }
                            }
                            Err(e) => {
                                let _ = in_tx.send(NetEvent::Closed(format!("bad message: {e}")));
                                return;
                            }
                        },
                        Some(Ok(Message::Close(_))) | None => {
                            let _ = in_tx.send(NetEvent::Closed("server closed the connection".into()));
                            return;
                        }
                        Some(Err(e)) => {
                            let _ = in_tx.send(NetEvent::Closed(format!("socket error: {e}")));
                            return;
                        }
                        _ => {}
                    },
                }
            }
        });
    });
    NetHandle { out: out_tx, inbox: Mutex::new(in_rx) }
}
