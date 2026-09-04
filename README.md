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

## License

The Star Fight source code and the original artwork in `assets/` are
dual-licensed under either the [MIT License](LICENSE-MIT) or the
[Apache License, Version 2.0](LICENSE-APACHE), at your option. Contributions
are accepted under the same terms. Third-party sprites and their licenses
(public domain unless noted) are listed in `assets/ships/SOURCES.md`.

**What the license does not cover.** Star Fight is a fan project and is not
affiliated with, endorsed by, or licensed by Fantasy Flight Games, Atomic
Mass Games, Lucasfilm Ltd., or Disney. The ship names, pilot names, card
abilities, statistics and point costs in `assets/data/*.ron` reproduce
material from *Star Wars: X-Wing Miniatures Game* for interoperability with
the physical game; that material remains the property of its owners and is
not licensed under the terms above. Card images are never distributed with
this project. The bundled DejaVu Sans Mono font is distributed under its own
license in `assets/fonts/DejaVu-LICENSE.txt`.
