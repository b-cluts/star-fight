# Where we left off (2026-08-31)

## State: M3 COMPLETE (pending human playtest) — client is fully connected

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

## Housekeeping backlog

- 15 clippy style warnings (collapsible-if, too-many-args, range patterns —
  list via `cargo clippy --workspace`). Harmless; batch-fix sometime.
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

- Action economy (when actions happen, token lifetime, stress interaction) —
  blocks wiring combat into turn resolution (dice/arc/range all built).
- Critical-effect list, real squad costs, pilot roster, ordnance content.
- 3+ player support is designed-for (initiative order display) but the
  GameState/Seat model is 2-player; generalizing is future work.
