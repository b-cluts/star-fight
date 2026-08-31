# Where we left off (2026-08-31, session 2)

## New this session

- Placement-visibility playtest bug FIXED (ships now seeded/draggable).
- ACTIONS implemented from core rules p.8-9 (Focus/Evade/BarrelRoll/
  TargetLock/Pass, action bars, tokens, stress/bump forfeits) — planned
  secretly with the dial (async adaptation, revisit if interactive
  activation wanted). Keys 1-6 in planning; 6 then click enemy = lock.
- COMBAT PHASE implemented from p.10-13: full attack pipeline, auto
  target/token policy (documented in ARCHITECTURE.md), simultaneous
  equal-skill rule, initiative wins mutual kill. Server injects RNG.
- Laser bolt animations: faction-colored (RebelAlliance red, Empire
  green), impact flashes (blue=shields, orange=hull, burst=kill),
  misses fly past and fade. Combat log in HUD.
- Faction field on ShipClass. Fixed mid-animation rubber-banding.
- The user's core_rules_en.pdf (repo root) is the rules source — read
  specific pages on demand; pages 8-13 are done. NOT in git (18 MB).

## State: M3 COMPLETE — full networked game loop with combat

`cargo build` clean, `cargo test --workspace` green (46 tests). The client now
opens on a main menu over a procedural starfield/nebula backdrop:

- **Offline Sandbox** — the previous sandbox, unchanged behavior (Esc → menu).
- **Create Game / Join Game** — full networked play: name/server/code fields
  (click to focus, Tab cycles), placement (drag, Q/E/scroll rotate, each
  release submits; A submits all), secret planning (Tab/←→/Enter, C commits,
  X resigns), animated turn resolution along server paths, initiative +
  squad totals + ship status + stress-lock warnings in the HUD.

To try it: `cargo run -p sf-server` then two `cargo run -p sf-client`
instances — create in one, join with the code in the other.

## Client structure (crates/sf-client/src/)

main.rs (Screen state: Menu/Sandbox/Online, global setup), render.rs (shared
Game resource, ship_visual, draw helpers), menu.rs, online.rs (server mirror +
Snap/Anim, never mutates game state locally), sandbox.rs, net.rs (background
thread + channels), starfield.rs.

## NEXT TASK (either order)

1. **Human playtest of M3** — user runs server + two clients; fix whatever
   feels wrong (animation speed constant ANIM_SAMPLES_PER_SEC=40, HUD copy,
   placement UX).
2. **M4 — security**: TLS with pinned self-signed cert + server password,
   mirroring ../hex-ship-game (design in ARCHITECTURE.md §4): server
   generates/persists cert, prints SHA-256 fingerprint + join string
   `starfight://host:port/#<fp>`; client custom rustls verifier (≥16 hex
   prefix), remembered pins in config file (changed pin = hard error),
   constant-time password check, menu gains password + fingerprint fields.
   Server password field already exists in Hello (currently ignored).

## Housekeeping / workflow

- POLICY: clippy warnings are treated as errors — verify with
  `cargo clippy --workspace -- -D warnings` (currently clean). sf-client
  has a documented crate-level allow for type_complexity and
  too_many_arguments only (idiomatic Bevy can't satisfy those two).
- Workflow: user wants noisy verification (build/clippy/test) delegated to a
  Haiku subagent that reports summaries; main model does edits. This caught
  a real bug already (tokio feature-unification masking a missing "macros"
  feature — check crates alone, not together).

## Recently decided (in ARCHITECTURE.md, don't re-litigate)

- Initiative: lower squad total; tie → seat-0 red-die roll (Hit/Crit keeps).
  Breaks ALL pilot-skill ties. Provisional costs: TIE 12, T-70 24.
- Squad builder: client builds/saves squads; scenarios restrict; shared
  validate_squad() — client for UX, server enforces (not yet implemented).
- Modifiers: one effect-tag system for crits/ordnance/abilities; no card UI.
- Pilots become data with per-ship assignment (not yet implemented).
- Speed-4 turn radius (2.925 u) is canonical.

## Still needed from the user

- **USER TODO (agreed 2026-08-31): research and supply the critical-hit
  effect table** — a small list of persistent penalties applied as
  modifier tags when a crit reaches the hull (crits_to_hull is already
  recorded per attack; effects currently do nothing). No card UI.
- User is also reconsidering the stressed-red-reveal rule: currently
  auto-substituted with the slowest white straight (PROVISIONAL, marked
  in game.rs) — they may propose a different approach.
- Real squad costs, pilot roster, ordnance content.
- 3+ player support is designed-for (initiative order display) but the
  GameState/Seat model is 2-player; generalizing is future work.
