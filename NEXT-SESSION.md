# Where we left off (2026-08-31, end of session 2)

## State: full networked game loop with combat, actions, and crits

`cargo build` clean, `cargo test --workspace` green (74 tests),
`cargo clippy --workspace -- -D warnings` clean. Rulebook coverage:
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
- Two-stage turn animation: flown paths, then faction-colored laser
  bolts (Alliance red, Empire green), shield/hull impact flashes,
  misses fly past and fade; bullseye lane shaded amber on previews.

To try it: `cargo run -p sf-server` then two `cargo run -p sf-client`
instances — create in one, join with the code in the other.

## NEXT TASK (user picks)

1. **Playtest** — crits/combat are new since the last playtest; expect
   tuning requests (animation pacing ANIM_SAMPLES_PER_SEC/ATTACK_DUR in
   online.rs, HUD copy, log length).
2. **M4 — security**: TLS with pinned self-signed cert + server password,
   mirroring ../hex-ship-game (design in ARCHITECTURE.md §4): server
   generates/persists cert, prints SHA-256 fingerprint + join string
   `starfight://host:port/#<fp>`; client custom rustls verifier (≥16 hex
   prefix), remembered pins in config (changed pin = hard error),
   constant-time password check, menu gains password + fingerprint
   fields. Hello already carries password (currently ignored).
3. **Rulebook p.18+** — upgrade cards / squad points, feeding the squad
   builder + pilots/ordnance design already sketched in ARCHITECTURE.md.

## Client structure (crates/sf-client/src/)

main.rs (Screen state: Menu/Sandbox/Online, global setup), render.rs
(Game resource, ship_visual, draw helpers, bullseye shade), menu.rs,
online.rs (server mirror + Snap/Anim two-stage animation, never mutates
game state locally), sandbox.rs, net.rs (background thread + channels),
starfield.rs.

## Housekeeping / workflow

- POLICY: clippy warnings are errors — `cargo clippy --workspace -- -D
  warnings` must stay at zero. sf-client has a documented crate-level
  allow for type_complexity + too_many_arguments only (Bevy idiom).
- Workflow: delegate noisy verification (build/clippy/test) to a Haiku
  subagent reporting summaries; main model does edits. Check crates
  individually (feature unification across crates can mask breaks).
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

## Open items / needed from the user

- Stressed-red-reveal rule: PROVISIONAL auto-substitution (slowest white
  straight, effective-color aware; marked in game.rs) — user is
  considering an alternative approach.
- Real squad costs, pilot roster (abilities would activate Injured
  Pilot), ordnance content, faction rosters for the squad builder.
- 3+ players: designed-for but GameState/Seat model is 2-player.
- Boost exists as an action (T-70 bar) — sandbox/online action keys
  cover it; no dedicated preview arrows yet.
