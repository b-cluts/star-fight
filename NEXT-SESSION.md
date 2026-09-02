# Where we left off (2026-09-02, session 4)

## State: full networked game loop with combat, actions, and crits

`cargo build` clean, `cargo test --workspace` green (83 tests),
`cargo clippy --workspace -- -D warnings` clean, `cargo fmt --check`
clean (rustfmt.toml: max_width 100, use_small_heuristics Max). Rulebook coverage:
core_rules_en.pdf pages 8-13 and 16-17 are fully implemented (the PDF
sits at the repo root, gitignored — read further pages on demand;
p.18+ covers upgrade cards / squad building, not yet read).

What exists end-to-end:
- Menu (over procedural starfield) → Offline Sandbox or Create/Join game.
- Networked play: hidden placement (drag, Q/E rotate, A submits all),
  secret planning of maneuver (dial keys) + action (keys 1-6; 6 then
  click enemy = target lock), C commits, X resigns.
- Resolution: movement lowest-skill-first (initiative breaks ties;
  move-THROUGH allowed, only final overlap bumps and backs up along the
  template; K-turn/Tallon degrade to unflipped maneuver on overlap;
  fleeing the board destroys), one action after each move (stress/bump/
  sensors forfeit), Combat highest-skill-first (arc + closest-point
  range, bullseye lane denies defender tokens, lock/focus/evade auto-
  spend policy, simultaneous fire at equal skill, initiative wins mutual
  kill), End phase (focus/evade cleared, locks persist, timed crits tick).
- All 14 critical damage effects as modifier tags (crit.rs); events
  narrated in the combat log; ship status shows tokens + crits.
- Turn playback queue: flown paths, then faction-colored laser bolts
  (Alliance red, Empire green), shield/hull impact flashes, misses fly
  past and fade; bullseye lane shaded amber on previews.
- DECLARE TARGET (session 3): when an attacker has 2+ eligible enemies
  the server pauses combat and prompts the owner (ChooseTarget); the
  client shows the attacker's arc + highlighted candidates after the
  movement animation; click or press a number; opponent sees a waiting
  notice. Zero/one candidate stays automatic. Core: Phase::Combat +
  CombatState, commit_plans_begin / combat_step / declare_target;
  commit_plans keeps the auto policy for tests.
- Camera view (session 3): +/- zoom, right-drag pan, Home resets; an
  inset minimap (bottom-right, second camera) appears automatically
  whenever the board doesn't fully fit the main view.
- M4 SECURITY (session 4, done, NOT yet playtested by the user):
  pinned self-signed TLS + server password + rate limiting. Server mints
  tls_cert.pem/tls_key.pem on first run (gitignored, --tls-dir), prints
  the SHA-256 fingerprint, the password (--password or random) and the
  join string `starfight://host:port/#<fp>`; `--insecure` keeps
  plaintext ws:// with no password for local testing. Client: menu has
  Name / Server / Password / Cert fingerprint / Game code; the Server
  field takes the join string (Ctrl+V pastes via arboard; also accepted
  as `sf-client <join-string>` CLI arg); a ≥16-hex prefix pins; the full
  fingerprint is remembered per host:port in the config dir
  (~/.config/starfight/pins.txt on Linux) after the first handshake, and
  a contradicting pin later is a hard error naming that file. Shared
  code: sf-proto::tls (fingerprint, parse_target, PinnedCert verifier).
  Server: ServerOpts, constant-time compare (subtle), 5 failures/minute
  per IP blocks Hello. Tests: sf-proto unit tests + sf-server
  tests/security.rs.

To try it: `cargo run -p sf-server` (copy the printed join string and
password) then two `cargo run -p sf-client` instances — paste the join
string into Server, type the password, create in one, join with the
code in the other. Quick local loop without TLS: `sf-server --insecure`
and Server `ws://127.0.0.1:7777`.

## NEXT TASK

1. ~~Playtest M4~~ done 2026-09-02: joining over TLS worked. Fix applied
   after it: menu field text now wraps at any character so the long join
   string stays inside the field (not yet re-checked visually by the
   user).
2. **Rulebook p.18+** — upgrade cards / squad points, feeding the squad
   builder + pilots/ordnance design already sketched in ARCHITECTURE.md.
   Tuning knobs if ever needed: ANIM_SAMPLES_PER_SEC / ATTACK_DUR in
   online.rs, MINI_PX in render.rs.

## Client structure (crates/sf-client/src/)

main.rs (Screen state: Menu/Sandbox/Online, global setup, two cameras),
render.rs (Game resource, ship_visual, draw helpers, bullseye shade,
ViewCtl pan/zoom + minimap), menu.rs (fields, paste, pin resolution),
online.rs (server mirror; Anim is a queue of AnimItem:
Move/Attack/Prompt/Waiting/TurnEnd; never mutates game state locally;
remembers the pin on NetEvent::Secured), sandbox.rs, net.rs (background
thread + channels; pinned TLS via tokio-rustls), pins.rs (config dir:
pins.txt + last-used menu values), starfield.rs.

## Housekeeping / workflow

- POLICY: clippy warnings are errors — `cargo clippy --workspace -- -D
  warnings` must stay at zero. sf-client has a documented crate-level
  allow for type_complexity + too_many_arguments only (Bevy idiom).
- WORKFLOW RULE (user asked repeatedly, now also in CLAUDE.md): ALWAYS
  delegate cargo check/build/test/clippy/fmt to a Haiku subagent that
  reports PASS/FAIL + error excerpts; never run them inline on the main
  model; main model investigates and edits. Check crates individually
  when feature unification could mask breaks. Tell the subagent
  explicitly that IT is the verifier and must not delegate further —
  otherwise it reads CLAUDE.md, tries to re-delegate, and returns
  nothing (happened 2026-09-02).
- Frequent small commits, one concern each; tests scripted via the
  `roll: &mut dyn FnMut() -> u8` d8 injection (7=blank, 0=hit/evade).

## Decided (in ARCHITECTURE.md / code, don't re-litigate)

- Initiative: lower squad total; tie → seat-0 red-die roll (Hit/Crit
  keeps). Breaks ALL skill ties. Provisional costs: TIE 12, T-70 24.
- Async adaptations of the tabletop, all documented: actions planned
  secretly with the dial; combat targets + token spending auto-resolved
  server-side (interactive later if wanted); "choosing" initiative is
  automated as choosing yourself.
- Crits/ordnance/abilities are ONE modifier-tag system; no card UI.
- Squad builder: client builds/saves squads; scenarios restrict; shared
  validate_squad() client+server (not yet implemented).
- Pilots become data with per-ship assignment (not yet implemented);
  pilot_skill on ShipClass is the generic pilot until then.
- Speed-4 turn radius (2.925 u) is canonical.

## TODO backlog (user requests)

- **Ship callsigns + hover tooltip** (requested 2026-09-02): during squad
  formation the player names each ship with a squad callsign — squad name
  plus number, "leader" for the squad leader (e.g. Obsidian-leader,
  Obsidian-2, Red-leader, Red-2). In play, hovering the mouse over a ship
  shows its name. Plan: `callsign: String` on the fleet entry / ShipState /
  ShipView (server-assigned defaults like "Red-1" until the squad builder
  exists), a hover tooltip in the client (cursor-in-footprint → small text
  label near the ship, both sandbox and online), and callsigns replacing
  "#id" in the combat log / HUD / Declare Target prompt.

## Open items / needed from the user

- Stressed-red-reveal rule: PROVISIONAL auto-substitution (slowest white
  straight, effective-color aware; marked in game.rs) — user is
  considering an alternative approach.
- Real squad costs, pilot roster (abilities would activate Injured
  Pilot), ordnance content, faction rosters for the squad builder.
- 3+ players: designed-for but GameState/Seat model is 2-player.
- Boost exists as an action (T-70 bar) — sandbox/online action keys
  cover it; no dedicated preview arrows yet.
