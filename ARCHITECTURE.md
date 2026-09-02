# Star Fight — Software Architecture

A cross-platform (Linux / macOS / Windows) multiplayer fleet-combat game written in Rust.
Turn-based maneuver combat: players secretly plot flight paths for each ship, then moves
resolve on an authoritative server.

---

## 1. Design Principles

1. **Authoritative server.** The server owns the true game state. Clients send *intents*
   (place ship here, fly this maneuver); the server validates and broadcasts results.
   This makes cheating structurally impossible for hidden-information mechanics
   (secretly plotted maneuvers) and keeps clients simple.
2. **Shared rules crate.** All game rules (geometry, maneuver templates, legality checks)
   live in one pure-logic crate compiled into both client and server. The client uses it
   for previews ("show me where this maneuver ends"); the server uses it as the source of
   truth. One implementation, zero rules drift.
3. **Data-driven content.** Ship stats, sizes, and maneuver lists are data files (RON),
   not code. Adding a ship or tweaking a maneuver set never requires a recompile of logic.
4. **Turn-based, not real-time.** No tick loops, no lag compensation, no client
   prediction. Networking is simple request/response + server broadcasts, which keeps the
   whole stack small.

---

## 2. Cargo Workspace Layout

```
star-fight/
├── Cargo.toml                 # [workspace] members = ["crates/*"]
├── ARCHITECTURE.md
├── assets/
│   ├── ships/                 # PNG sprites (see §8)
│   └── data/
│       ├── ships.ron          # ship classes: size, hull, maneuver-set id, sprite path
│       └── maneuvers.ron      # maneuver templates per maneuver-set
└── crates/
    ├── sf-core/               # PURE game logic — no I/O, no async, no rendering
    │   ├── geometry.rs        #   Pose, Vec2, arcs, rigid transforms, footprints
    │   ├── ship.rs            #   ShipClass, SizeClass, ShipState
    │   ├── maneuver.rs        #   Maneuver templates, path segments, pose application
    │   ├── board.rs           #   Board dims, deployment zones, bounds checks
    │   ├── rules.rs           #   Placement legality, maneuver legality, collision
    │   └── game.rs            #   GameState, phase machine, apply/validate commands
    ├── sf-proto/              # Wire protocol: serde message types + version constant
    │   ├── messages.rs        #   ClientMsg / ServerMsg enums
    │   └── codec.rs           #   length-framing helpers, (de)serialization
    ├── sf-server/             # Linux binary: tokio + TLS, lobbies, game sessions
    │   ├── main.rs
    │   ├── listener.rs        #   TLS accept loop, connection tasks
    │   ├── lobby.rs           #   create/join games (join codes), matchmaking later
    │   ├── session.rs         #   one task per game: owns GameState, applies commands
    │   └── registry.rs        #   player identity, reconnection tokens
    └── sf-client/             # Cross-platform binary: Bevy app
        ├── main.rs
        ├── net.rs             #   background tokio task ↔ channels ↔ game loop
        ├── states/            #   MainMenu, Lobby, Placement, Planning, Resolution
        ├── render/            #   board, sprites, maneuver-preview overlays
        └── ui.rs              #   maneuver picker, fleet panel
```

Dependency direction (strictly one-way):

```
sf-client ──┐
            ├──► sf-proto ──► sf-core
sf-server ──┘
```

`sf-core` and `sf-proto` have no async, no networking, no graphics dependencies —
they compile fast and are fully unit-testable.

---

## 3. Technology Choices

| Concern            | Choice                                   | Why |
|--------------------|------------------------------------------|-----|
| Client engine      | **Bevy** (0.16+)                         | Pure Rust, one codebase → Linux/macOS/Windows, built-in ECS, 2D sprites, UI, asset loading. |
| Server runtime     | **tokio**                                | Standard async runtime; one lightweight task per connection and per game session. |
| Transport          | **WebSocket over TLS (`wss://`)** via `tokio-tungstenite` + `rustls` | Message framing for free, encrypted, firewall/proxy-friendly, and leaves the door open to a future browser client. No OpenSSL — `rustls` is pure Rust, so cross-compiling the client stays painless. |
| Certificates       | **Self-signed cert + SHA-256 fingerprint pinning** (see §4) | No domain, CA, or renewal needed; works on bare IPs/LAN; fingerprint shared out-of-band is MITM-proof. WebPKI (Let's Encrypt) can be added later via the pluggable rustls verifier. |
| Serialization      | **serde** — JSON during development, `postcard` behind the same trait once stable | JSON is debuggable with any tool; postcard is compact later. The protocol enum carries a `PROTOCOL_VERSION` so old clients get a clean "please update" rejection. |
| Data files         | **RON**                                  | Rust-native readable format for ship/maneuver definitions. |
| Persistence        | **SQLite** via `rusqlite` (server only, later) | Accounts, match history. Not needed for v1 (join codes + in-memory games). |
| Logging            | `tracing` + `tracing-subscriber`         | Structured logs on both ends. |

Alternative noted: if Bevy compile times hurt on the dev machine, `macroquad` is a much
lighter 2D engine with the same platform coverage. The architecture doesn't change —
only `sf-client`'s internals. Bevy is the default recommendation for its ECS, UI, and
ecosystem headroom.

---

## 4. Networking & Security

### Trust model: pinned self-signed certificate + server password

Proven in the sibling `hex-ship-game` project; repeated here with refinements.

- **Server side**: on first run the server generates a self-signed TLS certificate and
  persists it (`tls_cert.pem` / `tls_key.pem`), so its identity is stable across
  restarts. On startup it prints the certificate's SHA-256 fingerprint, the join
  password, and a single copy-pasteable **join string**:

  ```
  starfight://host:4433/#<fingerprint-hex>
  ```

- **Client side**: instead of trusting any CA, a custom rustls `ServerCertVerifier`
  accepts the connection **only if the presented certificate's SHA-256 fingerprint
  matches the pinned value** (full fingerprint if given; a ≥16-hex-char prefix — 64
  bits — is the accepted minimum). Because players receive the fingerprint out-of-band
  from the host, a man-in-the-middle cannot impersonate the server: forging a
  certificate with a colliding fingerprint is computationally infeasible. For a private
  server this is *stronger* than CA trust, and it needs no domain name, no Let's
  Encrypt, no renewal.
- **Remembered pins**: after the first successful connect, the client saves
  `host → fingerprint` in its config file, so the fingerprint is entered exactly once
  per server. A changed fingerprint on a known host is a hard error with a clear
  warning, never a silent re-pin.
- **Server password**: sent inside the established TLS tunnel with the join request;
  gates the server as a whole. Per-game **join codes** then route players to their
  match. Hardening: constant-time password comparison and rate-limited join attempts.
- **Later, if a public server with a domain ever exists**: the verifier is pluggable —
  add normal WebPKI validation alongside pinning without touching anything else.

Implemented (M4, 2026-09-02): `sf-proto::tls` holds the shared pieces (fingerprint,
join-string parser, `PinnedCert` verifier, pinned client config; ring provider only),
`sf-server::tls` the persisted identity (`--tls-dir`, `--insecure` for plaintext dev),
`sf-client::pins` the remembered pins and last-used menu values in the platform config
directory. Password check uses `subtle` constant-time comparison; five failures per
address per minute block further Hellos. Covered by `sf-server/tests/security.rs`.

### Protocol

- Single TLS WebSocket connection per client, kept open for the whole session.
- **Client → Server (`ClientMsg`)**: `Hello{proto_version, name, password}`, `CreateGame`,
  `JoinGame{code}`, `PlaceShip{ship_id, pose}`, `PlanManeuver{ship_id, maneuver_id}`,
  `CommitPlans`, `Resign`, `Ping`.
- **Server → Client (`ServerMsg`)**: `Welcome{player_id, reconnect_token}`,
  `GameCreated{code}`, `LobbyState`, `PhaseChanged`, `PlacementAccepted/Rejected{reason}`,
  `TurnResult{resolved_moves}`, `GameOver`, `Error`.
- **Hidden information stays server-side**: planned maneuvers are never echoed to the
  opponent until both players commit; then a single `TurnResult` reveals and resolves.
- **Reconnection**: `Welcome` includes a random reconnect token; a dropped client
  re-authenticates with it and receives a full state snapshot. Every message the server
  sends is derivable from `GameState`, so resync is just "serialize current state."
- **Validation**: every client message is checked against `sf-core::rules` before it
  mutates anything. Illegal input → `Rejected{reason}`, never a crash or trust.
- v1 identity is "name + server password + join code" over pinned TLS. Per-user
  accounts (argon2-hashed passwords) are a later, additive feature in `registry.rs`
  + SQLite.

### Server internal structure

```
TLS accept loop ─► per-connection task (reads/writes socket)
                        │  mpsc channels
                        ▼
                 lobby task (create/join, hands players to sessions)
                        ▼
                 per-game session task — owns the GameState exclusively
```

One task owns each `GameState`; commands arrive on its channel, so there are **no locks
around game rules**. Crashes in one session can't touch another.

---

## 5. Core Domain Model (`sf-core`)

### Geometry

- Continuous 2D board (not a grid). Coordinates are `f64` on the server-authoritative
  path; the client renders with `f32`. Since the server is the single authority,
  cross-platform float determinism is *not* required — clients display what the server
  decides.
- **1 game unit = 40 mm** of the physical tabletop system: the side of a small ship
  base, and exactly the length of a speed-1 straight template. All maneuver distances,
  ship footprints, and board dimensions are expressed in game units; the mm → unit
  conversion lives in one place (`sf-core/src/templates.rs`).

```rust
pub struct Pose {
    /// Position of the ship's FRONT-CENTER point (the maneuver anchor).
    pub anchor: Vec2,
    /// Facing, radians; 0 = +X, counter-clockwise positive.
    pub heading: f64,
}
```

The **front-center anchor** is the primitive everything hangs off:
- Maneuvers transform the anchor and heading (see §6).
- The ship's rectangular footprint extends *backward* from the anchor:
  half-width left/right, full length behind.
- Sprites are drawn with their front-center pixel mapped onto the anchor (§8).

### Ships

```rust
pub enum SizeClass { Small, Medium, Large }

pub struct ShipClass {          // loaded from ships.ron
    pub id: ShipClassId,
    pub name: String,
    pub size: SizeClass,
    pub footprint: Footprint,   // length × width in game units
    pub maneuver_set: ManeuverSetId,
    pub sprite: String,         // asset path (+ portrait, sprite_px, anchor_px)
    pub attack_dice: u8,        // primary weapon dice (front arc)
    pub pilot_skill: u8,        // low moves first; high fires first
    pub agility: u8,            // defense dice
    pub hull: u8,               // crit-able hit points
    pub shields: u8,            // absorbed first; block crits while up
}

Starter stats — TIE/ln: attack 2, skill 1, agility 3, hull 3, shields 0 (all dodge,
no buffer, crit-able from the first hit). T-70 X-Wing: attack 3, skill 2, agility 2
(provisional), hull 3, shields 3 (6 effective HP; crits blocked while shielded).

pub struct ShipState {
    pub id: ShipId,
    pub owner: PlayerId,
    pub class: ShipClassId,
    pub pose: Pose,
    // later: damage, status effects…
}
```

Ship bases are square (length = width), from the physical system:

| Size   | Base (mm)   | Game units  | Notes                        |
|--------|-------------|-------------|------------------------------|
| Small  | 40 × 40     | 1.0 × 1.0   | = speed-1 straight template  |
| Medium | 60 × 60     | 1.5 × 1.5   |                              |
| Large  | 80 × 80     | 2.0 × 2.0   | = speed-2 straight template  |
| Huge   | 80 × 192    | 2.0 × 4.8   | width × length               |

### Board & Deployment

```rust
pub struct Board {
    pub width: f64,
    pub height: f64,
    pub deploy_depth: f64,   // deployment zone extends this far from each player's edge
}
```

Placement legality = footprint fully inside your deployment zone ∧ no overlap with your
already-placed ships. All checked in `rules.rs`, used identically by client (to grey out
bad placements live) and server (to enforce).

---

## 6. Maneuver System

This is the heart of the game, so it gets the most careful design.

### Maneuvers are data, not code

```rust
pub enum Steer {
    Straight,                   // no heading change, speeds 1..=5
    BankLeft,   BankRight,      // 45° arc, speeds 1..=3
    TurnLeft,   TurnRight,      // 90° arc, speeds 1..=4
    TallonLeft, TallonRight,    // Tallon roll: 90° turn, then +90° flip
    KTurn,                      // Koiogran: straight at speed, then flip 180°
}

pub struct Maneuver {
    pub steer: Steer,
    pub distance: u8,            // speed (see ranges above)
    pub difficulty: Difficulty,  // dial color: Easy=blue, Normal=white, Hard=red
}

pub struct ManeuverSet {         // maneuvers.ron — one per agility tier
    pub id: ManeuverSetId,
    pub maneuvers: Vec<Maneuver>,
}
```

A ship class points at a `ManeuverSet`; maneuverable ships get the richer set. When you
provide the per-ship maneuver lists, they drop into `maneuvers.ron` — no code changes.

### Executing a maneuver

Each maneuver expands to a **path**: a short sequence of segments applied to the
front-center anchor in the ship's local frame:

- `Line(len)` — advance `len` units along current heading.
- `Arc(radius, sweep)` — circular arc; heading rotates by `sweep`, signed for left/right.
- `Rotate(angle)` — turn in place (used only by the U-turn's 180° flip).

Template dimensions come from the physical system (all templates 20 mm wide; the
anchor travels the template **centerline**, i.e. inside radius + 10 mm). Converted at
1 unit = 40 mm in `templates.rs`:

| Template          | Inside radius | Centerline | Game units | Path |
|-------------------|---------------|------------|------------|------|
| Straight n (1..5) | —             | 40·n mm    | n          | `Line(n)` |
| Bank 1 (45°)      | 70 mm         | 80 mm      | 2.0        | `Arc(2.0, ±45°)` |
| Bank 2 (45°)      | 120 mm        | 130 mm     | 3.25       | `Arc(3.25, ±45°)` |
| Bank 3 (45°)      | 170 mm        | 180 mm     | 4.5        | `Arc(4.5, ±45°)` |
| Turn 1 (90°)      | 25 mm         | 35 mm      | 0.875      | `Arc(0.875, ±90°)` |
| Turn 2 (90°)      | 53 mm         | 63 mm      | 1.575      | `Arc(1.575, ±90°)` |
| Turn 3 (90°)      | 80 mm         | 90 mm      | 2.25       | `Arc(2.25, ±90°)` |
| Turn 4 (90°)      | 107 mm (extrapolated) | 117 mm | 2.925  | `Arc(2.925, ±90°)` |
| Tallon roll n     | turn template n | —        | —          | `Arc(r_turn(n), ±90°)`, `Rotate(±90°)` |
| Koiogran n (1..5) | —             | 40·n mm    | n          | `Line(n)`, `Rotate(180°)` |

No physical speed-4 turn template exists; this game's canonical radius extends the
1–3 progression (+27 mm per speed), adopted deliberately in preference to the
physical game's makeshift speed-4 turn rules.

Difficulty is the dial color and drives the (future) stress system: flying a **blue**
(Easy) maneuver removes a stress token, **red** (Hard) adds one, and a stressed ship
may not select red maneuvers. Which steer/speed/color combinations a ship gets is
entirely dial data in `maneuvers.ron` — e.g. the standard TIE Fighter dial has no
speed-1 hard turns, blue straights 1–3, and red Koiogran turns at 3 and 4.

`apply(pose, maneuver) -> (final Pose, sampled path)` is a pure function in `sf-core`.
Because it's pure and shared:

- the **client** calls it to draw the ghost-ship preview and path arc while the player
  browses the maneuver list;
- the **server** calls the *same function* to resolve the turn.

### Collision & bounds

The path is sampled at small steps; footprint rectangles are tested via the
separating-axis test. Per core rules p.17, ships **move through** occupied space
freely — only the FINAL position matters. If the final base overlaps another ship,
the mover backs up along its template to the last clear pose ("bump") and forfeits
its action; a K-turn or Tallon roll that would end overlapping executes as the
plain straight/turn of the same speed and color instead (no flip, stress still
applies). Touching ships cannot target each other in combat. Fleeing: a maneuver
whose final base is even partly outside the play area destroys the ship.

### Firing arcs & range bands (`combat.rs`)

Both starter ships use the standard **Front Firing Arc**: a 90° forward cone (±45° of
heading) originating at the **center of the ship's base**. Range is measured from the
closest point of the attacker's base inside the arc to the closest point of the
defender's base, in three bands of exactly 100 mm = **2.5 game units** each:

| Band | Distance          | Attacker           | Defender          |
|------|-------------------|--------------------|-------------------|
| 1    | 0–2.5 u (100 mm)  | +1 attack die      | —                 |
| 2    | 2.5–5 u (200 mm)  | standard dice      | —                 |
| 3    | 5–7.5 u (300 mm)  | standard dice      | +1 defense die    |

Primary attack dice are a `ShipClass` stat (`attack_dice`: TIE Fighter 2, X-Wing T-70
3). The client overlays the cone and band arcs on the selected ship and on maneuver
previews, so players can see where a move will point their guns before committing.

### Combat dice (`dice.rs`)

Custom d8s, deterministic in core (the server supplies raw d8 values from its seeded
RNG, so resolutions are replayable):

| Die              | Faces                                | Natural success |
|------------------|--------------------------------------|-----------------|
| Red (attack)     | Hit x3, Crit x1, Focus x2, Blank x2  | 50%             |
| Green (defense)  | Evade x3, Focus x2, Blank x3         | 37.5%           |

Modifiers: spending a Focus token converts Focus faces to Hits (attack) or Evades
(defense); a Target Lock rerolls blanks (action economy lands with game state).
Resolution: evades cancel hits first, then crits; hits resolve before crits;
shields absorb damage point-per-point and **block critical effects** — only crits
that reach the hull become face-up critical cards.

### Squad points & initiative

Ships, pilots, and modifiers all cost **squad points** (`squad_points` on each — ship
values provisional until final costs land). At setup the server totals each squad and
assigns **initiative** automatically: the *lower* total takes it; on a tie, seat 0
rolls one red die — Hit/Crit keeps initiative, Focus/Blank hands it to the opponent
(the tabletop "chooser" step is automated as choosing yourself). Initiative breaks
every pilot-skill tie: the initiative player's ships move first *and* fire first at
equal skill. The client must display who holds initiative and the resulting turn
order — especially once games grow beyond two players (a design goal: `GameState`'s
two-seat assumptions are localized in `committed`, `Seat`, and `initiative_seat`, the
places to generalize when 3+ player matches arrive).

### Squad builder & scenarios (design)

- **Squad builder in the client**: choose a faction, add ships, then pilots and
  ordnance/upgrades (content details TBD). Squads are saved locally as RON files in
  the user's config directory and offered when joining a game.
- **Scenarios**: a server game session can carry restrictions — max squad points,
  banned size classes, faction locks, etc. — expressed as a `ScenarioRules` struct in
  sf-core, sent to clients with the lobby info.
- **Validation is shared, enforcement is server-side**: one
  `validate_squad(squad, rules)` function in sf-core. The client runs it live in the
  builder and before joining (instant, explanatory feedback); the server re-runs it
  on `JoinGame` and rejects violations — the client check is a courtesy, the server
  check is the guarantee. Same shared-crate principle as maneuver legality.

### Actions (`action.rs`, core rules p.8-9)

Each ship performs **one action right after its maneuver**, from its class
`action_bar`: **Focus** (token: eyes→hits or evades when spent), **Evade** (token:
+1 evade result), **Barrel Roll** (straight-1 template sideways: lateral shift of
1 unit + base width, heading unchanged; simplified — no fore/aft slide), **Boost**
(fly a straight-1 or bank-1 template; not a maneuver, so no stress interaction;
blocked if it would overlap or leave the board), **Acquire Target Lock** (enemy at
range 1-3, measured closest-point 360°; persists until re-locked or spent), or
**Pass**. A stressed ship cannot act; a ship that bumped
loses its action; an impossible action (blocked roll, out-of-range lock) fails.
Focus/evade tokens are removed in the End phase; locks and stress persist.

**Async adaptation:** actions are planned secretly during Planning alongside the
dial (`PlanAction`) and auto-executed — the tabletop chooses actions live after
each move. Revisit if interactive activation is ever wanted.

### Combat phase (`game.rs::perform_attack`, core rules p.10-13)

After all movement+actions, each ship attacks once: highest pilot skill first,
initiative breaking ties; equal-skill ships fire "simultaneously" (everyone alive
at their skill group's start still shoots, even if destroyed within the group).
Target must have any part of its base in the attacker's 90° front arc at range
1-3; touching bases cannot be targeted. Attack dice = `attack_dice` (+1 at range
1); defense dice = `agility` (+1 at range 3). Evades cancel hits before crits;
hits resolve before crits; shields absorb first and block crit effects
(`crits_to_hull` recorded for the future modifier system). Destroying every enemy
wins; a simultaneous final kill goes to the initiative holder.

**Async adaptation (documented policy, revisit later):** the server auto-picks
the target (locked ship if eligible, else nearest in arc) and auto-spends tokens
(lock rerolls misses; focus when eyes matter; evade when damage would land).
Dice come from the server RNG via `commit_plans`'s injected roll source, so
resolutions stay deterministic/replayable; every die face lands in `AttackRecord`
for the client's combat log and laser-bolt animation (Rebel Alliance red, Empire
green; impact flash on hits — blue for shields, orange for hull — misses fly
past and fade).

### Card-less adaptations still OPEN (discuss before implementing)

- **Critical damage effects** (p.16): faceup cards carry effects; we record
  `crits_to_hull` but apply no effect yet. Plan: a small table of modifier tags
  drawn randomly per crit — contents need agreement.
- **Revealing red while stressed** (p.17): the opponent chooses a replacement
  non-red maneuver. Currently unreachable (stress only comes from own maneuvers,
  and planning already blocks red while stressed) — becomes live once crits or
  abilities can add stress mid-turn; an auto-policy will be needed.
- Color note: the physical rules call easy maneuvers **green**; this game uses
  blue (second-edition style) — same mechanics.

### Pilots, upgrades & modifiers (design)

- **Pilots are data** (`assets/data/pilots.ron`, `sf-core::pilot`): a `Pilot` carries
  ship class, skill, squad cost (ship + pilot), talent slot, source pack and an ability
  tag. A fleet is a list of pilots; `ShipState.pilot` gives each ship its skill and
  cost. Every pilot and ship class has an XWS id, which is also the card image file
  name in an XWS-layout card directory (a local, gitignored clone of
  voidstate/xwing-card-images; never redistributed with the game). Abilities are
  documented variants of `PilotAbility` and are enforced only once
  `implemented()` says so — data first, rules one at a time with tests.
- **One modifier system for everything.** Ordnance, pilot abilities, and critical-hit
  effects are all the same mechanism: effect tags attached to a ship instance that
  rules code consults during movement/combat. Critical hits are NOT shown as cards in
  the UI — a crit simply attaches its modifier, and the ship status panel lists active
  modifiers as text.
- **Runtime ship state** (with game state in M3): current hull/shields, stress tokens,
  action tokens (Focus / Target Lock / Evade), assigned pilot, active modifiers.

### Declare Target (interactive combat step)

Combat is resolved **step by step** on the server (`commit_plans_begin` →
`combat_step` / `declare_target`). Attacks with zero or one eligible target resolve
automatically; when an attacker has **several** enemies in arc and range, the server
pauses, sends `ChooseTarget` to the owner (and `OpponentChoosing` to the other side),
and resumes when `DeclareTarget` arrives. Each resolved attack is streamed as an
`AttackResult`; `MovementResult` opens the sequence and `TurnEnd` closes it. The
client keeps a playback queue (moves → attacks → prompt → …) so the prompt is only
shown once movement has animated and positions are visible; the player answers by
clicking a highlighted ship or pressing its number. Token spending (lock/focus/evade)
remains automatic. Tests and offline play use `commit_plans`, which drives the same
machine with the locked-else-nearest policy.

### Turn structure (phase machine in `game.rs`)

```
Setup ─► Placement ─► [ Planning ─► Resolution ]* ─► GameOver
```

1. **Placement** — alternating or simultaneous secret placement in deployment zones.
2. **Planning** — each player secretly assigns one maneuver to every ship, then commits.
3. **Resolution** — server reveals all plans and resolves in **pilot-skill order**
   (a `ShipClass` stat): movement executes lowest skill first (a rookie commits early;
   an ace moves last with full information), then combat fires highest skill first
   (the ace shoots before slower pilots can respond). TIE Fighter pilots are skill 1,
   T-70 pilots skill 2 — so TIEs move first and X-Wings fire first. Skill ties break
   deterministically by ship id. The server broadcasts `TurnResult` with each ship's
   path + final pose so clients can animate it.
4. Loop until a win condition (fleet destroyed / objective — later).

---

## 7. Client Architecture (`sf-client`)

Bevy app organized around a state enum mirroring the game phases:

- `AppState::{MainMenu, Connecting, Lobby, Placement, Planning, Resolution, GameOver}`
- **Networking bridge**: the WebSocket lives on a small background tokio runtime;
  crossbeam/mpsc channels bridge it to Bevy systems (`ServerMsg` in, `ClientMsg` out).
  Bevy systems never block on the network.
- **Placement UI**: drag ship from fleet panel; footprint tinted green/red via
  `sf-core::rules` legality check; rotate with scroll/keys; confirm sends `PlaceShip`.
- **Planning UI**: select ship → maneuver list from its `ManeuverSet` → hovering a
  maneuver renders the path arc + translucent ghost ship at the final pose (via the same
  `apply` function the server uses). Commit button sends `CommitPlans`.
- **Ship status panel**: toggling between your ships (Tab) drives a corner subscreen
  showing the selected ship's pilot, hull/shield status, stress and action tokens, and
  active modifiers — plus the action picker (Focus, Target Lock, …) for the turn.
- **Resolution**: animate each ship along the server-provided sampled path; the final
  pose always snaps to the server's answer.
- The client keeps a mirror `GameState` updated only from `ServerMsg` — it never
  self-mutates game state.

---

## 8. Art & Asset Pipeline

**Sprite format: PNG (32-bit RGBA).** That's the answer for the ship images you'll provide:

- Lossless with a real alpha channel (transparent background is essential for
  odd-shaped spacecraft over the starfield). JPEG has no alpha; GIF's is 1-bit.
- Universally supported by Bevy/image crates, easy to author anywhere.
- If you author the art as vectors, keep the SVG as the *source of truth* in
  `assets/src/` and export PNGs — the game itself loads PNG.

Authoring conventions (put these in the file so exports stay consistent):

1. **Ship faces UP** (nose toward the top of the image). The renderer rotates from there.
2. **Transparent background**, ship roughly centered horizontally.
3. **Nose tip at a known pixel** — ideally touching the top edge, centered. Each ship
   entry in `ships.ron` records `anchor_px: (x, y)` so the front-center pixel maps
   exactly onto the geometric anchor. That keeps art and movement math in perfect
   agreement even if a sprite has a little empty margin.
4. **Resolution**: target ~256 px per game unit of ship length —
   small ≈ 256 px tall, medium ≈ 410, large ≈ 640 (power-of-two canvases like 256²/512²
   are tidy but not required). Downscaling looks good; upscaling doesn't.

---

## 9. Testing Strategy

- `sf-core` is pure → dense unit tests: maneuver end-poses (known-answer tests for each
  template), footprint collision cases, placement legality, full phase-machine walkthroughs.
- Property tests (`proptest`): e.g. "slight-left N then slight-right N returns heading
  to start", "no legal maneuver ever produces NaN", "resolution is order-stable".
- `sf-proto`: round-trip serialization tests; a version-mismatch rejection test.
- Integration: spawn `sf-server` in-process, drive two scripted fake clients through a
  whole game over a real socket.
- CI later: `cargo test --workspace` + `cargo clippy` on Linux/macOS/Windows runners
  (GitHub Actions) to keep the client honestly cross-platform.

---

## 10. Build Order (Milestones)

1. **M0 — Workspace skeleton**: crates, CI-less `cargo check`, empty message enums.
2. **M1 — Core geometry**: `Pose`, maneuver templates, `apply()`, footprint collision.
   Fully test-driven; no graphics or network needed.
3. **M2 — Local sandbox client**: Bevy window, board, placeholder triangle ships,
   placement + maneuver preview running *entirely offline* against `sf-core`.
   (This is where your ship PNGs and the real maneuver lists slot in.)
4. **M3 — Server + protocol**: lobby with join codes, two clients, full game loop over
   plaintext WebSocket on localhost.
5. **M4 — TLS + reconnection**: rustls, reconnect tokens, deploy on the Linux host.
6. **M5 — Combat & polish**: damage rules, win conditions, animations, sound, accounts.

Each milestone is playable/testable on its own, and M1–M2 need zero network code —
the shared-crate design is what makes that possible.
