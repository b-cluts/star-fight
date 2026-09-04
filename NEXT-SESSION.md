# Where we left off (2026-09-04, session 5)

## State: full networked game loop with combat, actions, and crits

`cargo build` clean, `cargo test --workspace` green (109 tests),
`cargo clippy --workspace -- -D warnings` clean, `cargo fmt --check`
clean (rustfmt.toml: max_width 100, use_small_heuristics Max). Rulebook coverage:
core_rules_en.pdf pages 8-13 and 16-19 are implemented (the PDF sits at
the repo root, gitignored — read pages on demand; p.20 obstacles, p.20
team play and p.21-24 missions are read but NOT implemented).

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

- UPGRADES AS DATA (session 4, done): assets/data/upgrades.ron — 128
  First Edition cards VERIFIED from the card scans: torpedoes (7),
  turrets (5), missiles (8), crew (21), bombs (7) and 5 more titles
  (added 2026-09-04/05, not enforced), tech
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
      abilities are live. NEXT: EPTs Wired,
      Predator, Lone Wolf, Crack Shot, Juke, Expertise, Calculation,
      Opportunist, Outmaneuver, Trick Shot; tech Weapons Guidance /
      Sensor Cluster; astromech R3/R7.
   c. Torpedoes: attack choice (primary vs each ready torpedo) in the
      Declare Target step — extend PendingAttack/ChooseTarget with weapon
      options and DeclareTarget with a weapon id; range/lock
      requirements; discard (Extra Munitions tokens, Munitions Failsafe);
      Guidance Chips.
   d. Token/stress abilities (Red Ace, Night Beast, Nien Nunb, Epsilon
      Leader, Chaser, Wingman, Cool Hand, R2-D2, R5-P9, Comm Relay…),
      then movement ones (Snap free boost, Blue Ace/Zeta Ace templates,
      Ello Asty/Adrenaline Rush/Stay on Target colours, BB-8, R2
      Astromech, Twin Ion Engine, Push the Limit free action), Epsilon
      Ace skill 12, Wampa, damage-card ones (Determination, Integrated
      Astromech, R5 Astromech, Draw Their Fire). Youngster/Squad Leader/
      Swarm Tactics/Decoy need multi-ship hooks; Seismic Torpedo and
      Trick Shot need obstacles (p.20, not implemented).
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
  new build: `git tag -a v0.1.1 -m "..." && git push origin v0.1.1`.
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
  slots), TIE/v1 (TIE Adv. Prototype, not our ship). Still wanted on
  the Imperial side: a large ship (Lambda-class Shuttle or Firespray-31). Turret/missile attacks and the six new pilot abilities
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
- 3+ players: designed-for but GameState/Seat model is 2-player.
- Boost exists as an action (T-70 bar) — sandbox/online action keys
  cover it; no dedicated preview arrows yet.
