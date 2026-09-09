# Where we left off (2026-09-09, session 6)

## State: full networked game loop with combat, actions, and crits

`cargo build` clean, `cargo test --workspace` green (220 tests),
`cargo clippy --workspace -- -D warnings` clean, `cargo fmt --check`
clean (rustfmt.toml: max_width 100, use_small_heuristics Max). Rulebook coverage:
core_rules_en.pdf pages 8-13 and 16-19 are implemented (the PDF sits at
the repo root, gitignored — read pages on demand; p.20 obstacles, p.20
team play and p.21-24 missions are read but NOT implemented).

What exists end-to-end:
- Menu (over procedural starfield) → Offline Sandbox, Effects Demo, or
  Create/Join game. The Effects Demo (online.rs `start_demo` /
  `fx_demo` / `demo_queue`) plays a scripted loop on a fake snapshot —
  every impact style, missile flight, turret shots, all seven bomb
  tokens and their detonations — through the real animation queue, with
  the HUD naming each effect; Esc leaves. Use it to review animations
  without a game.
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

- CALLSIGNS + HOVER (session 4, done, playtested OK): every ship
  has a squad callsign (ShipState/ShipView.callsign). Defaults per
  fleet faction: first Rebel squad Red, second Gold; Imperial Obsidian
  then Onyx; first ship "-leader", then "-2", "-3"… (ship.rs
  squad_names/default_callsign). During Placement, N renames the
  selected/hovered own ship (type, Enter sends ClientMsg::Rename, Esc
  cancels; ≤20 chars, unique ignoring case, GameState::rename). Hovering
  a ship shows a world-space name tag (callsign + class) in sandbox and
  online (render::Hover/hovered/apply_hover, Text2d HoverLabel). Combat
  log, HUD status line, lock label and the Declare Target prompt use
  callsigns instead of "#id".

- PILOTS AS DATA (session 4, done): assets/data/pilots.ron holds every
  First Edition pilot for the TIE/ln (13), T-70 (10) and TIE/fo (9),
  each with skill, cost, talent slot, source pack (CoreSet = Force
  Awakens core; OriginalCoreSet, TieFighterExpansion, T70Expansion,
  HeroesOfTheResistance, ImperialAssaultCarrier, TieFoExpansion) and an
  ability tag (PilotAbility in sf-core/src/pilot.rs, card text in the
  doc comments). Card texts were VERIFIED against the card images in
  reference/ (see below). Fleets are lists of PilotId; skill and squad
  cost come from the pilot; fixed fleets fly each class's basic pilot
  (Academy Pilot, Blue Squadron Novice). ShipView carries pilot name +
  skill (HUD shows "callsign (class, pilot PSn)").
  NO ability is enforced yet: PilotAbility::implemented() returns false
  for all; implement them one at a time with tests (start with the dice
  ones: Poe, Mauler, Backstabber, Scourge, Winged Gundark, Jess, Dark
  Curse, Howlrunner, Zeta Leader, Omega Ace, Omega Leader; then tokens:
  Red Ace, Night Beast, Nien Nunb, Epsilon Leader, Chaser; then
  movement: Snap, Blue Ace, Zeta Ace, Ello Asty; Epsilon Ace skill 12;
  Wampa; Youngster needs talents).
- TIE/fo class (id 3) with its real dial incl. Segnor's loops
  (Steer::SegnorLeft/Right, bank then flip); placeholder sprite shares
  the TIE/ln art. The T-70 lost its native barrel roll (card-correct).
  NOTE: the TIE/ln and T-70 dials in maneuvers.ron are the earlier
  house dials, not the printed cards — revisit if fidelity matters.
- CARD IMAGES: the user cloned voidstate/xwing-card-images (MIT-licensed
  repo of FFG card scans, XWS naming) into reference/ — GITIGNORED, never
  commit it. Checked: 561 images all valid PNG/JPEG, no trailing data,
  util scripts benign. Every pilot and ship class has an `xws` id;
  PilotDb::card_image(ships, id) gives `pilots/<faction>/<ship>/<xws>.png`
  relative to that repo's images/ dir, and a data test asserts every
  card exists when reference/ is present. Plan: the squad builder loads
  cards from a configurable local cards dir (default
  reference/xwing-card-images/images) and shows the pilot card when
  picking pilots; ship art stays ours.

- UPGRADES AS DATA (session 4, done): assets/data/upgrades.ron — 153
  First Edition cards VERIFIED from the card scans: torpedoes (7),
  turrets (5), missiles (8), crew (30), bombs (7), cannons (6), systems
  (9) and 6 more titles (added 2026-09-04/05, not enforced), tech
  (7), astromechs (17), modifications (14 usable by small ships; 15
  large/other-ship mods deliberately not encoded), title Black One, and
  37 elite pilot talents (Scum-only ones and Adaptability's second face
  skipped). Each card: xws, slot, cost, unique/limited, restrictions
  (Restriction enum: SmallShipOnly, ShipOnly(substring of xws),
  FactionOnly, SkillAbove/AtMost, RequiresAction, LacksSlot,
  RequiresSlots, AgilityBelow), optional SecondaryWeapon (dice, range,
  TargetLock/Focus/Free, discard_to_fire), an UpgradeEffect tag and the
  verbatim card text. NO effect enforced yet (implemented() false).
  ShipClass.upgrade_bar lists printed slots (T-70: Astromech, Torpedo,
  Tech; TIE/fo: Tech; TIE/ln: none); Modification + Title implicit
  (Slot::implicit). Content::load_dir(dir) reads all four data files;
  UpgradeDb::card_image → upgrades/<slot>/<xws>.png. Rulebook p.18-19
  read: 100-pt squads, one card per icon, unique names once per side,
  secondary weapons replace the primary attack (dice/range from card,
  "Attack (target lock)" needs a lock on the defender).

- SQUADS (session 4, done, NOT yet playtested): sf-core::squad —
  Squad/SquadShip/SquadRules + validate_squad (points, faction, source
  packs, unique names incl. same-name pilots, slots printed + implicit
  Mod/Title + pilot talent + R2-D6-granted, Limited, every Restriction
  incl. mod-granted action icons, callsigns). CreateGame/JoinGame carry
  Option<Squad>; server validates on join and builds the game with
  GameState::from_squads (None = basic fixed fleet). Client: menu button
  "Squad Builder" → Screen::Squad (squad_builder.rs): 1-3 add a ship,
  ↑/↓ ship, ←/→ column (pilot, then each slot), Q/E cycle pilot / legal
  card (pre-filtered by the validator), N callsign, M squad name, S save
  as <config>/squads/<name>.ron, L load next saved, Delete remove, F
  faction (clears), Esc back. Live errors + points; selected card image
  from the cards dir (STARFIGHT_CARDS env, reference/ clone, or
  <config>/cards) with the card text always shown. The builder's squad
  is written to <config>/current_squad.ron on save/exit and restored at
  startup; the MENU shows it and has ◀ ▶ buttons to pick among saved
  squads without opening the builder; Create/Join send it when valid
  (else the basic fleet). Players can keep many squads and pick one.

To try it: `cargo run -p sf-server` (copy the printed join string and
password) then two `cargo run -p sf-client` instances — paste the join
string into Server, type the password, create in one, join with the
code in the other. Quick local loop without TLS: `sf-server --insecure`
and Server `ws://127.0.0.1:7777`.

## NEXT TASK

0. **START HERE (2026-09-09).** Playtest status: the user played on
   main (2026-09-08) — new ships and the weapon prompt work ("things
   look good"); the missile option was missing only because no target
   lock had been taken. Agreed next order:
   a. ~~Weapon status in the HUD~~ DONE 2026-09-09 (unseen on screen):
      `sf_core::weapons::weapon_status()` mirrors `attack_options` from
      the player's snapshot (ShipView now carries `upgrade_ids`); the HUD
      prints "weapons (before moving): Primary: ready vs Red-2 R2 •
      Proton Torpedoes: needs target lock on Red-2 • …: no target in
      range/arc • OFFLINE (weapons failure)". Discarded ordnance simply
      leaves the list (the combat log says "discarded (fired)"). Still
      open: greying out unavailable weapons inside the Declare Target
      prompt (needs `ChooseTarget` to carry reasons → protocol 3).
   b. ~~Bombs~~ DONE 2026-09-09 (animations reviewable in the menu's
      Effects Demo; the drop keys still need a real playtest with a TIE
      Bomber carrying Seismic Charges + Proximity Mines).
      Rules as encoded (sf-core/src/bombs.rs + game.rs): planning key
      B cycles the dial-reveal bomb (`ClientMsg::PlanBomb`,
      `ShipState.bomb`), key M cycles the mine-drop action
      (`PlannedAction::DropMine`). Reveal bombs drop BEFORE the move,
      one straight-1 template behind the rear edge (token = 1×1 square,
      `bombs::drop_pose`), detonate at the END of Activation on every
      ship (both sides) whose base is within Range 1 of the token
      square: Proton = faceup card (hull + crit, shields ignored),
      Seismic = 1 damage, Ion = 2 ion tokens, Thermal = 1 damage + 1
      stress. Mines drop AFTER the move by the action and go off on the
      ship whose swept base crosses them (touching counts): Proximity 3
      attack dice, Cluster = three side-by-side tokens of 2 dice each,
      Conner Net = 1 damage + 2 ion + the action is skipped
      (`ActionResult::SkippedNetted`). Mines persist across turns in
      `GameState.bombs` (public, in Snapshot). Records: MoveRecord
      `dropped_before` / `dropped_after` / `mines_hit`, MovementResult
      `detonations`. Client: tokens drawn as squares with a per-kind
      symbol, blast = expanding ring to the Range-1 reach with fireball
      / ion-spark / fragment flavour and a flash on each ship caught;
      HUD narrates "<kind> detonates — Red-2: 1 damage". Thermal
      Detonator text CONFIRMED by the user 2026-09-09 (1 damage and 1
      stress, then discard). Range 1 is measured from the token's edge
      (its square footprint). Not modelled: bombs vs obstacles, Bomblet Generator /
      Extra Munitions / Cad Bane / Sabine crew riders, Cluster Mine
      placement when the three tokens would overlap ships.
   c. ~~Cut v0.3.0~~ DONE 2026-09-09 (tag v0.3.0, release zips built:
      Linux 26 MB, Windows 19 MB). The user's players will report on
      animation SPEED — `ATTACK_DUR` and `DETONATION_DUR` in online.rs
      are the knobs (a menu speed setting is ~1 h if they want it).
      Cutting a release: bump `version` in the workspace Cargo.toml,
      `cargo check` (lockfile), commit "Version x.y.z", `git tag -a
      vx.y.z -m "..."`, push main and the tag. Bump PROTOCOL_VERSION
      whenever a released client would misread the new messages.
   d. ~~Talent cards (4.b)~~ DONE 2026-09-09; ~~first batch of 4.d
      token/stress/movement abilities~~ DONE 2026-09-09 (see 4.b / 4.d
      for what is live and what was skipped); ~~in-game glossary~~ DONE
      2026-09-09 (F1 / "? glossary" button, see the backlog entry —
      unseen on screen by the user yet).
      >>> RESUME HERE: everything below is DONE 2026-09-09 (commits
      d97a1e6, d9d9915; 182 tests): ~~Seismic Torpedo~~ (card action
      `PlannedAction::CardActionAt(card, obstacle id)` — key K offers it,
      then click the obstacle; after moving it must be at Range 1-2 in
      the primary arc; every ship at Range 1 rolls one attack die via
      `detonate` with `BombKind::SeismicTorpedo`, obstacle removed;
      `MoveRecord.seismic: Option<SeismicBlast>`, client plays the blast
      and hides the obstacle via `Anim.removed_obstacles`).
      ~~Lieutenant Lorrir~~ (`PlannedAction::BarrelRollBank(Side,
      forward)`, `action::barrel_roll_bank_pose`: bank-1 template from
      the side midpoint, ship set flush at its far end, heading turns
      45° — away from the roll side when bending forward; keys ; and '
      (Shift = backward); 1 stress). ~~Turr Phennir~~
      (`SecondActionKind::RepositionAfterAttack`: the second slot is kept
      through movement and performed in `perform_attack_on` after the
      attack; `AttackRecord.reposition: Option<Reposition>` moves the
      sprite when the attack animation ends). ~~Expert Handling~~
      (`ActionExtras.expert_roll`: barrel roll allowed without the icon,
      stress if missing, then one enemy lock on the ship removed —
      `after_expert_roll`). ~~v0.4.0~~ CUT 2026-09-09 (tag v0.4.0,
      protocol 3, commit faaf170 "Version 0.4.0"; release zips from the
      `v*` workflow). Still unplaytested on screen: the setup screen,
      the second-action keys, the black hole pull, the torpedo pick and
      Lorrir's keys — ask the user for feedback next session.
      >>> RESUME HERE: card automation is essentially COMPLETE
      (2026-09-09, 216 tests). Upgrade-effect batches 1-3 (commits
      0c80143, 85bf2e1, ea58b3e) and pilot-ability batches A-B
      (add020a, c709fd4) are in. Machinery worth knowing: `used_round`
      (once-per-round cards, cleared in the End phase), `ordnance`
      tokens (Extra Munitions), `Shot.focus_hit` (Luke), `auto_lock` /
      `auto_lock_within` (Kagi-aware), `after_attack_cards`,
      `after_reposition_cards`, `dial_rotation` (Navigator, Stay on
      Target, Juno, Tetran — only when the planned move would leave the
      board or bump), `dice_card_action` (Lando, Saboteur, R5-D8 via K),
      `marked_enemy` (Kallus, A Score to Settle: the most expensive
      enemy), `white_reds` (Leia), `lingers` (Fel's Wrath), `tractor`,
      `stored_focus` (Rey), `CritEffect::severity` (Maarek).
      Policies chosen for "may" effects are documented at each hook.
      STILL DATA-ONLY (need a player choice or a bigger refactor):
      upgrades Electronic Baffle, Weapons Engineer (two locks), Jan Ors,
      Lightning Reflexes, Millennium Falcon title, Daredevil,
      Experimental Interface, Snap Shot, Decoy, Hyperwave Comm Scanner,
      Intelligence Agent; pilot Han Solo HotR (setup anywhere beyond
      Range 3). MULTI-PLAYER SEATS DONE 2026-09-09 (PROTOCOL 4,
      unreleased): `GameSetup.players` 2-4 + `teams: Vec<u8>` (side per
      seat; empty = free-for-all; `set_teams`, `points_for_seat` =
      points / seats on the side per core rules p.20, `mode_name`);
      `Seat` gained East/West with `deploy_zone` rectangles and
      `Seat::for_side` (sides deploy S, N, E, W); `GameState.teams`,
      `allied()`, `seat_of()`, `seat_ranks()` (initiative side first,
      then seat order), `alive_teams()`/`check_victory()`, `winner`
      is now the winning TEAM index, `resign()` destroys the player's
      ships and returns the winner only when one side is left; every
      "friendly/enemy" check in game.rs and weapons.rs is team-based
      (ownership checks stay for control/hidden info); `ShipView.team`.
      Server: capacity = setup.players, per-seat SquadRules, lobby code
      stays open until full, GameStart { seat, team, players }, Snapshot
      { committed: Vec, squad_totals: Vec, teams }, resign/disconnect
      with >2 sides keeps the game going (pre-start disconnect cancels
      the lobby). Client: setup screen fields Players (2-4) and Mode
      (teams / free-for-all), placement seeding per edge, ally colour
      (blue), team-based lock picking, HUD committed list per seat.
      Presets: Team battle 2v2, Outnumbered 1v2, Three-way and Four-way
      free-for-all; glossary "Team play and free-for-all". Tests:
      sf-core team/FFA tests, rules East/West zones, server 3-player
      flow (220 total). NOT done: unique-pilot limit per team (rules
      p.19) is still per squad; free-for-all is our extension (rules
      only know two sides); >2 seats unplaytested on screen — needs 3
      clients. Unplaytested on screen: everything since v0.4.0 plus the
      setup screen, second-action keys, black hole pull, torpedo pick,
      Lorrir keys. NEXT: playtest feedback → v0.5.0 (protocol 4 means
      new zips for everyone); the data-only cards above as planning
      toggles (a "reveal card" slot like `bomb` for Lightning Reflexes /
      Falcon title / Decoy).
      Skip: player-placed obstacles (user decision). ~~Defender policies (Elusiveness,
      R7) and damage-card riders~~ DONE (see 4.d second batch).
      ~~Second action / template choice~~ DONE 2026-09-09, PROTOCOL 3
      (unplaytested — needs a look at the keys on screen):
      `ShipState.planned_action2` + `plan_second_action` (proto
      `PlanSecondAction`), `action::SecondActionKind` (FreeBarAction =
      Push the Limit with stress after; TwoActions = Darth Vader;
      BoostAfterMove = Snap, speed 2-4 and not bumped, before the action
      step; RepositionAfterFocus = Jake Farrell; RollOnGreenReveal =
      BB-8, before the move) computed server-side into
      `ShipView.extras: ActionExtras { second, turn_boost (Blue Ace:
      BoostDir::TurnLeft/Right), far_roll (Zeta Ace:
      PlannedAction::BarrelRollFar), card_actions }`; card actions
      (`PlannedAction::CardAction(card)` → `ShipState.card_actions` for
      the round: Marksmanship, Rage (focus + 2 stress + 3 rerolls),
      Expose (+1 attack, -1 agility), R2-F2 (+1 agility)); the action
      step runs through `perform_action` (returns dropped mine tokens);
      MoveRecord gains `pre` (BB-8 roll) and `second`. Client keys: 0
      arms the second slot then any action key (1 clears), -/= turn
      boosts, [/] far rolls, K cycles card actions; HUD shows "2nd:" and
      the move narration says "then …". Six tests. ~~Cut v0.4.0~~ DONE
      2026-09-09; ~~reasons for unavailable weapons in the Declare Target
      prompt~~ DONE 2026-09-09 (`weapons::unavailable_reasons`,
      `PendingAttack.unavailable`, `ChooseTarget.unavailable`, shown as
      "not available now: Proton Torpedoes (needs a target lock on
      Red-2)" under the prompt); ~~obstacles~~ DONE 2026-09-09
      (`sf-core/src/obstacle.rs`: convex polygon tokens from four
      hand-drawn `SHAPES`, SAT overlap, segment-vs-polygon obstruction,
      `scatter()` random placement with the p.20 spacing; `GameState.
      obstacles` set by `place_obstacles` — the server scatters
      `--asteroids N` (default 6) asteroids at game start, tests push
      `Obstacle`s by hand; ships cannot deploy / boost / roll onto them;
      crossing an asteroid = `ActionResult::SkippedObstacle` + one die
      (hit 1 damage, crit faceup card), ending on one sets
      `ShipState.on_asteroid` (no attack options, `WeaponState::
      Grounded`, cleared in finish_turn); debris = stress + crit-only
      die; obstruction = `combat::closest_points` segment crossing any
      token → +1 defense die, `AttackRecord.obstructed`, Trick Shot +1
      attack die; Snapshot carries `obstacles`; client draws asteroids
      as craggy outlines and debris as dotted clouds (gizmos — swap for
      sprites when the user finds art), red placement tint on a token,
      "OBSTACLE!" in the move line, "(obstructed)" in the attack line.
      NOT done: bombs dropped onto obstacles, Seismic Torpedo (removes
      an obstacle — easy now: `GameState.obstacles.retain`). Player-
      placed obstacles: the user DOES NOT want them (2026-09-09) — the
      random scatter from the setup screen stays.
      ~~Game setup screen~~ DONE 2026-09-09 (user request: the host
      answers questions / picks a scenario): `sf-core/src/scenario.rs`
      (`Scenario` presets from `assets/data/scenarios.ron` via
      `Content.scenarios`; `GameSetup { scenario, players, points,
      asteroids, debris, board_width, board_height }` with `validate()`
      — 2 players only for now, ≤12 tokens, 20-400 pts, 12-40 unit
      sides — `board()`, `obstacle_kinds()`, `summary()`); proto
      `CreateGame.setup: Option<GameSetup>` and `GameStart.setup`; the
      server validates, sizes the board, scatters asteroids + debris
      and uses `points` for SquadRules (both squads); `--asteroids` is
      now only the default for clients that send no setup. Client:
      `Screen::Setup` (`setup.rs`) between Create Game and the
      connection (menu stores `PendingCreate`): Up/Down preset, Tab
      field, Left/Right adjust (edited presets become "… (custom)"),
      Enter connects, Esc back; the HUD header and the "Game code"
      status show `summary()`. Players > 2 is reserved (the session
      code assumes 2 seats; multi-player needs seats, deployment edges
      and turn order generalised). Six presets. Then
      Lorrir's bank-template roll,
      Turr Phennir (reposition after attack), Expert Handling, Squad
      Leader / Lando (friendly free actions) remain.
   Done 2026-09-08/09 (all pushed): server refusals now send Error +
   Close and drain (the old drop caused "connection reset by peer" that
   hid the reason) and the client shows "Connection refused: <why>";
   generated passwords `abcd-efgh-jkmn` over an unambiguous alphabet,
   join codes likewise, password check trims + case-folds (still
   constant time); TIE/X-Wing dials reordered speed-major; the Planning
   help line lists only the selected ship's action bar; failed target
   locks are narrated in the turn events (lock needs Range 1-3 right
   after the ship's own move); ordnance flies as a warhead from the nose
   (turret shots from the base center); impacts by weapon type:
   Blast / Sparks (ion) / Fragments (cluster, assault, flechette) /
   Flash — none of the effects have been seen on screen yet.
1. ~~Playtest M4~~ done 2026-09-02 (join string wrap confirmed good).
2. ~~Playtest callsigns~~ done 2026-09-02: hover name tag and N-rename
   confirmed working by the user.
3. ~~Playtest the squad builder~~ user: "builder appears to work well"
   (2026-09-02). Still worth a check: pick from the menu with < >, create/join, check callsigns/pilots/totals in the
   HUD; check the card image shows). Known rough edges: keyboard-only
   UI, the HUD text can get long with many ships; scenario rules are
   fixed at 100 pts / all sources (SquadRules::default) — a lobby
   setting later.
4. **START HERE — enforce card effects one at a time, each with tests
   and its own commit**, flipping `implemented()` to true per variant
   (pilot.rs PilotAbility / upgrade.rs UpgradeEffect) so the data test
   can later assert what is live. Suggested order:
   a. ~~Game-start stat mods~~ DONE (session 5): max_hull/max_shields/
      agility/action_bar/effective_skill on GameState consult the
      equipped upgrades; Stealth Device discards on hit; Adaptability is
      two entries (adaptabilityincrease/decrease). Hook points for the
      rest: `effects()` / `count_effect()` in game.rs.
   b. Dice-modifying pilot abilities. HOOKS EXIST (session 5): game.rs
      `free_attack_mods` / `free_defense_mods` run after lock rerolls and
      before token spending; `ability()` respects Injured Pilot; tests
      use the `duel(c, imperial_xws, rebel_xws)` helper + `scripted`
      dice (attack d8: 0-2 hit, 3 crit, 4-5 focus, 6-7 blank; defense:
      0-2 evade, 3-4 focus, 5-7 blank). DONE: Poe FocusToResult (attack
      and defense, token kept); extra-dice hook `extra_attack_dice()`
      (Mauler Mithel, Backstabber, Scourge, Zeta Leader — Zeta always
      takes the stress; `ship_in_front_arc()` helper; `duel_at` test
      helper stages the X-Wing anywhere); Winged Gundark hit→crit
      (`free_attack_mods` takes range); Omega Ace `spend_for_all_crits`
      (always used when lock+focus held); denials Dark Curse / Omega
      Leader (`attacker_may_modify` / `attacker_may_spend` /
      `defender_may_modify` flags in perform_attack_on); friendly
      rerolls Howlrunner + Jess (`friendly_rerolls`, `reroll_attack_dice`
      / `reroll_defense_dice`: blanks first, eyes if no focus token;
      `skirmish()` test helper for multi-ship sides). All pilot dice
      abilities are live. TALENTS DONE 2026-09-09 (each with a
      scripted-dice test, `talent_duel` helper = Red Squadron Veteran
      PS4 vs Obsidian Squadron Pilot PS3 at Range 3): Predator (1 reroll,
      2 vs PS≤2), Lone Wolf (1 blank when no friend within Range 2,
      attack and defense), Wired (all eyes while stressed with no focus
      token, both sides), Expertise (eyes→hits free while unstressed),
      Calculation (focus buys a crit when exactly one eye shows, else
      the plain spend), Opportunist (always takes the stress for +1 die
      vs a tokenless defender), Outmaneuver (defender agility -1 when it
      cannot see the attacker), Crack Shot (cancels 1 evade when that
      lets a result land, card discarded), Juke (evade token → one
      defender evade becomes a focus, same "worth it" test), Sensor
      Cluster (focus → blank to evade when no eyes). Hooks:
      `talent_attack_rerolls` / `talent_defense_rerolls` /
      `opportunist_die` / `has_effect` / free fn `reroll_matching`.
      SKIPPED: Trick Shot (no obstacles yet), Weapons Guidance (dominated
      by the normal focus spend), R3/R7 astromechs, Autothrusters
      (BlankToEvadeAtRange3OrOutsideArc — easy next), Marksmanship /
      Rage / Expose / Squad Leader (action-based: need a "card action"
      in the planning UI like DropMine), Swarm Tactics / Decoy / Wingman
      (start-of-Combat friendly effects), Elusiveness / R7 (force the
      attacker to reroll: needs a defender-side reroll policy).
      NEXT: token/movement pilot abilities, then the glossary.
   c. ~~Secondary weapons~~ DONE (session 5, 2026-09-05): game.rs
      `attack_options()` lists (weapon, target) pairs — primary in arc
      (all round for `turret_primary`), each equipped Torpedo/Missile/
      Cannon/Turret card in its band (Turret slot ignores the arc) when
      its requirement holds (lock on that target / focus token);
      `PendingAttack.options`, proto `ChooseTarget { options:
      Vec<AttackChoice> }` / `DeclareTarget { target, weapon }`; client
      prompt lists "n) Weapon -> Target Rn", click = primary on that
      ship. `perform_attack_on(a_idx, Shot { d_idx, range, weapon,
      second })`: own dice, no range bonuses either way, token spent up
      front when `SecondaryWeapon.spend` (false for Homing / Ion Pulse /
      Adv. Homing / Proton Rockets / XX-23), card discarded after the
      shot (after the repeat for Cluster Missiles / Twin Laser Turret,
      queued in `CombatState.followup`); `auto_target` never spends
      ordnance. Effects live: Proton (eye→crit), Adv. Proton (3
      blanks→eyes), Concussion (blank→hit), Mangler (hit→crit), HLC
      (crits→hits), Dorsal (+1 R1), Proton Rockets (+agility), Homing
      (no evade tokens), Autoblaster/Autoblaster Turret (uncancelable
      hits), Ion Cannon/Turret/Ion Pulse (1 damage + ion tokens),
      Flechette Cannon (1 damage + stress), Adv. Homing (faceup card
      past shields via `hull_point`), TLT (twice, 1 damage each),
      Flechette Torps (stress if hull ≤4), Plasma (strip shield), Ion
      Torps (ion splash R1), Assault (1 damage splash R1), XX-23
      (friends lock). ION TOKENS: `ShipState.ion` / `ShipView.ion`;
      an ionized ship's next maneuver is a forced white straight 1
      (resolve_movement), tokens then cleared; HUD shows "ion n".
      NOT done: Tractor Beam (token unmodelled; event only), Extra
      Munitions ordnance tokens, Munitions Failsafe, Guidance Chips,
      BTL-A4 title, Bomblet/Chardaan. Tests use `run_combat` +
      `prefer(weapon)` helpers.
   d. Token/stress/movement abilities — FIRST BATCH DONE 2026-09-09
      (nine tests): Night Beast (free focus after green), Red Ace (evade
      on the first shield lost per round — `ShipState.shield_lost_round`,
      reset in finish_turn; also fires from bombs), Epsilon Leader +
      Wingman (`combat_start_stress_relief` after movement), Nien Nunb
      pilot / Soontir Fel / Cool Hand via `gain_stress` (every stress
      source now goes through it: red maneuvers, Zeta Leader,
      Opportunist, flechettes, Thermal Detonators), Kyle Katarn crew
      via `lose_stress`, Tycho (acts while stressed), Chaser
      (`friend_spent_focus` after either side spends focus), Epsilon Ace
      (skill 12 while hull is full), Gemmer Sojan (+1 agility with an
      enemy at Range 1), maneuver colours in `maneuver_difficulty`
      (Ello Asty white Tallons unstressed, R2 Astromech 1-2 straights,
      Nien Nunb crew straights, TIE Mk. II banks green, Adrenaline Rush
      red→white discarded on reveal; plan_maneuver uses it too), and
      faceup-card riders in `apply_crit_effect` (Chewbacca pilot flips
      facedown, Determination discards Pilot-trait cards —
      `CritEffect::is_pilot_trait`). STILL OPEN: policy/UI-bound ones —
      Snap free boost, Blue Ace/Zeta Ace/Lorrir templates, BB-8, Stay on
      Target / Juno speed change / Tetran K-turn speeds, Push the Limit,
      Jake Farrell / Turr Phennir free repositions, Darth Vader two
      actions (all need a second action or template choice in the
      planning UI); R2-D2 / R5-P9 / R5 / Integrated Astromech (end-phase
      repairs), Draw Their Fire, Wampa, Carnor Jax, Kir Kanos, Zertik
      Strom, Fel's Wrath, Lando/Youngster/Squad Leader/Swarm Tactics/
      Decoy (multi-ship), Comm Relay (keep an evade), Seismic Torpedo and
      Trick Shot (obstacles, p.20 not implemented).
      SECOND BATCH DONE 2026-09-09 (six tests): defender-forced rerolls
      in `defender_forces_rerolls` after the attacker's modifications —
      Elusiveness (unstressed: stress for a reroll of the attacker's
      best die) and R7 Astromech (spend the lock on the attacker, reroll
      every hit/crit; the once-per-round limit is implicit since locks
      are gained once per round); Draw Their Fire (a friend at Range 1
      with the talent takes one crit that would reach the defender's
      hull); faceup-card riders in `apply_crit_effect` — Chewbacca crew
      (card discarded, hull point and a shield back, crew discarded),
      Moff Jerjerrod (discards himself), Integrated Astromech (discards
      the astromech, hull point back; cannot save a ship at 0 hull since
      the crit is never drawn for a destroyed ship); `finish_turn` now
      takes content + roll + events: R5-P9 (focus → shield at the end
      of Combat), R5 Astromech (one Ship-trait crit repaired), R2-D2
      crew (shield back at the end of the End phase, attack die hit →
      a facedown card turns faceup); R2-D2 astromech (shield back after
      a green maneuver, in resolve_movement).
   Tuning knobs if ever needed: ANIM_SAMPLES_PER_SEC / ATTACK_DUR in
   online.rs, MINI_PX in render.rs.

- FONT (session 4): assets/fonts/DejaVuSansMono.ttf (license alongside)
  is applied to every text entity by render::apply_font (Bevy's built-in
  subset font lacks arrows/triangles — they rendered as boxes). Any glyph
  DejaVu Sans Mono has is safe in UI strings now.

## Client structure (crates/sf-client/src/)

main.rs (Screen state: Menu/Sandbox/Online, global setup, two cameras),
render.rs (Game resource, ship_visual, draw helpers, bullseye shade,
ViewCtl pan/zoom + minimap, UiFont/apply_font), menu.rs (fields, paste,
pin resolution, squad picker), squad_builder.rs (Screen::Squad, Builder
resource, saved squads + current_squad.ron),
online.rs (server mirror; Anim is a queue of AnimItem:
Move/Attack/Prompt/Waiting/TurnEnd; never mutates game state locally;
remembers the pin on NetEvent::Secured), sandbox.rs, net.rs (background
thread + channels; pinned TLS via tokio-rustls), pins.rs (config dir:
pins.txt + last-used menu values), starfield.rs.

## Repo / CI (added session 5)

- GitHub: https://github.com/b-cluts/star-fight (public; remote
  `origin`). `.github/workflows/ci.yml` runs fmt/clippy/tests on every
  push to main and PR (Linux, Bevy apt deps listed there).
  `.github/workflows/release.yml` builds sf-client + sf-server in release
  mode on ubuntu-latest and windows-latest, zips them with assets/, and
  uploads artifacts; on a `v*` tag it also creates a GitHub Release with
  the zips (softprops/action-gh-release). Manual run: Actions → Release
  builds → Run workflow. Both CI and the Linux+Windows release builds
  passed on the first run (2026-09-04); tag v0.1.0 pushed → Release with
  the zips at https://github.com/b-cluts/star-fight/releases. To ship a
  new build: bump the workspace version, commit, then
  `git tag -a v0.x.y -m "..." && git push origin v0.x.y`. Released so far:
  v0.1.0 (2026-09-04), v0.2.0 (2026-09-05: eleven ship classes with
  sprites, pilot dice abilities, card data for every slot).
  `sf_proto::PROTOCOL_VERSION` must be bumped on any message-shape
  change (now 2); the server refuses mismatched clients with an
  "update required" message.
- README.md: player quick start + dev commands. No LICENSE file yet
  (user's call).

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

- **Scenarios** (requested 2026-09-04): when creating a game the host
  picks "generic game" or one of a set of pre-determined scenarios; the
  user will write scenarios to feed in. Plan: `assets/data/scenarios.ron`
  with `Scenario { id, name, description, rules: SquadRules (max_points,
  max_ships, sources), allowed_classes: Option<Vec<ShipClassId>>,
  faction_per_seat: Option<[Faction; 2]>, board: Board, obstacles:
  Vec<ObstaclePlacement>, later objectives/special rules from p.21-24
  (missions) }`. Existing `SquadRules` is the seed; extend it with
  allowed classes and validate in validate_squad. Protocol: CreateGame
  carries `scenario: Option<ScenarioId>`; the server keeps it in the
  session, validates BOTH squads against it, and GameStart carries the
  Scenario so the client's builder/menu can validate live and show the
  limits. Menu: a scenario picker (< >) next to the squad picker; the
  builder shows "valid for scenario X" and the joining player sees the
  scenario before choosing a squad.
- **Campaigns** (requested 2026-09-04): a sequence of linked battles.
  Each side starts with a squad limit and goals; the outcome of a battle
  (and its consequences: ships lost, pilots killed, objectives met)
  decides which battle comes next and what reinforcements each side
  receives. Plan, data-driven like scenarios: `assets/data/campaigns.ron`
  with `Campaign { id, name, description, start: BattleId, battles:
  Vec<CampaignBattle { id, scenario: ScenarioId, goals per side,
  outcomes: Vec<Outcome { condition: Win(seat) | Draw | ObjectiveMet(id)
  | ShipsLost{seat, at_least} …, next: Option<BattleId> (None = campaign
  over), reinforcements: per seat { points: i32, fixed_ships:
  Vec<PilotId>, allow_classes… }, consequences: e.g. destroyed ships
  stay destroyed, damaged ships carry hull damage / faceup crits,
  unique pilots killed are gone for the campaign }> }`. State: a
  `CampaignState { campaign, current battle, per-seat roster (surviving
  ShipStates with carried damage), banked points, history }` persisted
  by the server as a RON file under a campaign code so players can
  resume across sessions; the lobby gets "Continue campaign <code>".
  The squad builder then edits a roster within the campaign's limits
  (only surviving/reinforced ships and the banked points) instead of a
  free squad. Needs scenarios first (a battle IS a scenario plus
  goals/outcomes) and the mission objectives from rulebook p.21-24.
  DECIDED by the user (2026-09-04):
  - A lost ship does not come back as such; the player may spend banked
    points on a NEW ship, which may be identical (same class/pilot if
    the pilot is generic). A lost UNIQUE pilot is gone for the campaign.
  - Pilots gain experience: a pilot surviving battles gains skill
    points, faster the more they achieve (kills, scenario goals met).
    Track per-pilot `kills`, `goals`, `battles` in the campaign roster;
    skill bonus = f(achievements) applied like Veteran Instincts (cap 12
    total). Exact thresholds TBD with the user when implementing.
  - Branching graph: e.g. Rebels win → next battle attacks an Imperial
    forward outpost; Imperials win → next battle is a retreat where the
    Imperials try to finish the Rebels off before reinforcements arrive.
  - ESCAPING: in campaign play a ship that flies off the map is NOT
    destroyed — it escapes (saved for the next battle) but cannot help
    win; fleeing ships never count towards victory. Core change:
    `destroyed: bool` becomes a `ShipStatus { Active, Destroyed,
    Escaped }` (escaped ships leave the board, take no further part, are
    excluded from win checks as if destroyed for the OPPONENT's victory
    but survive into the roster). The base game's "fleeing = destroyed"
    (p.17) stays the default for generic games; scenarios/campaigns opt
    into escape via a rule flag.
- **Scenario-specific equipment and limited stock** (requested
  2026-09-04): some scenarios/campaign battles offer UNIQUE equipment
  that exists only for that battle (example: a "network hacker" upgrade
  that plants a virus in TIE fighters that survive and return to their
  mothership; if enough are infected, the NEXT battle attacks the large
  base ship with lowered shields and impaired defenses). Such items may
  be available in one battle and absent in later ones. Players may also
  hold a STOCK of special items across a campaign (e.g. special bombs)
  with a limited count — load them and use them up in the first battle
  and they are gone. Plan: upgrades.ron already holds card data; add a
  `custom_upgrades` list on a Scenario/CampaignBattle (same Upgrade
  struct, ids ≥ 1000, source tag `Scenario`) that the builder offers
  only for that battle; a campaign roster gets `stock: Vec<(UpgradeId,
  count)>` decremented when an item is equipped/consumed; outcomes can
  add stock or set campaign FLAGS (e.g. `virus_planted: n`) that later
  battles read (scenario `requires`/`modifiers` keyed on flags, e.g.
  "base ship shields −2, agility −1 if virus_planted ≥ 2"). This needs
  large/huge ships (base ship) — the ShipClass size/footprint system
  already supports Large/Huge footprints; art + dials + primary arcs
  for a huge ship come with that work. Equipment effects use the same
  UpgradeEffect tag system, so scenario items need new effect variants
  (e.g. `PlantVirusOnHit`, tracked as a campaign flag on escape).
- **Obstacles** (requested 2026-09-04): asteroids, moons, wrecked
  stations etc. on the map with graphics (user is sourcing images).
  Rulebook p.20 (already read): obstacles placed during setup before
  ships, alternating, not within Range 1-2 of any edge; a ship whose
  template or base overlaps an obstacle skips its action and rolls 1
  attack die (hit = damage, crit = critical); a ship overlapping an
  obstacle cannot attack but can be attacked; an attack whose range
  line crosses an obstacle is obstructed → +1 defense die (Trick Shot
  hooks in here). Plan: `assets/data/obstacles.ron` with `Obstacle { id,
  name, kind: Asteroid|Moon|Debris|Station, shape: Circle(r) |
  Polygon(Vec<Vec2>) in board units, sprite, sprite_px }`; GameState
  gains `obstacles: Vec<PlacedObstacle { id, pose }>`; rules.rs gets
  overlap tests for footprint-vs-obstacle and path-vs-obstacle and a
  segment-vs-obstacle test for obstruction; movement resolution and
  perform_attack_on apply the three rules; an obstacle-placement step
  before ship placement (Phase::Placement with a sub-step, or a new
  Phase::Obstacles) with drag-and-drop in the client; scenarios can
  pre-place them. Graphics: assets/obstacles/*.png (ours to include).

- ~~**In-game index / glossary**~~ DONE 2026-09-09: `sf-client/src/
  glossary.rs` overlay on every screen (F1 or the bottom-left "? glossary"
  button; PostUpdate input so its Esc never leaks to a screen; the
  screens' input systems run under `glossary::closed`). Tabs Ships /
  Pilots / Upgrades / Tokens & Actions / Damage / Rules, type-to-filter
  over names and text, Up/Down/PageUp/PageDown, detail block for the
  selected entry, "[automated]" vs "[NOT yet automated]" from
  `implemented()`. Data: `assets/data/glossary.ron` (Content.glossary,
  GlossaryDb — tokens, actions, 14 damage cards, rules terms) and
  `PilotAbility::text()` generated from the enum doc comments (keep the
  doc comments as the card text). NOT done: card images in the overlay
  (the squad builder shows them), mouse scrolling. Original request:
  an icon or
  button (and a key, e.g. `?` or F1) available on every screen — menu,
  squad builder, online game, sandbox — that opens an overlay where a
  player can look up what anything does: ship classes (stats, action
  bar, upgrade bar, dial), pilots (skill, cost, ability text), every
  upgrade card by slot (cost, restrictions, verbatim text; Proton
  Torpedoes, bombs, crew…), tokens (focus, evade, stress, ion, target
  lock), actions, damage cards (crit effects — CritEffect::name +
  a description string to add), and rules terms (range bands, firing
  arc, bullseye, initiative, K-turn…). Plan: the card/ship/pilot text
  already lives in `Content` (ships.ron / pilots.ron / upgrades.ron
  `text` fields); add `assets/data/glossary.ron` for tokens, actions,
  crits and terms; a `glossary` Bevy state or overlay resource with
  category tabs (Ships / Pilots / Upgrades / Tokens & Actions / Damage
  / Rules), a type-to-filter search box, Up/Down scroll, Esc to close;
  show the card image when the cards dir is present (same loader as
  the squad builder) and mark cards whose effect is not yet enforced
  ("not yet automated") via `implemented()`. Reachable mid-game
  without disturbing the game state (pure overlay; the online
  connection keeps pumping).
- ~~**Obstacle shadow in the firing arc**~~ DONE 2026-09-09 (user
  request): `render::draw_obstacle_shadows` hatches the part of the
  arc behind each token (angular extent of its outline seen from the
  arc origin, from its near edge out to Range 3) in the planning
  preview, the Declare Target prompt and the Effects Demo; the arc's own
  range bands and edge lines are drawn per segment and go ~4x fainter
  inside a wedge (`render::draw_firing_arc_with`; the plain
  `draw_firing_arc` is the no-obstacle wrapper the sandbox uses); the
  user saw the obstacle graphics, the black hole and the dimmed arc in
  the Effects Demo on 2026-09-09: "looks good"; the exact rule (range ruler
  between the closest points) drives `AttackOption.obstructed` →
  `AttackChoice.obstructed` → "(obstructed)" in the prompt options and
  ", obstructed" in the HUD weapons line (`WeaponState::Ready` now
  carries it; `weapon_status` takes the obstacles).
- **Black hole obstacle** (requested 2026-09-09, user's design): a new
  `ObstacleKind::BlackHole` drawn as a black disc (core diameter 1 unit)
  with swirling gas clouds spiralling in (animated gizmo spiral, maybe a
  sprite later). Rules as specified: after ALL ships have moved (end of
  the Activation phase, before or after bombs — decide; suggest before
  bombs so the pull can drag a ship into a blast), every ship within
  Range 5 (12.5 units) of the core is pulled 1 unit straight toward the
  center (translate the pose along the line to the core, heading
  unchanged); a ship whose base overlaps the core after the pull is
  swallowed — destroyed, no wreck, narrated. PARTLY DONE 2026-09-09:
  `ObstacleKind::BlackHole` exists (core = 12-gon of `CORE_RADIUS`
  0.5; a ship whose base or template touches the core is swallowed in
  `resolve_movement`, test `a_black_hole_core_swallows…`), the client
  draws it (stacked black rings, three spiralling gas arms animated
  with `Time`, drifting motes) and the Effects Demo shows one at
  (17.5, 17) plus two asteroids, a debris cloud and Onyx-2's arc with
  the asteroid's shadow over Gold-1 (obstructed shots labelled).
  GRAVITY PULL DONE 2026-09-09: `GameState::gravity_pulls` runs after
  all moves and BEFORE bombs — every ship whose base is within Range 5
  (`GRAVITY_BANDS` × RANGE_BAND_UNITS) of a core is dragged
  `PULL_UNITS` (1) straight toward the center in 0.1 steps, stopping
  short of any other ship (no damage), swallowed (destroyed) the
  moment its base touches the core; `obstacle::Pull { ship, hole,
  from, to, swallowed }` records flow through ActivationRecords /
  TurnRecords / MovementResult `pulls`; the client animates the slide
  (eased, shrinking into the core when swallowed) between the moves
  and the detonations, narrated in the HUD. Bomb tokens are NOT
  pulled; a pulled ship landing on an asteroid suffers nothing (both as
  suggested). `GameSetup.black_holes` (≤2, counted in the 12-token
  cap; setup-screen field "Black holes"; scattered first, spacing by
  the core polygon), preset "Event horizon" (1 hole + 3 asteroids),
  glossary entry "Black hole". Unplaytested on screen. Open questions for the
  user: does a pulled ship stop when it would overlap another ship
  (suggest: yes, bump-style back-off, no damage); are huge/large ships
  pulled the same distance; do bomb tokens get pulled (suggest no); is
  the pull cumulative with a ship's own move toward the hole (yes).
  Implementation: `Obstacle` gets a `radius` / kind-specific shape (the
  hole is a circle, not a polygon — add `ObstacleShape::Circle(r)` or
  treat the core as a small polygon), `resolve_movement` gets a
  `gravity_pull()` pass producing `MoveRecord`-like `Pull { ship, from,
  to, swallowed }` records for the client animation (MovementResult
  gains `pulls`), the obstruction test should ignore the gas (only the
  core obstructs?), scenario preset "Event horizon" (one black hole
  centred, few asteroids), glossary entry.
- **Ship size examples** (requested 2026-09-04): add one or two real
  ships per base size so Medium/Large/Huge footprints get exercised
  (movement, bumping, arcs, range all already work per footprint; huge
  ships also need Epic rules — energy, sections, no dial — later). User
  will source top-down art; I supply the names. Sprite requirements
  (see render.rs `ship_visual`): PNG with alpha (32-bit RGBA),
  transparent background, ship facing UP, cropped tight so the image
  height is the ship's base length (the sprite is scaled so its height
  = footprint.length), then ships.ron gets `sprite_px: (w, h)` and
  `anchor_px` = the nose's pixel (front-center). First Edition ships by
  base: Small (40 mm) — have TIE/ln, T-70, TIE/fo; candidates A-Wing,
  Y-Wing, TIE Interceptor, TIE Advanced, TIE Bomber, Z-95 Headhunter.
  Large (80 mm) — YT-1300 (Millennium Falcon), Firespray-31 (Slave I),
  Lambda-class Shuttle, VT-49 Decimator, YT-2400 (Outrider),
  Upsilon-class Shuttle, JumpMaster 5000, VCX-100 (Ghost). Huge (Epic,
  80 × 192 mm) — CR90 Corvette (Tantive IV), GR-75 Medium Transport,
  Raider-class Corvette, Gozanti-class Cruiser. Medium (60 mm) does not
  exist in First Edition; Second Edition moved ARC-170, Scurrg H-6,
  M12-L Kimogila and Auzituck Gunship onto it, so any of those would do
  if we want the size exercised. Dials for new classes go in
  maneuvers.ron (I know the First Edition dials). DONE 2026-09-04: Y-Wing
  BTL-A4 (class 4, dial set 4, pilots 401-404 Horton Salm / Dutch Vander
  / Gray / Gold, Source::YWingExpansion, turret slot + 5 turret cards,
  sprite assets/ships/y-wing.png from the user's public-domain render,
  listed in assets/ships/SOURCES.md; sandbox fields one on the Rebel
  side). Also A-Wing RZ-1 (class 5, dial set 5, pilots 501-506 Tycho /
  Jake Farrell / Arvel Crynyd / Gemmer Sojan / Green / Prototype,
  Sources AWingExpansion + RebelAces, missile slot + 8 missile cards
  ids 140-147, titles A-Wing Test Pilot 91 (BarGainsTalent, live) and
  BTL-A4 Y-Wing 92; sprite assets/ships/a-wing.png from the user's
  license-free WebP render). And YT-1300 (class 6, LARGE base 2×2,
  dial set 6, `turret_primary: true` → attack_candidates skips the arc
  test; pilots 601-608 incl. Outer Rim Smuggler whose card stats
  2/1/6/4 override the chassis via `Pilot.stats: Option<StatBlock>` /
  `GameState::printed()`; Source::YT1300Expansion; Crew slot with a
  21-card Rebel/generic starter set ids 150-170; titles Millennium
  Falcon 93 (BarGainsEvade, live) / 94; sprite assets/ships/yt-1300.png
  from the user's falcon.png; sandbox fields it instead of the X-Wing).
  Imperial side (2026-09-05): TIE Bomber (class 7, dial 7, pilots
  701-707, Bomb slot + 7 bomb cards ids 180-186, data only), TIE
  Advanced x1 (class 8, dial 8, pilots 801-808, title TIE/x1 95), TIE
  Interceptor (class 9, dial 9, pilots 901-911, title Royal Guard TIE
  96); sprites from the user's three WebP renders (were nose-down,
  turned 180°); Sources TieBomberExpansion / ImperialVeterans /
  TieAdvancedExpansion / ImperialRaider / TieInterceptorExpansion /
  ImperialAces; sandbox fields an Interceptor beside the TIE/ln. Squad
  builder class keys are now 1-9 (legend lists them). Not encoded:
  Chardaan Refit (cost −2, costs are u8), Bomblet Generator (two Bomb
  slots), TIE/v1 (TIE Adv. Prototype, not our ship). Lambda-class
  Shuttle (class 10, LARGE, dial 10 incl. the red stationary "Straight
  0" — templates::straight_length now accepts speed 0, sampler yields
  [start, start]; pilots 1001-1004; title ST-321 97; Cannon slot + 6
  cannons ids 190-195; System slot + 9 systems ids 200-208; 9 Imperial
  crew ids 171-179 — Palpatine (two slots) and the huge-ship-only crew
  left out; sprite keyed out of a black multi-view sheet, wings in the
  folded position). Fleet is now 6 Imperial / 5 Rebel classes. Turret/missile attacks and the six new pilot abilities
  are data only so far (weapons need the weapon-choice step, roadmap c;
  note in upgrades.ron which cards do NOT spend the lock/focus). Both
  sprites: Y-Wing confirmed good in the sandbox 2026-09-05; A-Wing still to be seen (Y-Wing is fielded
  in the sandbox; A-Wing via the squad builder key 3). Art must be public
  domain / CC0 / CC-BY (repo is MIT/Apache and the release zips
  redistribute assets/); the first two finds (ywing.png: good 1920×1080
  16-bit render, nose LEFT, needs rotate+crop+8-bit; awing.png: actually
  a Delta-7B Jedi starfighter, already transparent and nose-up) were
  personal-use only, so they stay at the repo root, gitignored via
  `/*.png`. Scan procedure for any new image: PNG chunk walk (only
  IHDR/IDAT/IEND + harmless ancillaries, CRCs ok, no trailing bytes),
  full decode with PIL, alpha histogram/bbox, then view a preview.
  Idea if personal-use art must be used locally: gitignored
  assets/local/ with a silhouette fallback when the file is missing.
- ~~Ship callsigns + hover tooltip~~ done 2026-09-02 (see above). When
  the squad builder exists, naming moves from the Placement N-key into
  the builder (callsign becomes a field of the squad entry).

## Open items / needed from the user

- Stressed-red-reveal rule: PROVISIONAL auto-substitution (slowest white
  straight, effective-color aware; marked in game.rs) — user is
  considering an alternative approach.
- Real squad costs, pilot roster (abilities would activate Injured
  Pilot), ordnance content, faction rosters for the squad builder.
- 3+ players: DONE 2026-09-09 (teams / free-for-all, 2-4 seats); unique-per-team limit not enforced.
- Boost exists as an action (T-70 bar) — sandbox/online action keys
  cover it; no dedicated preview arrows yet.
