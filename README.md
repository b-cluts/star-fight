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
   port 7777 (TCP). `--asteroids N` sets the default obstacle count for
   clients that do not send a setup.
2. Players: run `sf-client` from the folder that contains `assets/`. In the
   menu, paste the join string into **Server** (Ctrl+V), type the password
   (generated ones look like `abcd-efgh-jkmn`, avoid look-alike characters,
   and are accepted in any letter case),
   optionally build a squad (**Squad Builder**), then **Create Game** — pick a
   scenario on the setup screen (asteroids, debris, squad points per side,
   players 2-4 and the mode, board size; Up/Down, Tab, Left/Right, Enter)
   and share the 4-letter code — or **Join Game** with the code (joiners
   play the host's scenario; the game starts when every seat is taken).
   With 3-4 players the mode is **teams** (two sides, split as equally as
   possible, sharing the side's points and one board edge, winning
   together — core rules p.20) or a **free-for-all** (every seat its own
   side with the full points, deploying south, north, east and west; the
   last side flying wins).
   The three rulebook **missions** (core rules p.21-24) are scenarios too:
   Political Escort, Asteroid Run and Dark Whispers. The host flies the
   Rebel side and the joiner the Empire; joining without a squad fields the
   mission's printed force (31 points with the core-set ships), otherwise
   both build squads to the chosen points. Each mission's setup zones,
   special rules (the senator's shuttle and the **P** Protect action, the
   disabled ship, satellite scanning, reinforcements placed mid-game) and
   objectives are enforced and explained in the glossary's Rules tab.
   **Solo play**: the setup screen's last field, **Bots**, seats computer
   players after you (up to players − 1), so a duel, a team game, a
   free-for-all or a mission can be tried alone. Bots place their ships,
   fly toward the nearest enemy (or an escape edge / satellite in a
   mission), take Focus, and shoot the weakest target; they field the
   cheapest generic pilot of their faction up to the points, or the
   mission's printed force.
   **Factions**: Rebel Alliance (with the Resistance), Galactic Empire
   (with the First Order) and **Scum and Villainy** (Most Wanted: Z-95
   Headhunter, Y-Wing, Firespray-31 with its rear firing arc). Press F in
   the squad builder to cycle; the rulebook missions are Rebels vs Empire.
3. Card images for the squad builder are optional; see
   [Card images](#card-images-optional) below. Without them the builder
   shows the card text.
4. In a game, plan with the keys shown in the HUD help line: Tab selects a
   ship, Left/Right + Enter set the maneuver, number keys pick the action
   (6 then click an enemy = target lock), **B** cycles a bomb to drop on
   dial reveal, **M** makes a mine drop the action, **U** switches a
   card's once-a-round choice on (Lightning Reflexes, Electronic Baffle,
   Jan Ors, Decoy), **K** cycles card
   actions (Marksmanship, Rage, Expose, R2-F2, Fleet Officer, Squad
   Leader, R7-T1, Lando, Saboteur, R5-D8; Seismic Torpedo then asks you
   to click the obstacle to blast), C commits, X resigns. Pilots
   with a second action (Push the Limit, Darth Vader, Snap Wexley, Jake
   Farrell, BB-8, Turr Phennir's reposition after attacking) press **0**
   and then an action key to fill it; Blue Ace boosts with the turn
   templates on **-** / **=**, Zeta Ace far-rolls on **[** / **]**,
   Lieutenant Lorrir bank-rolls on **;** / **'** (hold Shift to bend the
   template backward), Expert Handling rolls on 4 / 5 without the icon.
   Weapons fire from the Declare Target prompt (number keys or click).
   The planning ghost shows the maneuver's end; a planned boost or barrel
   roll (and a reposition second action) is drawn on from there in cyan,
   red if it would leave the board or land on an obstacle, and with no
   reposition planned faint arrows mark every boost the ship could take.

**Effects Demo** in the menu plays every weapon impact, missile flight,
bomb token and detonation in a loop on a fake board so you can review the
animations without an opponent (Esc returns to the menu).

**Glossary**: press F1 (or click the "? glossary" button) on any screen,
including mid-game, to look up ship classes, pilots, every upgrade card,
tokens and actions, damage cards and rules terms. Left/Right switch tabs,
Up/Down select, typing filters, Esc closes. Cards the rules engine does not
enforce yet are marked "NOT yet automated".

Downloads: **Actions → Release builds** (or a tagged GitHub Release) has zips
for Linux and Windows containing both binaries and `assets/`.

## Playing over Tailscale (recommended)

The easiest way to play with friends elsewhere is a [Tailscale](https://tailscale.com)
network: no router port forwarding, no public IP, and the game's own pinned
TLS and password still apply on top of Tailscale's encryption.

1. Install Tailscale on the machine that runs `sf-server` and on every
   player's machine, all signed into the same tailnet (or share the server
   node with a friend's tailnet). Each machine gets a `100.x.y.z` address and,
   with MagicDNS, a name like `mybox.tailnet-name.ts.net`.
2. Start the server with its Tailscale name (or `100.x` address) so the
   printed join string points at it — the auto-detected address is usually
   the LAN one:

   ```
   sf-server --host mybox.tailnet-name.ts.net
   ```

   It prints the certificate fingerprint, the password and the join string
   `starfight://mybox.tailnet-name.ts.net:7777/#<fingerprint>`.
3. Send players the join string and the password. In the client they paste
   the join string into **Server**, type the password, then **Create Game**
   or **Join Game** with the 4-letter code. The fingerprint is remembered per
   host:port after the first connection.

Nothing needs opening on the router. Only a local firewall on the server
box could get in the way (allow TCP 7777 on the `tailscale0` interface). If
a player has MagicDNS turned off, use the server's `100.x` address in
`--host` instead of the name.

Without Tailscale, forward TCP 7777 on your router to the server box and
pass your public IP or dynamic-DNS name as `--host`; the pinned certificate
and password are what make that safe.

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
