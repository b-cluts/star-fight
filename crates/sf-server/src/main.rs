use std::sync::Arc;

use sf_core::data::Content;

fn load_content() -> Content {
    // Dev layout first (workspace assets), then alongside the binary.
    let candidates = [
        concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data").to_string(),
        "assets/data".to_string(),
    ];
    for dir in candidates {
        let ships = std::fs::read_to_string(format!("{dir}/ships.ron"));
        let dials = std::fs::read_to_string(format!("{dir}/maneuvers.ron"));
        if let (Ok(s), Ok(d)) = (ships, dials) {
            return Content::from_ron(&s, &d).expect("parse data files");
        }
    }
    panic!("could not find assets/data/{{ships,maneuvers}}.ron");
}

#[tokio::main]
async fn main() {
    let mut port = 7777u16;
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--port") {
        port = args.get(i + 1).and_then(|p| p.parse().ok()).unwrap_or_else(|| {
            eprintln!("usage: sf-server [--port <port>]");
            std::process::exit(2);
        });
    }
    let content = Arc::new(load_content());
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .unwrap_or_else(|e| panic!("bind port {port}: {e}"));
    println!("sf-server (protocol v{}) listening on port {port}", sf_proto::PROTOCOL_VERSION);
    println!("M3: plaintext WebSocket — TLS + password arrive in M4");
    sf_server::run(listener, content).await;
}
