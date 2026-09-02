//! Network bridge: a background thread owns the WebSocket on a small
//! single-threaded tokio runtime; Bevy systems talk to it over channels
//! and never block.

use std::sync::Mutex;
use std::sync::mpsc::{Receiver, channel};

use futures_util::{Sink, SinkExt, Stream, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::{self, Message};

use sf_proto::codec::{decode, encode};
use sf_proto::messages::{ClientMsg, ServerMsg};
use sf_proto::tls::{Target, fingerprint, pinned_client_config, server_name};

pub enum NetEvent {
    /// TLS handshake done; the server's full certificate fingerprint
    /// (matched the pin), so the client can remember it.
    Secured(String),
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

/// Connect (pinned TLS unless the target is plaintext `ws://`) and
/// immediately send `initial` (Hello, then Create/Join) once the socket is
/// up. Dropping the returned handle closes the connection.
pub fn connect(target: Target, initial: Vec<ClientMsg>) -> NetHandle {
    let (out_tx, out_rx) = unbounded_channel::<ClientMsg>();
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
            let addr = target.join_string();
            let tcp = match TcpStream::connect((target.host.as_str(), target.port)).await {
                Ok(t) => t,
                Err(e) => {
                    let _ = in_tx.send(NetEvent::Closed(format!("connect {addr}: {e}")));
                    return;
                }
            };
            let url = format!(
                "{}://{}:{}",
                if target.fingerprint.is_some() { "wss" } else { "ws" },
                target.host,
                target.port
            );
            match &target.fingerprint {
                Some(pin) => {
                    let connector = TlsConnector::from(pinned_client_config(pin.clone()));
                    let tls = match connector.connect(server_name(&target.host), tcp).await {
                        Ok(t) => t,
                        Err(e) => {
                            let _ = in_tx.send(NetEvent::Closed(format!("TLS {addr}: {e}")));
                            return;
                        }
                    };
                    if let Some(cert) = tls.get_ref().1.peer_certificates().and_then(|c| c.first())
                    {
                        let _ = in_tx.send(NetEvent::Secured(fingerprint(cert.as_ref())));
                    }
                    match tokio_tungstenite::client_async(url, tls).await {
                        Ok((ws, _)) => pump(ws, initial, out_rx, in_tx).await,
                        Err(e) => {
                            let _ = in_tx.send(NetEvent::Closed(format!("connect {addr}: {e}")));
                        }
                    }
                }
                None => match tokio_tungstenite::client_async(url, tcp).await {
                    Ok((ws, _)) => pump(ws, initial, out_rx, in_tx).await,
                    Err(e) => {
                        let _ = in_tx.send(NetEvent::Closed(format!("connect {addr}: {e}")));
                    }
                },
            }
        });
    });
    NetHandle { out: out_tx, inbox: Mutex::new(in_rx) }
}

/// Send `initial`, then relay both directions until either side closes.
async fn pump<S>(
    ws: WebSocketStream<S>,
    initial: Vec<ClientMsg>,
    mut out_rx: tokio::sync::mpsc::UnboundedReceiver<ClientMsg>,
    in_tx: std::sync::mpsc::Sender<NetEvent>,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    WebSocketStream<S>: Stream<Item = Result<Message, tungstenite::Error>>
        + Sink<Message, Error = tungstenite::Error>,
{
    {
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
    }
}
