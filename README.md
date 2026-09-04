# Star Fight

A cross-platform (Linux / macOS / Windows) multiplayer space-fighter game in
the style of the X-Wing miniatures game: hidden maneuver dials, movement
templates, firing arcs, dice with focus/evade/target-lock tokens, critical
damage, pilots, upgrades and squad building. Rust workspace: `sf-core`
(pure rules), `sf-proto` (wire messages), `sf-server` (tokio, TLS), `sf-client`
(Bevy).

## Playing

1. Host: run `sf-server` on a Linux box (or anywhere). It prints a
   certificate fingerprint, a password and a join string like
   `starfight://host:7777/#<fingerprint>` — send both to the players and open
   port 7777 (TCP).
2. Players: run `sf-client` from the folder that contains `assets/`. In the
   menu, paste the join string into **Server** (Ctrl+V), type the password,
   optionally build a squad (**Squad Builder**), then **Create Game** (share
   the 4-letter code) or **Join Game** with the code.
3. Card images for the squad builder are optional: clone
   [xwing-card-images](https://github.com/voidstate/xwing-card-images) into
   `reference/` next to the client (or point `STARFIGHT_CARDS` at its
   `images` folder). Without them the builder shows card text.

Downloads: **Actions → Release builds** (or a tagged GitHub Release) has zips
for Linux and Windows containing both binaries and `assets/`.

## Developing

```
cargo run -p sf-server -- --insecure     # plaintext, no password, local only
cargo run -p sf-client                   # twice: create in one, join in the other
cargo test --workspace
cargo clippy --workspace --tests -- -D warnings
```

`NEXT-SESSION.md` is the running design/progress log; `ARCHITECTURE.md` the
design document. Game data (ships, dials, pilots, upgrades) lives in
`assets/data/*.ron`.
