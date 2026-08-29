//! Star Fight server. M3 adds tokio + TLS WebSockets, lobbies, and game
//! sessions; until then this is a compile-checked placeholder.

fn main() {
    println!(
        "sf-server (protocol v{}) — networking arrives in milestone M3",
        sf_proto::PROTOCOL_VERSION
    );
}
