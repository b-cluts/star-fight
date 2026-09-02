use std::path::PathBuf;
use std::sync::Arc;

use rand::Rng;
use rand::distributions::Alphanumeric;

use sf_core::data::Content;
use sf_server::ServerOpts;

fn load_content() -> Content {
    // Dev layout first (workspace assets), then alongside the binary.
    let candidates = [
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data").to_string(),
        "assets/data".to_string(),
    ];
    for dir in candidates {
        if std::path::Path::new(&format!("{dir}/ships.ron")).exists() {
            return Content::load_dir(&dir).unwrap_or_else(|e| panic!("{e}"));
        }
    }
    panic!("could not find assets/data/*.ron");
}

fn usage() -> ! {
    eprintln!(
        "usage: sf-server [--port <port>] [--password <pw>] [--host <name-or-ip>] \
         [--tls-dir <dir>] [--insecure]\n\
         \n\
         --password  server password players must enter (default: random, printed)\n\
         --host      host name / IP to print in the join string (default: detected)\n\
         --tls-dir   where tls_cert.pem / tls_key.pem live (default: current dir)\n\
         --insecure  plaintext ws:// without password — local testing only"
    );
    std::process::exit(2);
}

/// Best-effort LAN address for the join string: the source address the OS
/// would pick for an outbound packet (nothing is actually sent).
fn detect_host() -> String {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("192.0.2.1:9").and_then(|_| s.local_addr()))
        .map(|a| a.ip().to_string())
        .ok()
        .filter(|ip| ip != "0.0.0.0")
        .unwrap_or_else(|| "127.0.0.1".into())
}

#[tokio::main]
async fn main() {
    let mut port = sf_proto::tls::DEFAULT_PORT;
    let mut password: Option<String> = None;
    let mut host: Option<String> = None;
    let mut tls_dir = PathBuf::from(".");
    let mut insecure = false;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let value = |i: usize| args.get(i + 1).cloned().unwrap_or_else(|| usage());
        match args[i].as_str() {
            "--port" => {
                port = value(i).parse().unwrap_or_else(|_| usage());
                i += 1;
            }
            "--password" => {
                password = Some(value(i));
                i += 1;
            }
            "--host" => {
                host = Some(value(i));
                i += 1;
            }
            "--tls-dir" => {
                tls_dir = PathBuf::from(value(i));
                i += 1;
            }
            "--insecure" => insecure = true,
            _ => usage(),
        }
        i += 1;
    }

    let content = Arc::new(load_content());
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("bind port {port}: {e}"));
    let host = host.unwrap_or_else(detect_host);
    println!("sf-server (protocol v{}) listening on port {port}", sf_proto::PROTOCOL_VERSION);

    let opts = if insecure {
        println!("INSECURE: plaintext WebSocket, no password — local testing only");
        println!("join address: ws://{host}:{port}");
        ServerOpts::insecure()
    } else {
        let (cert, key) = sf_server::tls::load_or_create_identity(&tls_dir)
            .unwrap_or_else(|e| panic!("TLS identity: {e}"));
        let fp = sf_server::tls::fingerprint(cert.as_ref());
        let tls =
            sf_server::tls::server_config(cert, key).unwrap_or_else(|e| panic!("TLS config: {e}"));
        let password = password.unwrap_or_else(|| {
            rand::thread_rng().sample_iter(&Alphanumeric).take(8).map(char::from).collect()
        });
        println!("certificate SHA-256 fingerprint (players pin this):\n  {fp}");
        println!("server password: {password}");
        println!("join string (paste into the client's Server field):");
        println!("  starfight://{host}:{port}/#{fp}");
        ServerOpts { tls: Some(tls), password: Some(password) }
    };
    sf_server::run(listener, content, opts).await;
}
