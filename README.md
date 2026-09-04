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
3. Card images for the squad builder are optional; see
   [Card images](#card-images-optional) below. Without them the builder
   shows the card text.

Downloads: **Actions → Release builds** (or a tagged GitHub Release) has zips
for Linux and Windows containing both binaries and `assets/`.

## Card images (optional)

The squad builder can show the real pilot and upgrade cards. The scans are
not part of the game download (they are Fantasy Flight Games' artwork), so
each player fetches them once from the community repository
[voidstate/xwing-card-images](https://github.com/voidstate/xwing-card-images)
and puts its `images` folder where the client looks for it.

The client checks these locations at startup, in order, and uses the first
one that contains a `pilots` folder:

1. The folder named by the `STARFIGHT_CARDS` environment variable.
2. `reference/xwing-card-images/images` under the folder the client was
   started from (this is how the development tree is laid out).
3. `cards` inside the per-user config folder:
   - Windows: `%APPDATA%\starfight\cards`
     (usually `C:\Users\<you>\AppData\Roaming\starfight\cards`)
   - Linux: `~/.config/starfight/cards`
     (or `$XDG_CONFIG_HOME/starfight/cards` if that variable is set)
   - macOS: `~/Library/Application Support/starfight/cards`

The simplest setup for the release zip is option 3. Download the repository
as a zip from GitHub (green **Code** button → **Download ZIP**) or clone it,
then copy the contents of its `images` folder so that the layout is:

```
<config>/starfight/cards/
    pilots/
        rebels/t70xwing/poedameron.png
        imperial/tiefighter/howlrunner.png
        ...
    upgrades/
        talent/veteraninstincts.png
        torpedo/protontorpedoes.png
        ...
```

That is, `cards` must directly contain `pilots` and `upgrades`. The
`starfight` folder already exists once the client has been run at least
once (it also holds the saved squads and server pins); create `cards`
inside it. A Windows player can do it in PowerShell after downloading and
extracting the zip:

```
mkdir $env:APPDATA\starfight\cards
Copy-Item -Recurse .\xwing-card-images-master\images\* $env:APPDATA\starfight\cards\
```

On Linux:

```
git clone https://github.com/voidstate/xwing-card-images.git
mkdir -p ~/.config/starfight/cards
cp -r xwing-card-images/images/* ~/.config/starfight/cards/
```

Restart the client afterwards; the folder is only scanned at startup. If a
card has no image the builder falls back to its text, so a partial copy is
fine.

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
