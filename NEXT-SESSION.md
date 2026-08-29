# Where we left off (2026-08-29)

## State: M3 roughly 70% complete — server done and tested, client wiring next

`cargo test --workspace` is fully green (44 sf-core tests + proto round-trip +
a two-client server integration test). Every layer below the connected client
is built, tested, and committed.

## Working today

- `cargo run -p sf-client` — offline sandbox: placement (drag, Q/E/scroll
  rotate), flight mode with real TIE/T-70 dials, ghost + path previews,
  firing-arc overlay (F toggles), facing indicators.
- `cargo run -p sf-server` — real WebSocket server on port 7777: lobby with
  join codes, per-game session tasks, full validated turn loop (proven by
  `crates/sf-server/tests/gameflow.rs` — read that test to see the whole
  protocol conversation).

## NEXT TASK: finish M3 — the connected client

Build in `sf-client` (keep the offline sandbox available, e.g. `--sandbox`
flag or menu entry):

1. Main menu: player name, server address, Create Game / Join by code.
2. Network bridge: background tokio task owning the WebSocket;
   crossbeam/std mpsc channels to/from Bevy systems (pattern already
   sketched in ARCHITECTURE.md §7).
3. Connected placement: drag ships in own zone only, send PlaceShip,
   render Rejected reasons; opponent ships appear when Planning starts.
4. Connected planning: dial picker + ghost preview (reuse sandbox code),
   PlanManeuver/CommitPlans, "waiting for opponent" state.
5. Resolution animation: fly ships along TurnResult paths in order.
6. HUD: initiative token display + squad totals (in every Snapshot),
   ship status subscreen (hull/shields/stress) per design in doc.
7. Procedural starfield + nebula background (user-requested): generate at
   startup — layered random stars + value-noise nebula into a texture.
   Purely cosmetic, client-only.

Then M4: TLS via pinned self-signed cert + server password (design in
ARCHITECTURE.md §4 — mirror hex-ship-game's proven approach, plus
remembered pins + single join-string UX).

## Recently decided (already in ARCHITECTURE.md, don't re-litigate)

- Initiative: lower squad total takes it; tie → seat 0 rolls red die
  (Hit/Crit keeps, else opponent). Breaks ALL pilot-skill ties (move first
  + fire first). Snapshot carries initiative + squad_totals.
- Squad points on ShipClass are PROVISIONAL (TIE 12, T-70 24) — user will
  supply real costs later.
- Squad builder design: client builds/saves squads (faction → ships →
  pilots/ordnance, content TBD); scenarios restrict (points cap, size
  bans); shared sf-core validate_squad() — client checks for UX, server
  enforces at JoinGame.
- Modifiers: crits/ordnance/pilot abilities are all one effect-tag system;
  no face-up card UI ever.
- Pilots become data (pilots.ron) with per-ship assignment; pilot_skill on
  ShipClass is the basic pilot's value until then.
- Speed-4 turn radius (2.925 u) is canonical, deliberately better than the
  physical game's makeshift rules.

## Still needed from the user (ask when relevant)

- Action economy: when actions happen in the turn, token lifetimes,
  whether stress blocks actions.
- Critical-effect list (as modifier tags), real squad-point costs,
  pilot roster, ordnance/upgrade content.
- Combat is NOT yet in turn resolution: dice/arc/range machinery all
  exists and is tested (combat.rs, dice.rs), but resolve() only moves
  ships — wiring combat into the phase flow needs the action economy.
