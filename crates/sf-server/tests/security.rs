//! M4 security: pinned-certificate TLS handshake, server password check
//! (constant-time), and rate limiting of failed attempts.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::tungstenite::Message;

use sf_core::data::Content;
use sf_proto::codec::{decode, encode};
use sf_proto::messages::{ClientMsg, ServerMsg};
use sf_proto::tls::{fingerprint, pinned_client_config, server_name};
use sf_server::{RATE_LIMIT, ServerOpts};

fn content() -> Content {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data");
    Content::load_dir(dir).unwrap()
}

/// Start a TLS server with a fresh identity in a scratch directory.
/// Returns (port, full fingerprint).
async fn start(password: &str) -> (u16, String) {
    let dir = std::env::temp_dir().join(format!("sf-tls-test-{}", rand_suffix()));
    let (cert, key) = sf_server::tls::load_or_create_identity(&dir).unwrap();
    let fp = fingerprint(cert.as_ref());
    // Reloading yields the same certificate — the pin survives restarts.
    let (again, _) = sf_server::tls::load_or_create_identity(&dir).unwrap();
    assert_eq!(fingerprint(again.as_ref()), fp);
    let _ = std::fs::remove_dir_all(&dir);

    let tls = sf_server::tls::server_config(cert, key).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let opts = ServerOpts { tls: Some(tls), password: Some(password.to_string()) };
    tokio::spawn(sf_server::run(listener, Arc::new(content()), opts));
    (port, fp)
}

fn rand_suffix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(1)
        ^ std::process::id() as u64
}

type Ws = tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;

/// TLS + WebSocket to the server, pinning `pin`; Err on handshake failure.
async fn connect(port: u16, pin: &str) -> Result<Ws, String> {
    let tcp = TcpStream::connect(("127.0.0.1", port)).await.map_err(|e| e.to_string())?;
    let connector = TlsConnector::from(pinned_client_config(pin.to_string()));
    let tls = connector.connect(server_name("127.0.0.1"), tcp).await.map_err(|e| e.to_string())?;
    let (ws, _) = tokio_tungstenite::client_async(format!("wss://127.0.0.1:{port}"), tls)
        .await
        .map_err(|e| e.to_string())?;
    Ok(ws)
}

async fn hello(ws: &mut Ws, password: &str) -> ServerMsg {
    let msg = ClientMsg::Hello {
        proto_version: sf_proto::PROTOCOL_VERSION,
        name: "tester".into(),
        password: password.into(),
    };
    ws.send(Message::Text(encode(&msg))).await.unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out")
        .expect("stream ended")
        .expect("ws error");
    match frame {
        Message::Text(t) => decode(&t).unwrap(),
        other => panic!("unexpected frame {other:?}"),
    }
}

#[tokio::test]
async fn pinned_tls_and_password_admit_the_right_client() {
    let (port, fp) = start("open-sesame").await;

    // Full fingerprint and a 16-hex prefix both pin successfully.
    for pin in [fp.as_str(), &fp[..16]] {
        let mut ws = connect(port, pin).await.expect("handshake with correct pin");
        assert!(matches!(hello(&mut ws, "open-sesame").await, ServerMsg::Welcome { .. }));
    }

    // Wrong password is refused inside the tunnel.
    let mut ws = connect(port, &fp).await.unwrap();
    match hello(&mut ws, "open-sesamE").await {
        ServerMsg::Error { message } => assert!(message.contains("password"), "{message}"),
        other => panic!("expected password error, got {other:?}"),
    }
}

#[tokio::test]
async fn wrong_fingerprint_never_completes_the_handshake() {
    let (port, fp) = start("pw").await;
    let mut wrong: String = fp.clone();
    // Flip the first nibble so the pin differs from the real certificate.
    let first = wrong.remove(0);
    wrong.insert(0, if first == '0' { '1' } else { '0' });
    let err = match connect(port, &wrong).await {
        Err(e) => e,
        Ok(_) => panic!("handshake must fail"),
    };
    assert!(err.contains("fingerprint"), "{err}");
}

#[tokio::test]
async fn repeated_wrong_passwords_are_rate_limited() {
    let (port, fp) = start("secret").await;
    for _ in 0..RATE_LIMIT {
        let mut ws = connect(port, &fp).await.unwrap();
        assert!(matches!(hello(&mut ws, "nope").await, ServerMsg::Error { .. }));
    }
    // Even the right password is refused once the address is blocked.
    let mut ws = connect(port, &fp).await.unwrap();
    match hello(&mut ws, "secret").await {
        ServerMsg::Error { message } => assert!(message.contains("too many"), "{message}"),
        other => panic!("expected rate-limit error, got {other:?}"),
    }
}
