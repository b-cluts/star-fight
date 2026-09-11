//! Connected play against the server: placement, secret planning, and
//! animated turn resolution driven by server messages. The client mirrors
//! what the server tells it and never mutates game state itself.

use bevy::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::f64::consts::FRAC_PI_2;

use sf_core::action::{ActionKind, ActionResult, BoostDir, PlannedAction, SecondActionKind, Side};
use sf_core::board::Seat;
use sf_core::bombs::{BombKind, BombToken, Detonation};
use sf_core::combat;
use sf_core::game::{AttackRecord, MoveRecord, Phase, ShipView};
use sf_core::geometry::{Pose, Vec2 as GVec2};
use sf_core::maneuver::{self, Difficulty};
use sf_core::mission::{MissionKind, MissionView};
use sf_core::obstacle::{self, Obstacle, ObstacleKind, Pull};
use sf_core::rules;
use sf_core::ship::ShipId;
use sf_core::upgrade::{Slot, UpgradeEffect, UpgradeId};
use sf_core::weapons::{WeaponState, weapon_status};
use sf_proto::messages::{ClientMsg, ServerMsg};

use crate::Screen;
use crate::net::{NetEvent, NetHandle};
use crate::pins;
use crate::render::{self, ClassArt, CursorUnits, Game, Ghost, HudText, ShowArcs};

/// Path samples flown per second during resolution animation
/// (samples are 0.1 units apart → 4 units/second).
const ANIM_SAMPLES_PER_SEC: f32 = 40.0;

/// Seconds a black-hole pull slides a ship for.
const PULL_DUR: f32 = 0.7;
/// Seconds a bomb or mine detonation plays for.
const DETONATION_DUR: f32 = 1.1;
/// Seconds per attack in the combat animation.
const ATTACK_DUR: f32 = 1.1;
/// Fraction of an attack spent in bolt flight (the rest: impact / fade).
const FLY_FRAC: f32 = 0.55;

pub struct Snap {
    pub phase: Phase,
    pub turn: u32,
    pub ships: Vec<ShipView>,
    /// One entry per seat.
    pub committed: Vec<bool>,
    pub initiative: u8,
    pub totals: Vec<u32>,
    /// Side of each seat.
    pub teams: Vec<u8>,
    /// Bomb and mine tokens on the board.
    pub bombs: Vec<BombToken>,
    /// Asteroid and debris tokens.
    pub obstacles: Vec<Obstacle>,
    /// Where we may place ships right now.
    pub zones: Vec<(f64, f64, f64, f64)>,
    /// The rulebook mission in play.
    pub mission: Option<MissionView>,
}

/// One step of the turn playback queue, fed by server messages as the
/// combat streams in (attacks can arrive while moves still animate).
/// One selectable attack from a ChooseTarget prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    /// None = primary weapon; Some = an equipped secondary weapon card.
    pub weapon: Option<u16>,
    pub target: u32,
    pub range: u8,
    /// The shot is obstructed by an obstacle (+1 defense die).
    pub obstructed: bool,
}

#[derive(Clone)]
pub enum AnimItem {
    Move(MoveRecord),
    Attack {
        rec: AttackRecord,
        line: String,
    },
    /// The server asks us to declare a target (and weapon) for `attacker`.
    Prompt {
        attacker: u32,
        options: Vec<Choice>,
        /// Weapons that cannot fire this time, with the reason.
        unavailable: Vec<(String, String)>,
    },
    /// The opponent is declaring a target for their `attacker`.
    Waiting {
        attacker: u32,
    },
    /// A ship dragged toward a black hole (swallowed at the end if so).
    Pull(Pull),
    /// A bomb or mine token going off.
    Detonation(Detonation),
    /// Combat finished: adopt the post-turn snapshot after this.
    TurnEnd,
}

pub struct Anim {
    pub queue: VecDeque<AnimItem>,
    pub current: Option<AnimItem>,
    pub t: f32,
    /// Poses after each ship's move this turn (attack fx, target prompts).
    pub end_poses: HashMap<u32, Pose>,
    /// Attacks animated so far this turn (miss side alternation).
    pub attack_no: usize,
    /// Tokens on the board as the playback stands (drops appear as the
    /// move plays, detonated tokens vanish when their blast ends).
    pub tokens: Vec<BombToken>,
    /// Obstacles blown up by Seismic Torpedoes so far in the playback.
    pub removed_obstacles: Vec<u32>,
}

impl Anim {
    fn new(tokens: Vec<BombToken>) -> Self {
        Self {
            queue: VecDeque::new(),
            current: None,
            t: 0.0,
            end_poses: HashMap::new(),
            attack_no: 0,
            tokens,
            removed_obstacles: Vec::new(),
        }
    }

    fn push(&mut self, item: AnimItem) {
        match &item {
            AnimItem::Move(m) => {
                self.end_poses.insert(m.ship.0, m.end);
            }
            AnimItem::Pull(p) => {
                self.end_poses.insert(p.ship.0, p.to);
            }
            _ => {}
        }
        self.queue.push_back(item);
    }

    /// A ship's pose once its move this turn has resolved.
    fn end_pose(&self, ship: u32, snap: &Snap) -> Option<Pose> {
        self.end_poses
            .get(&ship)
            .copied()
            .or_else(|| snap.ships.iter().find(|v| v.id.0 == ship).and_then(|v| v.pose))
    }
}

#[derive(Resource, Default)]
pub struct Online {
    pub net: Option<NetHandle>,
    /// Where we connected (host:port + pin), for remembering the pin.
    pub target: Option<sf_proto::tls::Target>,
    pub seat: Option<u8>,
    pub code: Option<String>,
    /// Every seat's name, in seat order (from GameStart).
    pub players: Vec<String>,
    /// Our side (team index).
    pub team: u8,
    pub snap: Option<Snap>,
    /// Snapshot held back while a turn animation plays.
    pub pending_snap: Option<Snap>,
    pub status: String,
    pub over: Option<String>,
    pub anim: Option<Anim>,
    /// Selected own ship (by ship id).
    pub sel: Option<u32>,
    pub dial_idx: usize,
    pub drag: Option<(u32, GVec2)>,
    /// Local provisional poses while arranging placement.
    pub overrides: HashMap<u32, Pose>,
    /// Waiting for the player to click an enemy to target-lock.
    pub lock_pick: bool,
    /// Waiting for the player to click an obstacle for this Seismic
    /// Torpedo card.
    pub obstacle_pick: Option<UpgradeId>,
    /// Human-readable summary of last turn's combat, shown in the HUD.
    pub combat_log: Vec<String>,
    /// Active Declare Target prompt: (attacker, options) awaiting input.
    pub prompt: Option<(u32, Vec<Choice>)>,
    /// Opponent's ship currently declaring a target (for the HUD).
    pub waiting_on: Option<u32>,
    /// Placement: callsign being typed for (ship, buffer).
    pub rename: Option<(u32, String)>,
    /// Effects demo: a scripted, looping turn on a fake snapshot.
    pub demo: bool,
    /// The next action key sets the ship's SECOND action.
    pub second_pick: bool,
    /// The host's scenario choice (from GameStart, or our own setup).
    pub setup: Option<sf_core::scenario::GameSetup>,
}

/// Callsign of a ship in the current snapshot ("ship" if unknown).
fn callsign(snap: Option<&Snap>, id: u32) -> String {
    snap.and_then(|s| s.ships.iter().find(|v| v.id.0 == id))
        .map(|v| v.callsign.clone())
        .unwrap_or_else(|| format!("ship #{id}"))
}

impl Online {
    fn phase(&self) -> Option<Phase> {
        self.snap.as_ref().map(|s| s.phase)
    }

    fn effective_pose(&self, view: &ShipView) -> Option<Pose> {
        self.overrides.get(&view.id.0).copied().or(view.pose)
    }

    fn my_seat(&self) -> u8 {
        self.seat.unwrap_or(0)
    }

    /// The board edge our side deploys from.
    fn my_edge(&self, teams: &[u8]) -> Seat {
        let sides = teams.iter().copied().max().map_or(2, |m| m + 1);
        Seat::for_side(self.team, sides)
    }

    /// Is a ship on our side?
    fn friendly(&self, view: &ShipView) -> bool {
        view.team == self.team
    }

    fn send(&self, m: ClientMsg) {
        if let Some(net) = &self.net {
            net.send(m);
        }
    }
}

#[derive(Component)]
pub struct OnlineTag;

#[derive(Component)]
pub struct OnlineShip(pub u32);

pub fn plugin(app: &mut App) {
    app.init_resource::<Online>().add_systems(OnExit(Screen::Online), exit_online).add_systems(
        Update,
        (
            fx_demo,
            poll_net,
            sync_ships,
            animate,
            rename_input.run_if(crate::glossary::closed),
            placement_input.run_if(crate::glossary::closed),
            planning_input.run_if(crate::glossary::closed),
            target_input.run_if(crate::glossary::closed),
            leave_keys.run_if(crate::glossary::closed),
            draw,
            hud,
        )
            .chain()
            .run_if(in_state(Screen::Online)),
    );
}

/// Enter the effects demo: every weapon impact style, missile flight,
/// bomb token and detonation, played on a fake board in a loop with the
/// same animation code the game uses. Esc leaves.
pub fn start_demo(online: &mut Online, game: &Game) {
    *online = Online::default();
    online.demo = true;
    online.seat = Some(0);
    online.sel = Some(0);
    online.snap = Some(demo_snap(game));
    online.status = "EFFECTS DEMO — loops until Esc; the HUD names each effect".into();
}

/// Refill the playback queue whenever the demo turn has finished.
fn fx_demo(mut online: ResMut<Online>, game: Res<Game>) {
    if !online.demo || online.anim.is_some() {
        return;
    }
    let anim = online.snap.as_ref().map(|snap| demo_queue(&game, snap));
    online.anim = anim;
}

fn demo_view(id: u32, owner: u32, class: u32, callsign: &str, pose: Pose) -> ShipView {
    ShipView {
        id: ShipId(id),
        owner: sf_core::ship::PlayerId(owner),
        team: owner as u8,
        class: sf_core::ship::ShipClassId(class),
        callsign: callsign.into(),
        pilot: "Demo Pilot".into(),
        skill: 3,
        upgrades: Vec::new(),
        upgrade_ids: Vec::new(),
        max_hull: 5,
        max_shields: 3,
        agility: 2,
        actions: Vec::new(),
        pose: Some(pose),
        hull: 5,
        shields: 3,
        stress: 0,
        focus: 0,
        evade: 0,
        ion: 0,
        lock: None,
        lock2: None,
        crits: Vec::new(),
        destroyed: false,
        escaped: false,
        satellites: 0,
        card_uses: Vec::new(),
        on_asteroid: false,
        plan: None,
        planned_action: None,
        bomb: None,
        planned_action2: None,
        extras: Default::default(),
    }
}

/// Two Imperial ships along the south row, two Rebel ships along the
/// north row, and one token of every bomb kind between them — placed so
/// the nearer ships sit exactly at Range 1 of the tokens.
fn demo_snap(game: &Game) -> Snap {
    let north = FRAC_PI_2;
    let south = -FRAC_PI_2;
    let ships = vec![
        demo_view(0, 0, 7, "Onyx-1", Pose::new(6.0, 7.0, north)),
        demo_view(1, 0, 1, "Onyx-2", Pose::new(14.0, 7.0, north)),
        demo_view(2, 1, 2, "Red-1", Pose::new(6.0, 13.0, south)),
        demo_view(3, 1, 6, "Gold-1", Pose::new(14.0, 13.0, south)),
    ];
    let kinds = [
        BombKind::Proton,
        BombKind::Seismic,
        BombKind::Ion,
        BombKind::Thermal,
        BombKind::ProximityMine,
        BombKind::ClusterMine,
        BombKind::ConnerNet,
    ];
    let card = |kind: BombKind| {
        game.content
            .upgrades
            .upgrades
            .iter()
            .find(|u| u.effect.and_then(BombKind::from_effect) == Some(kind))
            .map(|u| u.id)
            .unwrap_or(UpgradeId(0))
    };
    let bombs = kinds
        .iter()
        .enumerate()
        .map(|(i, &kind)| BombToken {
            id: i as u32,
            kind,
            card: card(kind),
            // Five bombs across the middle, the two mines off to the right
            // (the asteroid sits at x=14 between Onyx-2 and Gold-1).
            pose: Pose::new(
                if i < 5 { 3.0 + i as f64 * 2.0 } else { 16.5 + (i - 5) as f64 * 2.0 },
                10.5,
                north,
            ),
            owner: sf_core::ship::PlayerId((i % 2) as u32),
        })
        .collect();
    let rock = |id, kind, x, y, shape| Obstacle {
        id,
        kind,
        center: GVec2::new(x, y),
        heading: 0.6,
        shape,
    };
    let obstacles = vec![
        rock(0, ObstacleKind::Asteroid, 14.0, 10.0, 0),
        rock(1, ObstacleKind::Asteroid, 3.0, 4.0, 1),
        rock(2, ObstacleKind::Debris, 17.0, 4.0, 3),
        rock(3, ObstacleKind::BlackHole, 17.5, 17.0, 0),
    ];
    Snap {
        phase: Phase::Combat,
        turn: 1,
        ships,
        committed: vec![true, true],
        initiative: 0,
        totals: vec![100, 100],
        teams: vec![0, 1],
        bombs,
        obstacles,
        zones: Vec::new(),
        mission: None,
    }
}

fn demo_queue(game: &Game, snap: &Snap) -> Anim {
    use sf_core::dice::AttackFace;
    let mut a = Anim::new(snap.bombs.clone());
    for v in &snap.ships {
        if let Some(p) = v.pose {
            a.end_poses.insert(v.id.0, p);
        }
    }
    let card =
        |xws: &str| game.content.upgrades.upgrades.iter().find(|u| u.xws == xws).map(|u| u.id);
    // (label, attacker, defender, weapon, hits, hull damage)
    let shots: Vec<(&str, u32, u32, Option<UpgradeId>, bool, bool)> = vec![
        ("primary weapon — shields soak it (blue flash)", 0, 2, None, true, false),
        ("primary weapon — hull damage (orange flash)", 2, 0, None, true, true),
        ("primary weapon — miss (bolts fly past)", 1, 3, None, false, false),
        (
            "YT-1300 turret primary — fires from the base center, obstructed by the asteroid (+1 defense die)",
            3,
            1,
            None,
            true,
            false,
        ),
        ("Proton Torpedoes — warhead, Blast impact", 0, 2, card("protontorpedoes"), true, true),
        (
            "Concussion Missiles — warhead, Blast impact",
            1,
            3,
            card("concussionmissiles"),
            true,
            false,
        ),
        ("Cluster Missiles — Fragments impact", 0, 2, card("clustermissiles"), true, true),
        ("Flechette Torpedoes — Fragments impact", 2, 0, card("flechettetorpedoes"), true, true),
        ("Ion Pulse Missiles — Sparks impact", 1, 3, card("ionpulsemissiles"), true, false),
        (
            "Ion Cannon Turret — Sparks, from the base center",
            3,
            1,
            card("ioncannonturret"),
            true,
            false,
        ),
        (
            "Heavy Laser Cannon — cannon bolts, Flash impact",
            2,
            0,
            card("heavylasercannon"),
            true,
            true,
        ),
        ("Proton Torpedoes — warhead misses", 0, 2, card("protontorpedoes"), false, false),
    ];
    for (label, atk, def, weapon, hit, hull) in shots {
        let rec = AttackRecord {
            attacker: ShipId(atk),
            defender: ShipId(def),
            range: 2,
            weapon,
            // Shots between Onyx-2 and Gold-1 cross the asteroid.
            obstructed: matches!((atk, def), (1, 3) | (3, 1)),
            attack_faces: vec![AttackFace::Hit; 3],
            defense_faces: Vec::new(),
            lock_spent: false,
            attacker_focus_spent: false,
            defender_focus_spent: false,
            evade_spent: false,
            defender_in_bullseye: false,
            hits: if hit { 2 } else { 0 },
            crits: 0,
            shields_lost: if hit && !hull { 2 } else { 0 },
            hull_lost: if hull { 1 } else { 0 },
            crits_to_hull: 0,
            defender_destroyed: false,
            reposition: None,
        };
        a.push(AnimItem::Attack { rec, line: format!("DEMO: {label}") });
    }
    // Every token goes off, catching whichever demo ships sit within
    // Range 1 of it (the real rule), with that kind's typical result.
    for t in &snap.bombs {
        let tc = t.corners();
        let hits = snap
            .ships
            .iter()
            .filter(|v| {
                v.pose.is_some_and(|p| {
                    let fp = game.ships.classes[game.class_index(v.class)].footprint;
                    sf_core::combat::base_distance(&tc, &rules::footprint_corners(p, fp))
                        <= sf_core::combat::RANGE_BAND_UNITS
                })
            })
            .map(|v| {
                let (damage, crits, ion, stress) = match t.kind {
                    BombKind::Proton => (0, 1, 0, 0),
                    BombKind::Seismic | BombKind::ClusterMine | BombKind::SeismicTorpedo => {
                        (1, 0, 0, 0)
                    }
                    BombKind::Ion => (0, 0, 2, 0),
                    BombKind::Thermal => (1, 0, 0, 1),
                    BombKind::ProximityMine => (2, 0, 0, 0),
                    BombKind::ConnerNet => (1, 0, 2, 0),
                };
                sf_core::bombs::BombHit { ship: v.id, damage, crits, ion, stress, destroyed: false }
            })
            .collect();
        a.push(AnimItem::Detonation(Detonation { token: *t, hits }));
    }
    a.push(AnimItem::TurnEnd);
    a
}

fn exit_online(
    mut commands: Commands,
    mut online: ResMut<Online>,
    tagged: Query<Entity, With<OnlineTag>>,
    mut ghost: Query<&mut Visibility, With<Ghost>>,
) {
    for e in &tagged {
        commands.entity(e).despawn();
    }
    if let Ok(mut vis) = ghost.single_mut() {
        *vis = Visibility::Hidden;
    }
    *online = Online::default(); // drops NetHandle → socket closes
}

fn poll_net(mut online: ResMut<Online>, mut game: ResMut<Game>) {
    let events = match &online.net {
        Some(net) => net.drain(),
        None => return,
    };
    for ev in events {
        match ev {
            NetEvent::Msg(msg) => match *msg {
                ServerMsg::Welcome { .. } | ServerMsg::Pong => {}
                ServerMsg::GameCreated { code } => {
                    let what = online
                        .setup
                        .as_ref()
                        .map(|s| format!(" — {}", s.summary()))
                        .unwrap_or_default();
                    let missing = online.setup.as_ref().map(|s| s.players).unwrap_or(2) - 1;
                    online.status = format!(
                        "Game code: {code}{what}  —  waiting for {missing} more player{}…",
                        if missing == 1 { "" } else { "s" }
                    );
                    online.code = Some(code);
                }
                ServerMsg::Lobby { code, players, capacity } => {
                    let missing = usize::from(capacity).saturating_sub(players.len());
                    let what = online
                        .setup
                        .as_ref()
                        .map(|s| format!(" — {}", s.summary()))
                        .unwrap_or_default();
                    online.status = if missing == 0 {
                        format!("Game {code}: all {capacity} players in — starting")
                    } else {
                        format!(
                            "Game code: {code}{what}  —  {} of {capacity} in ({})  —  waiting for {missing} more…",
                            players.len(),
                            players.join(", ")
                        )
                    };
                    online.code = Some(code);
                }
                ServerMsg::GameStart { seat, team, players, board, setup } => {
                    if setup.is_some() {
                        online.setup = setup;
                    }
                    game.board = board;
                    online.seat = Some(seat);
                    online.team = team;
                    let others: Vec<&str> = players
                        .iter()
                        .enumerate()
                        .filter(|(s, _)| *s != seat as usize)
                        .map(|(_, n)| n.as_str())
                        .collect();
                    online.status =
                        format!("Matched with {} — place your ships", others.join(", "));
                    online.players = players;
                }
                ServerMsg::Snapshot {
                    phase,
                    turn,
                    ships,
                    committed,
                    initiative,
                    squad_totals,
                    teams,
                    bombs,
                    obstacles,
                    zones,
                    mission,
                } => {
                    if phase != Phase::Placement {
                        online.overrides.clear();
                        online.drag = None;
                    }
                    if online.sel.is_none() {
                        let seat = online.my_seat();
                        online.sel =
                            ships.iter().find(|s| s.owner.0 == seat as u32).map(|s| s.id.0);
                    }
                    let snap = Snap {
                        phase,
                        turn,
                        ships,
                        committed,
                        initiative,
                        totals: squad_totals,
                        teams,
                        bombs,
                        obstacles,
                        zones,
                        mission,
                    };
                    if online.anim.is_some() {
                        online.pending_snap = Some(snap);
                    } else {
                        online.snap = Some(snap);
                        // Unplaced own ships need provisional draggable spots
                        // (the ship list only exists once a snapshot is here).
                        if phase == Phase::Placement {
                            seed_default_placement(&mut online, &game);
                        }
                    }
                }
                ServerMsg::Rejected { reason } => {
                    online.status = format!("Rejected: {reason}");
                }
                ServerMsg::MovementResult { moves, pulls, detonations, events } => {
                    online.status.clear();
                    online.waiting_on = None;
                    online.combat_log = events;
                    let tokens = online.snap.as_ref().map(|s| s.bombs.clone()).unwrap_or_default();
                    // Snap Shot attacks play right after the move they answer.
                    let mut items = Vec::new();
                    for mut m in moves {
                        let snaps = std::mem::take(&mut m.snap_shots);
                        items.push(AnimItem::Move(m));
                        for rec in snaps {
                            let line = attack_line(&online, &game, &rec);
                            online.combat_log.push(line.clone());
                            items.push(AnimItem::Attack { rec, line });
                        }
                    }
                    let anim = online.anim.get_or_insert_with(|| Anim::new(tokens));
                    for it in items {
                        anim.push(it);
                    }
                    for p in pulls {
                        anim.push(AnimItem::Pull(p));
                    }
                    for d in detonations {
                        anim.push(AnimItem::Detonation(d));
                    }
                }
                ServerMsg::AttackResult { attack, events } => {
                    let line = attack_line(&online, &game, &attack);
                    online.combat_log.push(line.clone());
                    online.combat_log.extend(events);
                    online.waiting_on = None;
                    online
                        .anim
                        .get_or_insert_with(|| Anim::new(Vec::new()))
                        .push(AnimItem::Attack { rec: attack, line });
                }
                ServerMsg::ChooseTarget { attacker, options, unavailable } => {
                    let options = options
                        .into_iter()
                        .map(|o| Choice {
                            weapon: o.weapon.map(|u| u.0),
                            target: o.target.0,
                            range: o.range,
                            obstructed: o.obstructed,
                        })
                        .collect();
                    online
                        .anim
                        .get_or_insert_with(|| Anim::new(Vec::new()))
                        .push(AnimItem::Prompt { attacker: attacker.0, options, unavailable });
                }
                ServerMsg::OpponentChoosing { attacker } => {
                    online
                        .anim
                        .get_or_insert_with(|| Anim::new(Vec::new()))
                        .push(AnimItem::Waiting { attacker: attacker.0 });
                }
                ServerMsg::TurnEnd { events } => {
                    online.combat_log.extend(events);
                    online
                        .anim
                        .get_or_insert_with(|| Anim::new(Vec::new()))
                        .push(AnimItem::TurnEnd);
                }
                ServerMsg::GameOver { winner, reason } => {
                    let text = match winner {
                        Some(w) if w == online.team => format!("VICTORY — {reason}"),
                        Some(_) => format!("DEFEAT — {reason}"),
                        None => format!("GAME OVER — {reason}"),
                    };
                    online.over = Some(text);
                }
                ServerMsg::Error { message } => {
                    online.status = format!("Server: {message}");
                }
            },
            NetEvent::Secured(fp) => {
                if let Some(t) = &online.target {
                    pins::remember_pin(&t.key(), &fp);
                }
            }
            NetEvent::Closed(e) => {
                // A server refusal ("wrong password", "protocol … unsupported")
                // arrives as an Error just before the close: show that reason
                // rather than the bare socket outcome.
                if online.over.is_none() {
                    let refusal = online.status.strip_prefix("Server: ").map(str::to_owned);
                    online.over = Some(match refusal {
                        Some(why) => format!("Connection refused: {why}"),
                        None => format!("Connection closed: {e}"),
                    });
                }
            }
        }
    }
}

/// Give unplaced own ships sensible provisional spots in the deployment
/// zone so there is something to drag.
fn seed_default_placement(online: &mut Online, game: &Game) {
    if online.seat.is_none() {
        return;
    }
    let Some(snap) = &online.snap else { return };
    let seat = online.my_seat();
    let edge = online.my_edge(&snap.teams);
    let heading = edge.facing();
    // Along the edge, 1.5 units in; teammates start further along so
    // their rows do not overlap.
    let (x0, y0, x1, y1) =
        snap.zones.first().copied().unwrap_or_else(|| game.board.deploy_zone(edge));
    let mut seeds = Vec::new();
    let mut t = 3.0 + f64::from(seat) * 1.5;
    for view in snap.ships.iter().filter(|s| s.owner.0 == seat as u32) {
        if view.pose.is_none() && !online.overrides.contains_key(&view.id.0) {
            let pose = match edge {
                Seat::South => Pose::new(t, y0 + 1.5, heading),
                Seat::North => Pose::new(t, y1 - 1.5, heading),
                Seat::East => Pose::new(x1 - 1.5, t, heading),
                Seat::West => Pose::new(x0 + 1.5, t, heading),
            };
            seeds.push((view.id.0, pose));
        }
        t += 4.0;
    }
    for (id, pose) in seeds {
        online.overrides.insert(id, pose);
    }
}

/// Display name of a weapon choice: the card's name, or "primary".
fn weapon_name(game: &Game, weapon: Option<u16>) -> String {
    match weapon.and_then(|u| game.content.upgrades.upgrade(UpgradeId(u))) {
        Some(card) => card.name.clone(),
        None => "primary".into(),
    }
}

/// One-line narration of an attack for the combat log / HUD.
fn attack_line(online: &Online, game: &Game, a: &AttackRecord) -> String {
    let name = |id: u32| callsign(online.snap.as_ref(), id);
    let landed = a.hits + a.crits;
    let obstructed = if a.obstructed { " (obstructed)" } else { "" };
    let mut line =
        format!("{} -> {} @R{}{obstructed}: ", name(a.attacker.0), name(a.defender.0), a.range);
    if a.weapon.is_some() {
        line = format!(
            "{} -> {} @R{} [{}]: ",
            name(a.attacker.0),
            name(a.defender.0),
            a.range,
            weapon_name(game, a.weapon.map(|u| u.0))
        );
    }
    if landed == 0 {
        line.push_str("miss");
    } else {
        line.push_str(&format!(
            "{} dmg (-{} shields, -{} hull)",
            landed, a.shields_lost, a.hull_lost
        ));
    }
    if a.defender_in_bullseye {
        line.push_str(" [bullseye]");
    }
    if a.defender_destroyed {
        line.push_str(" — DESTROYED");
    }
    if let Some(r) = a.reposition {
        let outcome = if r.result == ActionResult::Performed { "" } else { " (failed)" };
        line.push_str(&format!(
            " — then {}{outcome}",
            action_name(game, online.snap.as_ref(), r.action)
        ));
    }
    line
}

/// Declare Target: click a highlighted enemy (or press its number).
fn target_input(
    mut online: ResMut<Online>,
    game: Res<Game>,
    cursor: Res<CursorUnits>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
) {
    let Some((_, options)) = online.prompt.clone() else {
        return;
    };
    // Number keys pick a listed option; clicking a highlighted ship picks
    // its primary-weapon option when there is one, else its first.
    let mut choice: Option<Choice> = None;
    let digits = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ];
    for (n, key) in digits.iter().enumerate() {
        if keys.just_pressed(*key)
            && let Some(o) = options.get(n)
        {
            choice = Some(o.clone());
        }
    }
    if choice.is_none()
        && buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
        && let (Some(a), Some(snap)) = (&online.anim, &online.snap)
    {
        for o in &options {
            let Some(p) = a.end_pose(o.target, snap) else {
                continue;
            };
            let Some(view) = snap.ships.iter().find(|v| v.id.0 == o.target) else {
                continue;
            };
            let fp = game.ships.classes[game.class_index(view.class)].footprint;
            if rules::point_in_footprint(p, fp, cur) {
                let primary = options.iter().find(|x| x.target == o.target && x.weapon.is_none());
                choice = Some(primary.cloned().unwrap_or_else(|| o.clone()));
                break;
            }
        }
    }
    if let Some(Choice { weapon, target, .. }) = choice {
        online.send(ClientMsg::DeclareTarget {
            target: ShipId(target),
            weapon: weapon.map(UpgradeId),
        });
        online.prompt = None;
        if let Some(a) = online.anim.as_mut() {
            a.current = None;
        }
    }
}

fn sync_ships(
    mut commands: Commands,
    online: Res<Online>,
    game: Res<Game>,
    art: Res<ClassArt>,
    mut ships_q: Query<(Entity, &OnlineShip, &mut Sprite, &mut Transform, &mut Visibility)>,
) {
    let Some(snap) = &online.snap else { return };
    // While a turn animation plays, animate() owns every ship transform:
    // already-moved ships hold their end poses, unmoved ships their
    // pre-turn poses. Snapping back to snapshot poses here would rubber-
    // band ships mid-animation.
    let animating = online.anim.is_some();
    let mut existing: HashMap<u32, Entity> = HashMap::new();
    for (e, ship, ..) in &ships_q {
        existing.insert(ship.0, e);
    }
    for view in &snap.ships {
        let class_idx = game.class_index(view.class);
        let class = &game.ships.classes[class_idx];
        match existing.get(&view.id.0) {
            Some(&e) => {
                let Ok((_, _, mut sprite, mut tf, mut vis)) = ships_q.get_mut(e) else {
                    continue;
                };
                if animating {
                    continue;
                }
                let pose = online.effective_pose(view);
                if view.destroyed && online.anim.is_none() || pose.is_none() {
                    *vis = Visibility::Hidden;
                    continue;
                }
                let (size, t) = render::ship_visual(class, pose.unwrap(), &game, 1.0);
                sprite.custom_size = Some(size);
                *tf = t;
                *vis = Visibility::Visible;
            }
            None => {
                let pose = online.effective_pose(view);
                let (size, tf) = match pose {
                    Some(p) => render::ship_visual(class, p, &game, 1.0),
                    None => (Vec2::ONE, Transform::default()),
                };
                let mut e = commands.spawn((
                    Sprite {
                        image: art.0[class_idx].clone(),
                        custom_size: Some(size),
                        ..default()
                    },
                    tf,
                    OnlineShip(view.id.0),
                    OnlineTag,
                ));
                if pose.is_none() {
                    e.insert(Visibility::Hidden);
                }
            }
        }
    }
}

fn animate(
    time: Res<Time>,
    mut online: ResMut<Online>,
    game: Res<Game>,
    mut ships_q: Query<(&OnlineShip, &mut Sprite, &mut Transform, &mut Visibility)>,
) {
    let Online { anim, snap, pending_snap, prompt, waiting_on, .. } = &mut *online;
    let Some(a) = anim else { return };
    let Some(snapshot) = snap else { return };
    loop {
        if a.current.is_none() {
            match a.queue.pop_front() {
                Some(item) => {
                    if let AnimItem::Move(m) = &item {
                        a.tokens.extend(m.dropped_before.iter().copied());
                    }
                    a.current = Some(item);
                    a.t = 0.0;
                }
                // Idle: hold every pose until the server streams more.
                None => return,
            }
        }
        let item = a.current.clone().expect("set above");
        match item {
            AnimItem::Move(mv) => {
                a.t += time.delta_secs() * ANIM_SAMPLES_PER_SEC;
                let k = a.t as usize;
                if let Some(view) = snapshot.ships.iter().find(|s| s.id.0 == mv.ship.0) {
                    let class = &game.ships.classes[game.class_index(view.class)];
                    let pose = if k < mv.path.len() { mv.path[k] } else { mv.end };
                    for (ship, mut sprite, mut tf, mut vis) in &mut ships_q {
                        if ship.0 == mv.ship.0 {
                            let (size, t) = render::ship_visual(class, pose, &game, 1.5);
                            sprite.custom_size = Some(size);
                            *tf = t;
                            // Only off-board destruction hides here; combat
                            // kills are revealed at the impact moment.
                            *vis = if k >= mv.path.len() && mv.destroyed {
                                Visibility::Hidden
                            } else {
                                Visibility::Visible
                            };
                        }
                    }
                }
                if k >= mv.path.len() {
                    a.tokens.extend(mv.dropped_after.iter().copied());
                    // A Seismic Torpedo blast plays after the move (and
                    // after any mines the move set off).
                    if let Some(blast) = &mv.seismic {
                        a.queue.push_front(AnimItem::Detonation(blast.detonation.clone()));
                    }
                    // Mines this move set off play right after it.
                    for d in mv.mines_hit.iter().rev() {
                        a.queue.push_front(AnimItem::Detonation(d.clone()));
                    }
                    a.current = None;
                    continue;
                }
                return;
            }
            AnimItem::Pull(p) => {
                a.t += time.delta_secs();
                let k = (a.t / PULL_DUR).clamp(0.0, 1.0);
                let ease = k * k;
                let pose = Pose {
                    anchor: GVec2::new(
                        p.from.anchor.x + (p.to.anchor.x - p.from.anchor.x) * f64::from(ease),
                        p.from.anchor.y + (p.to.anchor.y - p.from.anchor.y) * f64::from(ease),
                    ),
                    heading: p.from.heading,
                };
                if let Some(view) = snapshot.ships.iter().find(|s| s.id.0 == p.ship.0) {
                    let class = &game.ships.classes[game.class_index(view.class)];
                    // Swallowed ships shrink into the core.
                    let scale = if p.swallowed { 1.0 - ease * 0.85 } else { 1.0 };
                    for (ship, mut sprite, mut tf, mut vis) in &mut ships_q {
                        if ship.0 == p.ship.0 {
                            let (size, t) = render::ship_visual(class, pose, &game, 1.5);
                            sprite.custom_size = Some(size * scale);
                            *tf = t;
                            *vis = if p.swallowed && a.t >= PULL_DUR {
                                Visibility::Hidden
                            } else {
                                Visibility::Visible
                            };
                        }
                    }
                }
                if a.t >= PULL_DUR {
                    a.current = None;
                    continue;
                }
                return;
            }
            AnimItem::Detonation(d) => {
                a.t += time.delta_secs();
                if a.t >= DETONATION_DUR {
                    for hit in d.hits.iter().filter(|h| h.destroyed) {
                        for (ship, _, _, mut vis) in &mut ships_q {
                            if ship.0 == hit.ship.0 {
                                *vis = Visibility::Hidden;
                            }
                        }
                    }
                    if d.token.kind == BombKind::SeismicTorpedo {
                        // The token id is the obstacle's: it is gone now.
                        a.removed_obstacles.push(d.token.id);
                    } else {
                        a.tokens.retain(|t| t.id != d.token.id);
                    }
                    a.current = None;
                    continue;
                }
                return;
            }
            AnimItem::Attack { rec, .. } => {
                a.t += time.delta_secs();
                if a.t >= ATTACK_DUR {
                    if rec.defender_destroyed {
                        for (ship, _, _, mut vis) in &mut ships_q {
                            if ship.0 == rec.defender.0 {
                                *vis = Visibility::Hidden;
                            }
                        }
                    }
                    // Turr Phennir: the attacker hops to its new spot.
                    if let Some(r) = rec.reposition
                        && r.result == ActionResult::Performed
                    {
                        a.end_poses.insert(rec.attacker.0, r.to);
                        if let Some(view) = snapshot.ships.iter().find(|s| s.id.0 == rec.attacker.0)
                        {
                            let class = &game.ships.classes[game.class_index(view.class)];
                            for (ship, mut sprite, mut tf, _) in &mut ships_q {
                                if ship.0 == rec.attacker.0 {
                                    let (size, t) = render::ship_visual(class, r.to, &game, 1.5);
                                    sprite.custom_size = Some(size);
                                    *tf = t;
                                }
                            }
                        }
                    }
                    a.attack_no += 1;
                    a.current = None;
                    continue;
                }
                return;
            }
            AnimItem::Prompt { attacker, options, .. } => {
                // Holds here until target_input answers and clears `current`.
                if prompt.is_none() {
                    *prompt = Some((attacker, options));
                }
                return;
            }
            AnimItem::Waiting { attacker } => {
                *waiting_on = Some(attacker);
                a.current = None;
                continue;
            }
            AnimItem::TurnEnd => {
                *waiting_on = None;
                *anim = None;
                if let Some(s) = pending_snap.take() {
                    *snap = Some(s);
                }
                return;
            }
        }
    }
}

/// Typing a callsign during Placement: Enter sends it, Esc cancels.
fn rename_input(
    mut online: ResMut<Online>,
    mut events: EventReader<bevy::input::keyboard::KeyboardInput>,
) {
    use bevy::input::ButtonState;
    use bevy::input::keyboard::Key;
    if online.rename.is_none() {
        events.clear();
        return;
    }
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        match &ev.logical_key {
            Key::Enter => {
                if let Some((id, buf)) = online.rename.take() {
                    online.send(ClientMsg::Rename { ship_id: ShipId(id), callsign: buf });
                }
            }
            Key::Escape => online.rename = None,
            Key::Backspace => {
                if let Some((_, buf)) = &mut online.rename {
                    buf.pop();
                }
            }
            Key::Space => {
                if let Some((_, buf)) = &mut online.rename {
                    buf.push(' ');
                }
            }
            Key::Character(s) => {
                if let Some((_, buf)) = &mut online.rename {
                    for c in s.chars().filter(|c| !c.is_control()) {
                        if buf.chars().count() < sf_core::ship::CALLSIGN_MAX {
                            buf.push(c);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn placement_input(
    mut online: ResMut<Online>,
    game: Res<Game>,
    cursor: Res<CursorUnits>,
    buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    mut wheel: EventReader<bevy::input::mouse::MouseWheel>,
) {
    if online.phase() != Some(Phase::Placement)
        || online.anim.is_some()
        || online.over.is_some()
        || online.rename.is_some()
    {
        wheel.clear();
        return;
    }
    let seat = online.my_seat();
    let own_views: Vec<ShipView> = online
        .snap
        .as_ref()
        .map(|s| s.ships.iter().filter(|v| v.owner.0 == seat as u32).cloned().collect())
        .unwrap_or_default();

    if buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
    {
        for view in &own_views {
            let Some(pose) = online.effective_pose(view) else {
                continue;
            };
            let fp = game.ships.classes[game.class_index(view.class)].footprint;
            if rules::point_in_footprint(pose, fp, cur) {
                online.drag = Some((view.id.0, pose.anchor - cur));
                online.sel = Some(view.id.0);
                break;
            }
        }
    }
    if buttons.just_released(MouseButton::Left)
        && let Some((id, _)) = online.drag.take()
        && let Some(pose) = online.overrides.get(&id).copied()
    {
        online.send(ClientMsg::PlaceShip { ship_id: ShipId(id), pose });
    }
    if let (Some((id, off)), Some(cur)) = (online.drag, cursor.0) {
        let view = own_views.iter().find(|v| v.id.0 == id);
        if let Some(base) = view.and_then(|v| online.effective_pose(v)) {
            online.overrides.insert(id, Pose { anchor: cur + off, heading: base.heading });
        }
    }

    // Rotation on the dragged (else hovered) own ship.
    let scroll: f32 = wheel.read().map(|e| e.y).sum();
    let mut steps = if scroll > 0.0 {
        1.0f64
    } else if scroll < 0.0 {
        -1.0
    } else {
        0.0
    };
    if keys.just_pressed(KeyCode::KeyQ) {
        steps += 1.0;
    }
    if keys.just_pressed(KeyCode::KeyE) {
        steps -= 1.0;
    }
    if steps != 0.0 {
        let target = online.drag.map(|(id, _)| id).or_else(|| {
            let cur = cursor.0?;
            own_views.iter().find_map(|v| {
                let pose = online.effective_pose(v)?;
                let fp = game.ships.classes[game.class_index(v.class)].footprint;
                rules::point_in_footprint(pose, fp, cur).then_some(v.id.0)
            })
        });
        if let Some(id) = target {
            let view = own_views.iter().find(|v| v.id.0 == id);
            if let Some(mut pose) = view.and_then(|v| online.effective_pose(v)) {
                pose.heading += steps * std::f64::consts::PI / 12.0;
                online.overrides.insert(id, pose);
                if online.drag.is_none() {
                    online.send(ClientMsg::PlaceShip { ship_id: ShipId(id), pose });
                }
            }
        }
    }

    // N: rename the selected (else hovered) own ship.
    if keys.just_pressed(KeyCode::KeyN) {
        let hovered = cursor.0.and_then(|cur| {
            own_views.iter().find_map(|v| {
                let pose = online.effective_pose(v)?;
                let fp = game.ships.classes[game.class_index(v.class)].footprint;
                rules::point_in_footprint(pose, fp, cur).then_some(v.id.0)
            })
        });
        if let Some(id) = online.sel.or(hovered) {
            let current = callsign(online.snap.as_ref(), id);
            online.sel = Some(id);
            online.rename = Some((id, current));
        }
    }

    // A: submit every current position at once.
    if keys.just_pressed(KeyCode::KeyA) {
        for view in &own_views {
            if let Some(pose) = online.effective_pose(view) {
                online.send(ClientMsg::PlaceShip { ship_id: ShipId(view.id.0), pose });
            }
        }
    }
}

fn planning_input(
    mut online: ResMut<Online>,
    game: Res<Game>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    cursor: Res<CursorUnits>,
) {
    if online.phase() != Some(Phase::Planning) || online.anim.is_some() || online.over.is_some() {
        return;
    }
    let seat = online.my_seat();
    let own_ids: Vec<u32> = online
        .snap
        .as_ref()
        .map(|s| {
            s.ships
                .iter()
                .filter(|v| v.owner.0 == seat as u32 && !v.destroyed)
                .map(|v| v.id.0)
                .collect()
        })
        .unwrap_or_default();
    // Mission 1: Rebel ships may protect the senator's shuttle.
    let protect_ok = online
        .snap
        .as_ref()
        .and_then(|s| s.mission.as_ref())
        .is_some_and(|m| m.kind == MissionKind::PoliticalEscort && m.rebel_side == online.team);
    if own_ids.is_empty() {
        return;
    }
    let current = online.sel.filter(|id| own_ids.contains(id)).unwrap_or(own_ids[0]);
    let mut selected = current;
    if keys.just_pressed(KeyCode::Tab) {
        let i = own_ids.iter().position(|&x| x == current).unwrap_or(0);
        selected = own_ids[(i + 1) % own_ids.len()];
        online.dial_idx = 0;
    }
    online.sel = Some(selected);

    let dial_len = {
        let Some(snap) = &online.snap else { return };
        let Some(view) = snap.ships.iter().find(|v| v.id.0 == selected) else {
            return;
        };
        let class = &game.ships.classes[game.class_index(view.class)];
        game.dial(class).len()
    };
    if dial_len == 0 {
        return;
    }
    if keys.just_pressed(KeyCode::ArrowRight) {
        online.dial_idx = (online.dial_idx + 1) % dial_len;
    }
    if keys.just_pressed(KeyCode::ArrowLeft) {
        online.dial_idx = (online.dial_idx + dial_len - 1) % dial_len;
    }
    if keys.just_pressed(KeyCode::Enter) {
        online.send(ClientMsg::PlanManeuver {
            ship_id: ShipId(selected),
            maneuver_index: online.dial_idx as u8,
        });
    }

    // Action planning: 1 Pass, 2 Focus, 3 Evade, 4/5 barrel roll L/R,
    // 6 target lock (then click an enemy ship).
    let (bar, extras) = {
        let Some(snap) = &online.snap else { return };
        let Some(view) = snap.ships.iter().find(|v| v.id.0 == selected) else {
            return;
        };
        (view.actions.clone(), view.extras.clone())
    };
    // 0 arms the second-action slot (Push the Limit, Darth Vader, Snap,
    // Jake Farrell, BB-8): the next action key fills it instead.
    if keys.just_pressed(KeyCode::Digit0) && extras.second.is_some() {
        online.second_pick = !online.second_pick;
        online.lock_pick = false;
        online.obstacle_pick = None;
    }
    let plan_action = |online: &mut Online, action: PlannedAction| {
        online.lock_pick = false;
        online.obstacle_pick = None;
        if online.second_pick {
            online.second_pick = false;
            let action = (action != PlannedAction::Pass).then_some(action);
            online.send(ClientMsg::PlanSecondAction { ship_id: ShipId(selected), action });
        } else {
            online.send(ClientMsg::PlanAction { ship_id: ShipId(selected), action });
        }
    };
    if keys.just_pressed(KeyCode::Digit1) {
        plan_action(&mut online, PlannedAction::Pass);
    }
    if keys.just_pressed(KeyCode::KeyP) && protect_ok {
        plan_action(&mut online, PlannedAction::Protect);
    }
    if keys.just_pressed(KeyCode::Digit2) && bar.contains(&ActionKind::Focus) {
        plan_action(&mut online, PlannedAction::Focus);
    }
    if keys.just_pressed(KeyCode::Digit3) && bar.contains(&ActionKind::Evade) {
        plan_action(&mut online, PlannedAction::Evade);
    }
    // Granted repositions (BB-8's roll, Snap's boost, Jake Farrell's
    // either) need no icon on the bar while the second slot is armed.
    let free_repo = online.second_pick
        && matches!(
            extras.second,
            Some(
                SecondActionKind::RollOnGreenReveal
                    | SecondActionKind::BoostAfterMove
                    | SecondActionKind::RepositionAfterFocus
                    | SecondActionKind::RepositionAfterAttack
            )
        );
    // Expert Handling rolls without the icon (for a stress).
    let can_roll = bar.contains(&ActionKind::BarrelRoll) || free_repo || extras.expert_roll;
    if keys.just_pressed(KeyCode::Digit4) && can_roll {
        plan_action(&mut online, PlannedAction::BarrelRoll(Side::Left));
    }
    if keys.just_pressed(KeyCode::Digit5) && can_roll {
        plan_action(&mut online, PlannedAction::BarrelRoll(Side::Right));
    }
    // Lieutenant Lorrir: ; and ' roll with the bank templates bending
    // forward; with Shift held they bend backward.
    if extras.bank_roll {
        let forward = !(keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight));
        if keys.just_pressed(KeyCode::Semicolon) {
            plan_action(&mut online, PlannedAction::BarrelRollBank(Side::Left, forward));
        }
        if keys.just_pressed(KeyCode::Quote) {
            plan_action(&mut online, PlannedAction::BarrelRollBank(Side::Right, forward));
        }
    }
    if keys.just_pressed(KeyCode::Digit6) && bar.contains(&ActionKind::TargetLock) {
        online.lock_pick = true;
        online.status = "Target lock: click an enemy ship".into();
    }
    if bar.contains(&ActionKind::Boost) || free_repo {
        if keys.just_pressed(KeyCode::Digit7) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::Straight));
        }
        if keys.just_pressed(KeyCode::Digit8) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::BankLeft));
        }
        if keys.just_pressed(KeyCode::Digit9) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::BankRight));
        }
        // Blue Ace: -/= boost with the turn-1 templates.
        if extras.turn_boost && keys.just_pressed(KeyCode::Minus) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::TurnLeft));
        }
        if extras.turn_boost && keys.just_pressed(KeyCode::Equal) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::TurnRight));
        }
    }
    // Zeta Ace: [/] barrel roll with the straight-2 template.
    if extras.far_roll && bar.contains(&ActionKind::BarrelRoll) {
        if keys.just_pressed(KeyCode::BracketLeft) {
            plan_action(&mut online, PlannedAction::BarrelRollFar(Side::Left));
        }
        if keys.just_pressed(KeyCode::BracketRight) {
            plan_action(&mut online, PlannedAction::BarrelRollFar(Side::Right));
        }
    }
    // B: cycle the bomb card to drop on dial reveal (then none);
    // M: cycle the mine card dropped as this turn's action (then Pass).
    let (reveal, mines, cur_bomb, cur_action) = {
        let Some(snap) = &online.snap else { return };
        let Some(view) = snap.ships.iter().find(|v| v.id.0 == selected) else {
            return;
        };
        let (reveal, mines) = bomb_cards(&game, view);
        (reveal, mines, view.bomb, view.planned_action)
    };
    if keys.just_pressed(KeyCode::KeyB) && !reveal.is_empty() {
        let bomb = match cur_bomb.and_then(|c| reveal.iter().position(|u| *u == c)) {
            None => Some(reveal[0]),
            Some(i) => reveal.get(i + 1).copied(),
        };
        online.send(ClientMsg::PlanBomb { ship_id: ShipId(selected), bomb });
    }
    // U: switch the next toggle card on (Lightning Reflexes, Electronic
    // Baffle, Jan Ors, Decoy); with every one on, switch them all off.
    if keys.just_pressed(KeyCode::KeyU) && !extras.toggles.is_empty() {
        let on: Vec<UpgradeId> = online
            .snap
            .as_ref()
            .and_then(|s| s.ships.iter().find(|v| v.id.0 == selected))
            .map(|v| v.card_uses.clone())
            .unwrap_or_default();
        match extras.toggles.iter().find(|c| !on.contains(c)) {
            Some(&card) => {
                online.send(ClientMsg::PlanCardUse { ship_id: ShipId(selected), card, on: true });
            }
            None => {
                for card in extras.toggles.clone() {
                    online.send(ClientMsg::PlanCardUse {
                        ship_id: ShipId(selected),
                        card,
                        on: false,
                    });
                }
            }
        }
    }
    // K: cycle the card actions (Marksmanship, Rage, Expose, R2-F2), then Pass.
    if keys.just_pressed(KeyCode::KeyK) && !extras.card_actions.is_empty() {
        let cards = &extras.card_actions;
        let next = match cur_action {
            Some(PlannedAction::CardAction(c) | PlannedAction::CardActionAt(c, _)) => {
                cards.iter().position(|u| *u == c).and_then(|i| cards.get(i + 1).copied())
            }
            _ => Some(cards[0]),
        };
        let seismic = next.is_some_and(|c| {
            game.content.upgrades.upgrade(c).and_then(|u| u.effect)
                == Some(UpgradeEffect::SeismicTorpedoAction)
        });
        if seismic {
            // The torpedo needs a target: the next click on an obstacle
            // plans it.
            online.lock_pick = false;
            online.obstacle_pick = next;
            online.status =
                "Seismic Torpedo: click an obstacle (Range 1-2, in your arc after moving)".into();
        } else {
            plan_action(
                &mut online,
                next.map(PlannedAction::CardAction).unwrap_or(PlannedAction::Pass),
            );
        }
    }
    if let Some(card) = online.obstacle_pick
        && buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
    {
        let hit = online.snap.as_ref().and_then(|snap| {
            snap.obstacles
                .iter()
                .find(|o| {
                    obstacle::point_in_convex(cur, &o.polygon())
                        || (o.kind == ObstacleKind::BlackHole
                            && (cur - o.center).dot(cur - o.center) <= 1.0)
                })
                .map(|o| o.id)
        });
        if let Some(id) = hit {
            online.status.clear();
            plan_action(&mut online, PlannedAction::CardActionAt(card, id));
        }
    }
    if keys.just_pressed(KeyCode::KeyM) && !mines.is_empty() {
        let next = match cur_action {
            Some(PlannedAction::DropMine(c)) => {
                mines.iter().position(|u| *u == c).and_then(|i| mines.get(i + 1).copied())
            }
            _ => Some(mines[0]),
        };
        plan_action(&mut online, next.map(PlannedAction::DropMine).unwrap_or(PlannedAction::Pass));
    }
    if online.lock_pick
        && buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
    {
        let target = online.snap.as_ref().and_then(|snap| {
            snap.ships
                .iter()
                .filter(|v| !online.friendly(v) && !v.destroyed)
                .find(|v| {
                    v.pose.is_some_and(|p| {
                        let fp = game.ships.classes[game.class_index(v.class)].footprint;
                        rules::point_in_footprint(p, fp, cur)
                    })
                })
                .map(|v| v.id.0)
        });
        if let Some(t) = target {
            online.status.clear();
            plan_action(&mut online, PlannedAction::TargetLock(ShipId(t)));
        }
    }

    if keys.just_pressed(KeyCode::KeyC) {
        online.send(ClientMsg::CommitPlans);
    }
    if keys.just_pressed(KeyCode::KeyX) {
        online.send(ClientMsg::Resign);
    }
}

/// Laser bolts, impact flash, and fly-by fade for the current attack.
fn draw_attack_fx(
    gizmos: &mut Gizmos,
    game: &Game,
    snap: &Snap,
    anim: &Anim,
    rec: &sf_core::game::AttackRecord,
) {
    use sf_core::ship::Faction;
    let Some(atk_pose) = anim.end_pose(rec.attacker.0, snap) else {
        return;
    };
    let Some(def_pose) = anim.end_pose(rec.defender.0, snap) else {
        return;
    };
    let (atk_view, def_view) = (
        snap.ships.iter().find(|v| v.id.0 == rec.attacker.0),
        snap.ships.iter().find(|v| v.id.0 == rec.defender.0),
    );
    let (Some(atk_view), Some(def_view)) = (atk_view, def_view) else {
        return;
    };
    let atk_class = &game.ships.classes[game.class_index(atk_view.class)];
    let def_class = &game.ships.classes[game.class_index(def_view.class)];

    let bolt_color = match atk_class.faction {
        Faction::RebelAlliance => Color::srgb(1.0, 0.25, 0.15),
        Faction::Empire => Color::srgb(0.25, 1.0, 0.3),
    };
    // Ordnance (torpedoes, missiles) flies as a warhead; turret weapons
    // and turret primaries fire from the base center, all round.
    let card = rec.weapon.and_then(|u| game.content.upgrades.upgrade(u));
    let slot = card.map(|c| c.slot);
    let impact = impact_style(card.and_then(|c| c.effect));
    let ordnance = matches!(slot, Some(Slot::Torpedo | Slot::Missile));
    let from_center = atk_class.turret_primary || slot == Some(Slot::Turret);
    let start = if from_center {
        game.to_world(sf_core::combat::base_center(atk_pose, atk_class.footprint))
    } else {
        game.to_world(atk_pose.anchor)
    };
    let target = game.to_world(sf_core::combat::base_center(def_pose, def_class.footprint));
    let hit = rec.hits + rec.crits > 0;
    let to_target = target - start;
    let dist = to_target.length().max(1.0);
    let dir = to_target / dist;
    let perp = Vec2::new(-dir.y, dir.x);
    // Misses aim visibly wide of the base (alternating side per attack).
    let side = if anim.attack_no.is_multiple_of(2) { 1.0 } else { -1.0 };
    let aim = if hit { target } else { target + perp * 0.7 * render::PX * side };

    let p = (anim.t / ATTACK_DUR).clamp(0.0, 1.0);
    let nbolts = rec.attack_faces.len().min(4);
    let volley = |gizmos: &mut Gizmos, head: Vec2, alpha: f32| {
        if ordnance {
            draw_missile(gizmos, start, head, dir, alpha);
        } else {
            draw_volley(gizmos, start, head, dir, nbolts, bolt_color, alpha);
        }
    };
    if hit {
        // Bolt flight until impact.
        if p < FLY_FRAC {
            let head = start + (aim - start) * (p / FLY_FRAC);
            volley(gizmos, head, 1.0);
        } else {
            // Impact flash: blue-white when shields soaked it, orange for
            // hull damage, and a wide burst on a kill.
            let q = ((p - FLY_FRAC) / (1.0 - FLY_FRAC)).clamp(0.0, 1.0);
            let alpha = 1.0 - q;
            let flash = if rec.hull_lost > 0 {
                Color::srgba(1.0, 0.6, 0.2, alpha)
            } else {
                Color::srgba(0.5, 0.75, 1.0, alpha)
            };
            match impact {
                ImpactStyle::Flash => {
                    gizmos.circle_2d(target, 6.0 + q * 26.0, flash);
                    gizmos.circle_2d(target, 3.0 + q * 14.0, Color::srgba(1.0, 1.0, 0.9, alpha));
                }
                ImpactStyle::Blast => {
                    // Warhead detonation: a big orange fireball and a shock ring.
                    gizmos.circle_2d(target, 8.0 + q * 40.0, Color::srgba(1.0, 0.5, 0.1, alpha));
                    gizmos.circle_2d(target, 4.0 + q * 22.0, Color::srgba(1.0, 0.85, 0.4, alpha));
                    gizmos.circle_2d(
                        target,
                        12.0 + q * 60.0,
                        Color::srgba(1.0, 1.0, 1.0, alpha * 0.4),
                    );
                }
                ImpactStyle::Sparks => {
                    // Ion discharge: jittering blue-white arcs crawling over the hull.
                    let seed = anim.attack_no as u32 * 31 + (anim.t * 40.0) as u32;
                    for k in 0..10u32 {
                        let a0 =
                            hash01(seed.wrapping_mul(7).wrapping_add(k)) * std::f32::consts::TAU;
                        let r0 = 6.0 + hash01(seed.wrapping_add(k * 13)) * 22.0;
                        let a1 = a0 + (hash01(seed.wrapping_add(k * 29)) - 0.5) * 1.2;
                        let p0 = target + Vec2::from_angle(a0) * r0;
                        let p1 = target + Vec2::from_angle(a1) * (r0 + 10.0);
                        gizmos.line_2d(p0, p1, Color::srgba(0.6, 0.85, 1.0, alpha));
                    }
                    gizmos.circle_2d(target, 4.0 + q * 10.0, Color::srgba(0.8, 0.95, 1.0, alpha));
                }
                ImpactStyle::Fragments => {
                    // Fragmentation: a cloud of shards flying apart.
                    let seed = anim.attack_no as u32 * 17;
                    for k in 0..18u32 {
                        let ang = hash01(seed.wrapping_add(k * 11)) * std::f32::consts::TAU;
                        let speed = 30.0 + hash01(seed.wrapping_add(k * 23)) * 50.0;
                        let d = Vec2::from_angle(ang);
                        let pos = target + d * (q * speed);
                        gizmos.line_2d(pos, pos + d * 4.0, Color::srgba(1.0, 0.8, 0.5, alpha));
                    }
                    gizmos.circle_2d(target, 5.0 + q * 16.0, flash);
                }
            }
            if rec.defender_destroyed {
                gizmos.circle_2d(target, 10.0 + q * 55.0, Color::srgba(1.0, 0.45, 0.1, alpha));
            }
        }
    } else {
        // Fly past the target and fade out on the way.
        let beyond = aim + dir * 3.0 * render::PX;
        let head = start + (beyond - start) * p;
        let fade = if p > 0.55 { 1.0 - (p - 0.55) / 0.45 } else { 1.0 };
        volley(gizmos, head, fade);
    }
}

/// How a weapon's hit is drawn.
#[derive(Clone, Copy, PartialEq)]
enum ImpactStyle {
    /// Laser bolts: a quick flash.
    Flash,
    /// Warheads: fireball and shock ring.
    Blast,
    /// Ion weapons: electric discharge.
    Sparks,
    /// Cluster/assault/flechette weapons: shards flying apart.
    Fragments,
}

fn impact_style(effect: Option<UpgradeEffect>) -> ImpactStyle {
    use UpgradeEffect::*;
    match effect {
        Some(TurretIonOneDamage | CannonIonOneDamage | MissileIonOneDamage | TorpedoIonSplash) => {
            ImpactStyle::Sparks
        }
        Some(
            MissileAttackTwice
            | MissileSplashRange1
            | CannonOneDamageAndStress
            | TorpedoStressIfHullLow,
        ) => ImpactStyle::Fragments,
        Some(
            TorpedoFocusToCrit
            | TorpedoBlanksToFocus
            | TorpedoStripShield
            | MissileBlankToHit
            | MissileDenyEvadeTokens
            | MissileFaceupDamage
            | RocketExtraDiceByAgility
            | MissileFriendsLockOnHit,
        ) => ImpactStyle::Blast,
        _ => ImpactStyle::Flash,
    }
}

/// Cheap deterministic 0..1 noise for effect jitter.
fn hash01(x: u32) -> f32 {
    let mut h = x.wrapping_mul(0x9E37_79B9);
    h ^= h >> 16;
    h = h.wrapping_mul(0x85EB_CA6B);
    h ^= h >> 13;
    (h & 0xFFFF) as f32 / 65535.0
}

/// A warhead in flight: pointed body, fins, exhaust flare and a fading
/// smoke trail back toward the launcher.
fn draw_missile(gizmos: &mut Gizmos, start: Vec2, head: Vec2, dir: Vec2, alpha: f32) {
    if (head - start).dot(dir) <= 0.0 {
        return;
    }
    let a = alpha.clamp(0.0, 1.0);
    let body = Color::srgba(0.95, 0.95, 0.9, a);
    let flame = Color::srgba(1.0, 0.55, 0.15, a);
    let perp = Vec2::new(-dir.y, dir.x);
    let len = 18.0_f32.min((head - start).length());
    let tail = head - dir * len;
    // Body with a pointed nose.
    gizmos.line_2d(tail + perp * 2.0, head - dir * 4.0 + perp * 2.0, body);
    gizmos.line_2d(tail - perp * 2.0, head - dir * 4.0 - perp * 2.0, body);
    gizmos.line_2d(head - dir * 4.0 + perp * 2.0, head, body);
    gizmos.line_2d(head - dir * 4.0 - perp * 2.0, head, body);
    gizmos.line_2d(tail + perp * 2.0, tail - perp * 2.0, body);
    // Fins.
    gizmos.line_2d(tail + perp * 2.0, tail - dir * 4.0 + perp * 6.0, body);
    gizmos.line_2d(tail - perp * 2.0, tail - dir * 4.0 - perp * 6.0, body);
    // Exhaust flare and a dotted smoke trail.
    gizmos.circle_2d(tail - dir * 3.0, 3.0, flame);
    let trail = (tail - start).length();
    let mut d = 10.0;
    while d < trail {
        let fade = (1.0 - d / trail.max(1.0)) * 0.5 * a;
        gizmos.circle_2d(tail - dir * d, 1.5, Color::srgba(0.8, 0.8, 0.8, fade));
        d += 10.0;
    }
}

fn draw_volley(
    gizmos: &mut Gizmos,
    start: Vec2,
    head: Vec2,
    dir: Vec2,
    nbolts: usize,
    color: Color,
    alpha: f32,
) {
    let c = color.with_alpha(alpha.clamp(0.0, 1.0));
    for b in 0..nbolts {
        let h = head - dir * (b as f32 * 24.0);
        // Only draw bolts that have left the muzzle.
        if (h - start).dot(dir) <= 0.0 {
            continue;
        }
        let tail = h - dir * 14.0;
        let tail = if (tail - start).dot(dir) < 0.0 { start } else { tail };
        gizmos.line_2d(tail, h, c);
        // Slight parallel line for perceived thickness.
        let off = Vec2::new(-dir.y, dir.x) * 1.2;
        gizmos.line_2d(tail + off, h + off, c);
    }
}

fn action_name(game: &Game, snap: Option<&Snap>, a: PlannedAction) -> String {
    match a {
        PlannedAction::Pass => "Pass".into(),
        PlannedAction::Focus => "Focus".into(),
        PlannedAction::Evade => "Evade".into(),
        PlannedAction::BarrelRoll(Side::Left) => "Barrel Roll L".into(),
        PlannedAction::BarrelRoll(Side::Right) => "Barrel Roll R".into(),
        PlannedAction::Boost(BoostDir::Straight) => "Boost".into(),
        PlannedAction::Boost(BoostDir::BankLeft) => "Boost L".into(),
        PlannedAction::Boost(BoostDir::BankRight) => "Boost R".into(),
        PlannedAction::Boost(BoostDir::TurnLeft) => "Boost turn L".into(),
        PlannedAction::Boost(BoostDir::TurnRight) => "Boost turn R".into(),
        PlannedAction::TargetLock(id) => format!("Lock {}", callsign(snap, id.0)),
        PlannedAction::DropMine(card) => format!("Drop {}", card_name(game, card)),
        PlannedAction::BarrelRollFar(Side::Left) => "Far Roll L".into(),
        PlannedAction::BarrelRollFar(Side::Right) => "Far Roll R".into(),
        PlannedAction::CardAction(card) => format!("{} action", card_name(game, card)),
        PlannedAction::CardActionAt(card, _) => format!("{} at obstacle", card_name(game, card)),
        PlannedAction::Protect => "Protect the shuttle".into(),
        PlannedAction::BarrelRollBank(side, forward) => format!(
            "Bank roll {} {}",
            if side == Side::Left { "L" } else { "R" },
            if forward { "fwd" } else { "aft" }
        ),
    }
}

/// Why a ship gets a second action, for the HUD.
fn second_action_label(kind: SecondActionKind) -> &'static str {
    match kind {
        SecondActionKind::FreeBarAction => "Push the Limit: 2nd bar action, then stress",
        SecondActionKind::TwoActions => "Darth Vader: 2nd action",
        SecondActionKind::BoostAfterMove => "Snap: free boost after a speed 2-4 move",
        SecondActionKind::RepositionAfterFocus => "Jake Farrell: boost/roll after a focus",
        SecondActionKind::RollOnGreenReveal => "BB-8: barrel roll before a green move",
        SecondActionKind::RepositionAfterAttack => "Turr Phennir: boost/roll after attacking",
        SecondActionKind::CardActionThenStress => {
            "Experimental Interface: free card action (K), then stress"
        }
    }
}

/// The ship's equipped bomb cards: (dial-reveal bombs, mines).
fn bomb_cards(game: &Game, view: &ShipView) -> (Vec<UpgradeId>, Vec<UpgradeId>) {
    let kind = |u: &UpgradeId| {
        game.content.upgrades.upgrade(*u).and_then(|c| c.effect).and_then(BombKind::from_effect)
    };
    let pick = |mine: bool| -> Vec<UpgradeId> {
        view.upgrade_ids
            .iter()
            .filter(|u| kind(u).is_some_and(|k| k.is_mine() == mine))
            .copied()
            .collect()
    };
    (pick(false), pick(true))
}

fn card_name(game: &Game, card: UpgradeId) -> String {
    game.content.upgrades.upgrade(card).map(|c| c.name.clone()).unwrap_or_else(|| "card".into())
}

/// One-line narration of a detonation for the HUD.
fn detonation_line(snap: &Snap, d: &Detonation) -> String {
    if d.hits.is_empty() {
        return format!("{} detonates — nobody within Range 1", d.token.kind.name());
    }
    let hits: Vec<String> = d
        .hits
        .iter()
        .map(|h| {
            let mut what = Vec::new();
            if h.damage > 0 {
                what.push(format!("{} damage", h.damage));
            }
            if h.crits > 0 && d.token.kind == BombKind::Proton {
                what.push("faceup card".to_string());
            } else if h.crits > 0 {
                what.push(format!("{} critical", h.crits));
            }
            if h.ion > 0 {
                what.push(format!("{} ion", h.ion));
            }
            if h.stress > 0 {
                what.push("stress".to_string());
            }
            let died = if h.destroyed { " DESTROYED" } else { "" };
            format!("{}: {}{died}", callsign(Some(snap), h.ship.0), what.join(", "))
        })
        .collect();
    format!("{} detonates — {}", d.token.kind.name(), hits.join("; "))
}

/// An obstacle token: asteroids as a craggy outline with a few inner
/// fracture lines, debris as a dotted cloud.
/// A satellite token (mission 3): a slowly turning cyan diamond with a
/// dish ring.
fn draw_satellite(gizmos: &mut Gizmos, game: &Game, pos: GVec2, t: f32) {
    let c = game.to_world(pos);
    let half = (sf_core::mission::SATELLITE_SIZE / 2.0) as f32 * render::PX;
    let a = t * 0.6;
    let pts: Vec<Vec2> = (0..5)
        .map(|k| {
            let ang = a + k as f32 * std::f32::consts::FRAC_PI_2;
            c + Vec2::new(ang.cos(), ang.sin()) * half
        })
        .collect();
    gizmos.linestrip_2d(pts, Color::srgb(0.4, 0.95, 1.0));
    gizmos.circle_2d(c, half * 0.45, Color::srgba(0.4, 0.95, 1.0, 0.7));
}

fn draw_obstacle(gizmos: &mut Gizmos, game: &Game, o: &Obstacle, t: f32) {
    let poly: Vec<Vec2> = o.polygon().into_iter().map(|p| game.to_world(p)).collect();
    let center = game.to_world(o.center);
    match o.kind {
        ObstacleKind::BlackHole => {
            // A solid black core (stacked rings) with three arms of gas
            // spiralling in, brighter and faster toward the horizon.
            let core = sf_core::obstacle::CORE_RADIUS as f32 * render::PX;
            let mut r = core;
            while r > 0.5 {
                gizmos.circle_2d(center, r, Color::BLACK);
                r -= 1.2;
            }
            gizmos.circle_2d(center, core + 1.5, Color::srgba(0.6, 0.4, 1.0, 0.8));
            for arm in 0..3u32 {
                let phase = t * 0.9 + arm as f32 * std::f32::consts::TAU / 3.0;
                let mut prev: Option<Vec2> = None;
                for step in 0..48u32 {
                    let k = step as f32 / 48.0;
                    let radius = core + (1.0 - k) * 2.2 * render::PX;
                    let a = phase + k * 3.6 * std::f32::consts::PI;
                    let p = center + Vec2::from_angle(a) * radius;
                    if let Some(q) = prev {
                        let alpha = 0.15 + k * 0.7;
                        gizmos.line_2d(q, p, Color::srgba(0.55 + k * 0.3, 0.35, 0.95, alpha));
                    }
                    prev = Some(p);
                }
            }
            // Drifting gas motes on the outer disc.
            let seed = o.id.wrapping_mul(29);
            for k in 0..10u32 {
                let a0 = hash01(seed.wrapping_add(k * 7)) * std::f32::consts::TAU;
                let r0 = 1.0 + hash01(seed.wrapping_add(k * 13)) * 1.6;
                let a = a0 + t * (0.4 + 0.6 / r0);
                let p = center + Vec2::from_angle(a) * r0 * render::PX;
                gizmos.circle_2d(p, 1.5, Color::srgba(0.7, 0.55, 1.0, 0.5));
            }
        }
        ObstacleKind::Asteroid => {
            let rim = Color::srgb(0.55, 0.5, 0.42);
            let mut pts = poly.clone();
            pts.push(poly[0]);
            gizmos.linestrip_2d(pts, rim);
            let inner = Color::srgba(0.45, 0.4, 0.33, 0.8);
            for (k, p) in poly.iter().enumerate() {
                if k % 2 == 0 {
                    gizmos.line_2d(center + (*p - center) * 0.15, *p, inner);
                }
            }
            let seed = o.id.wrapping_mul(53);
            for k in 0..3u32 {
                let a = hash01(seed.wrapping_add(k * 7)) * std::f32::consts::TAU;
                let r = 4.0 + hash01(seed.wrapping_add(k * 11)) * 6.0;
                let at = center + Vec2::from_angle(a) * (r + 6.0);
                gizmos.circle_2d(at, r * 0.35, inner);
            }
        }
        ObstacleKind::Debris => {
            let tint = Color::srgba(0.5, 0.6, 0.7, 0.7);
            let seed = o.id.wrapping_mul(71);
            for (k, p) in poly.iter().enumerate() {
                let q = poly[(k + 1) % poly.len()];
                for step in 0..4 {
                    let t = step as f32 / 4.0;
                    let at = *p + (q - *p) * t;
                    gizmos.circle_2d(
                        at,
                        1.5 + hash01(seed.wrapping_add(k as u32 * 9 + step)) * 2.0,
                        tint,
                    );
                }
            }
            for k in 0..6u32 {
                let a = hash01(seed.wrapping_add(k * 13)) * std::f32::consts::TAU;
                let r = hash01(seed.wrapping_add(k * 17)) * 14.0;
                gizmos.circle_2d(center + Vec2::from_angle(a) * r, 2.0, tint);
            }
        }
    }
}

/// A bomb or mine token: a square outline in the owner's shade with a
/// symbol for its kind.
fn draw_bomb_token(gizmos: &mut Gizmos, game: &Game, t: &BombToken, own: bool) {
    let c = t.corners();
    let outline = if own { Color::srgb(0.85, 0.85, 0.85) } else { Color::srgb(0.7, 0.45, 0.45) };
    for i in 0..4 {
        gizmos.line_2d(game.to_world(c[i]), game.to_world(c[(i + 1) % 4]), outline);
    }
    let center = game.to_world(t.center());
    let r = render::PX * 0.32;
    let tint = bomb_color(t.kind);
    match t.kind {
        BombKind::Proton | BombKind::Seismic | BombKind::Thermal | BombKind::SeismicTorpedo => {
            // A round bomb with a fuse tick.
            gizmos.circle_2d(center, r, tint);
            gizmos.circle_2d(center, r * 0.55, tint);
            gizmos.line_2d(center + Vec2::new(0.0, r), center + Vec2::new(r * 0.4, r * 1.5), tint);
        }
        BombKind::Ion => {
            gizmos.circle_2d(center, r, tint);
            for k in 0..4 {
                let a = k as f32 * std::f32::consts::FRAC_PI_2 + 0.4;
                gizmos.line_2d(center, center + Vec2::from_angle(a) * r * 0.9, tint);
            }
        }
        BombKind::ProximityMine | BombKind::ClusterMine => {
            // Spiked mine: circle with eight studs.
            gizmos.circle_2d(center, r * 0.7, tint);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::FRAC_PI_4;
                let dir = Vec2::from_angle(a);
                gizmos.line_2d(center + dir * r * 0.7, center + dir * r * 1.1, tint);
            }
        }
        BombKind::ConnerNet => {
            // A net: crossed lines inside the square.
            let s = r * 1.1;
            for k in -1..=1 {
                let o = k as f32 * s * 0.66;
                gizmos.line_2d(center + Vec2::new(-s, o), center + Vec2::new(s, o), tint);
                gizmos.line_2d(center + Vec2::new(o, -s), center + Vec2::new(o, s), tint);
            }
        }
    }
}

fn bomb_color(kind: BombKind) -> Color {
    match kind {
        BombKind::Proton => Color::srgb(1.0, 0.6, 0.2),
        BombKind::Seismic | BombKind::SeismicTorpedo => Color::srgb(1.0, 0.8, 0.3),
        BombKind::Thermal => Color::srgb(1.0, 0.35, 0.25),
        BombKind::Ion | BombKind::ConnerNet => Color::srgb(0.5, 0.8, 1.0),
        BombKind::ProximityMine | BombKind::ClusterMine => Color::srgb(1.0, 0.45, 0.35),
    }
}

/// Detonation: an expanding blast out to the token's Range-1 reach (a
/// short one for mines, which only hurt the ship on top of them), with
/// per-kind flavour — fireball, ion sparks, or flying fragments — and a
/// flash on every ship it caught.
fn draw_detonation(gizmos: &mut Gizmos, game: &Game, snap: &Snap, anim: &Anim, d: &Detonation) {
    let center = game.to_world(d.token.center());
    let q = (anim.t / DETONATION_DUR).clamp(0.0, 1.0);
    let alpha = 1.0 - q * q;
    let reach =
        if d.token.kind.is_mine() { 1.2 } else { (sf_core::combat::RANGE_BAND_UNITS + 0.5) as f32 }
            * render::PX;
    let tint = bomb_color(d.token.kind).with_alpha(alpha);
    match d.token.kind {
        BombKind::Proton | BombKind::Seismic | BombKind::Thermal | BombKind::SeismicTorpedo => {
            // Fireball core, then a white shock ring racing to Range 1.
            let core = (q * 3.0).min(1.0);
            gizmos.circle_2d(center, 6.0 + core * 34.0, Color::srgba(1.0, 0.5, 0.1, alpha));
            gizmos.circle_2d(center, 3.0 + core * 18.0, Color::srgba(1.0, 0.9, 0.5, alpha));
            gizmos.circle_2d(center, q * reach, Color::srgba(1.0, 1.0, 1.0, alpha * 0.6));
            gizmos.circle_2d(center, (q * reach - 8.0).max(0.0), tint);
        }
        BombKind::Ion | BombKind::ConnerNet => {
            // Blue shock rings and jittering arcs.
            gizmos.circle_2d(center, q * reach, Color::srgba(0.6, 0.85, 1.0, alpha));
            gizmos.circle_2d(center, q * reach * 0.6, Color::srgba(0.8, 0.95, 1.0, alpha * 0.7));
            let seed = d.token.id.wrapping_mul(97).wrapping_add((anim.t * 40.0) as u32);
            for k in 0..14u32 {
                let a0 = hash01(seed.wrapping_mul(7).wrapping_add(k)) * std::f32::consts::TAU;
                let r0 = hash01(seed.wrapping_add(k * 13)) * q * reach;
                let a1 = a0 + (hash01(seed.wrapping_add(k * 29)) - 0.5) * 1.2;
                let p0 = center + Vec2::from_angle(a0) * r0;
                let p1 = center + Vec2::from_angle(a1) * (r0 + 12.0);
                gizmos.line_2d(p0, p1, Color::srgba(0.6, 0.85, 1.0, alpha));
            }
        }
        BombKind::ProximityMine | BombKind::ClusterMine => {
            // Flash plus fragments flying outward.
            gizmos.circle_2d(
                center,
                4.0 + (q * 3.0).min(1.0) * 16.0,
                Color::srgba(1.0, 0.9, 0.6, alpha),
            );
            let seed = d.token.id.wrapping_mul(131);
            for k in 0..16u32 {
                let a = hash01(seed.wrapping_add(k * 17)) * std::f32::consts::TAU;
                let speed = 0.6 + hash01(seed.wrapping_add(k * 41)) * 0.8;
                let dir = Vec2::from_angle(a);
                let head = center + dir * q * reach * 1.4 * speed;
                gizmos.line_2d(head - dir * 6.0, head, tint);
            }
        }
    }
    for hit in &d.hits {
        let Some(pose) = anim.end_pose(hit.ship.0, snap) else { continue };
        let Some(view) = snap.ships.iter().find(|v| v.id.0 == hit.ship.0) else { continue };
        let fp = game.ships.classes[game.class_index(view.class)].footprint;
        let at = game.to_world(sf_core::combat::base_center(pose, fp));
        let flash = if hit.ion > 0 && hit.damage == 0 {
            Color::srgba(0.5, 0.75, 1.0, alpha)
        } else {
            Color::srgba(1.0, 0.6, 0.2, alpha)
        };
        let wide = if hit.destroyed { 2.0 } else { 1.0 };
        gizmos.circle_2d(at, (6.0 + q * 26.0) * wide, flash);
        gizmos.circle_2d(at, 3.0 + q * 12.0, Color::srgba(1.0, 1.0, 0.9, alpha));
    }
}

fn leave_keys(
    online: Res<Online>,
    keys: Res<ButtonInput<KeyCode>>,
    mut next: ResMut<NextState<Screen>>,
) {
    if keys.just_pressed(KeyCode::Escape) && (online.over.is_some() || online.demo) {
        next.set(Screen::Menu);
    }
}

fn draw(
    mut gizmos: Gizmos,
    time: Res<Time>,
    online: Res<Online>,
    game: Res<Game>,
    arcs: Res<ShowArcs>,
    art: Res<ClassArt>,
    mut ghost: Query<(&mut Sprite, &mut Transform, &mut Visibility), With<Ghost>>,
    mut bullseye: ResMut<render::BullseyePreview>,
    cursor: Res<CursorUnits>,
    mut hover: ResMut<render::Hover>,
) {
    bullseye.0 = None;
    hover.0 = None;
    render::draw_board_frame(&mut gizmos, &game);
    let Ok((mut gsprite, mut gtf, mut gvis)) = ghost.single_mut() else {
        return;
    };
    *gvis = Visibility::Hidden;
    let Some(snap) = &online.snap else { return };
    let seat = online.my_seat();
    // Our placement zone(s) while placing; the standard bands otherwise.
    if snap.phase == Phase::Placement && !snap.zones.is_empty() {
        for z in &snap.zones {
            render::draw_zone(&mut gizmos, &game, *z, Color::srgba(0.3, 0.9, 0.4, 0.6));
        }
    } else if snap.mission.is_none() {
        let d = game.board.deploy_depth;
        let (w, h) = (game.board.width, game.board.height);
        render::draw_zone(&mut gizmos, &game, (0.0, 0.0, w, d), Color::srgba(0.3, 0.9, 0.4, 0.4));
        render::draw_zone(&mut gizmos, &game, (0.0, h - d, w, h), Color::srgba(0.9, 0.4, 0.3, 0.4));
    }
    if let Some(m) = &snap.mission {
        for s in m.satellites.iter().filter(|s| s.on_board()) {
            draw_satellite(&mut gizmos, &game, s.pos, time.elapsed_secs());
        }
    }
    let tokens: &[BombToken] = match &online.anim {
        Some(a) => &a.tokens,
        None => &snap.bombs,
    };
    let gone: &[u32] = online.anim.as_ref().map(|a| a.removed_obstacles.as_slice()).unwrap_or(&[]);
    for o in snap.obstacles.iter().filter(|o| !gone.contains(&o.id)) {
        draw_obstacle(&mut gizmos, &game, o, time.elapsed_secs());
    }
    // Effects demo: keep Onyx-2's firing arc up so the asteroid's shadow
    // over Gold-1 is on show.
    if online.demo
        && let Some(v) = snap.ships.iter().find(|v| v.id.0 == 1)
        && let Some(p) = v.pose
    {
        let fp = game.ships.classes[game.class_index(v.class)].footprint;
        render::draw_firing_arc_with(&mut gizmos, &game, p, fp, &snap.obstacles, 0.5);
    }
    for t in tokens {
        draw_bomb_token(&mut gizmos, &game, t, t.owner.0 == u32::from(seat));
    }
    // Name tag for the ship under the cursor (post-move pose while a turn
    // animates, provisional pose while placing).
    hover.0 = render::hovered(
        cursor.0,
        snap.ships.iter().filter(|v| !v.destroyed).filter_map(|v| {
            let pose = match &online.anim {
                Some(a) => a.end_pose(v.id.0, snap)?,
                None => online.effective_pose(v)?,
            };
            Some((v.callsign.as_str(), &game.ships.classes[game.class_index(v.class)], pose))
        }),
    );

    for view in &snap.ships {
        let Some(pose) = online.effective_pose(view) else {
            continue;
        };
        if view.destroyed && online.anim.is_none() {
            continue;
        }
        let class = &game.ships.classes[game.class_index(view.class)];
        let own = view.owner.0 == seat as u32;
        let ally = !own && online.friendly(view);
        let selected = own && online.sel == Some(view.id.0);
        let mut color = match (own, ally, selected) {
            (_, _, true) => Color::srgb(1.0, 0.9, 0.3),
            (true, _, _) => Color::srgba(0.3, 0.9, 0.4, 0.7),
            (_, true, _) => Color::srgba(0.4, 0.7, 1.0, 0.7),
            _ => Color::srgba(0.9, 0.4, 0.3, 0.7),
        };
        // Placement legality tint for own provisional poses.
        if own && snap.phase == Phase::Placement {
            let zone_seat = online.my_edge(&snap.teams);
            let corners = rules::footprint_corners(pose, class.footprint);
            let on_rock =
                snap.obstacles.iter().any(|o| obstacle::convex_overlap(&corners, &o.polygon()));
            let legal = if snap.zones.is_empty() {
                rules::placement_legal(&game.board, zone_seat, pose, class.footprint, &[])
            } else {
                rules::placement_legal_in(&snap.zones, pose, class.footprint, &[])
            };
            if on_rock || legal.is_err() {
                color = Color::srgb(1.0, 0.35, 0.35);
            }
        }
        render::draw_base(&mut gizmos, &game, pose, class.footprint, color);
        if view.satellites > 0 {
            // Carrying scanned satellite data (mission 3).
            let c = combat::base_center(pose, class.footprint);
            gizmos.circle_2d(game.to_world(c), 5.0, Color::srgb(0.4, 0.95, 1.0));
        }
        if own && view.plan.is_some() && snap.phase == Phase::Planning {
            gizmos.circle_2d(game.to_world(pose.anchor), 4.0, Color::srgb(0.4, 1.0, 0.9));
        }
    }

    // Animation overlays: the current move's path, laser bolts, or the
    // Declare Target prompt (attacker's arc + highlighted candidates).
    if let Some(a) = &online.anim {
        let fp_of = |id: u32| {
            snap.ships
                .iter()
                .find(|v| v.id.0 == id)
                .map(|v| game.ships.classes[game.class_index(v.class)].footprint)
        };
        match &a.current {
            Some(AnimItem::Move(mv)) => {
                render::draw_path(
                    &mut gizmos,
                    &game,
                    &mv.path,
                    render::difficulty_color(mv.maneuver.difficulty),
                );
            }
            Some(AnimItem::Attack { rec, .. }) => {
                draw_attack_fx(&mut gizmos, &game, snap, a, rec);
            }
            Some(AnimItem::Detonation(d)) => {
                draw_detonation(&mut gizmos, &game, snap, a, d);
            }
            Some(AnimItem::Prompt { attacker, options, .. }) => {
                if let (Some(ap), Some(fp)) = (a.end_pose(*attacker, snap), fp_of(*attacker)) {
                    render::draw_firing_arc_with(&mut gizmos, &game, ap, fp, &snap.obstacles, 0.6);
                    bullseye.0 = Some(ap);
                }
                let hi = Color::srgb(1.0, 0.95, 0.2);
                for id in options.iter().map(|o| o.target) {
                    if let (Some(p), Some(fp)) = (a.end_pose(id, snap), fp_of(id)) {
                        render::draw_base(&mut gizmos, &game, p, fp, hi);
                        gizmos.circle_2d(game.to_world(p.anchor), 9.0, hi);
                    }
                }
            }
            _ => {}
        }
        return;
    }

    // Ghost preview while planning.
    if snap.phase != Phase::Planning {
        return;
    }
    let Some(sel) = online.sel else { return };
    let Some(view) = snap.ships.iter().find(|v| v.id.0 == sel) else {
        return;
    };
    let Some(pose) = view.pose else { return };
    let class_idx = game.class_index(view.class);
    let class = &game.ships.classes[class_idx];
    let dial = game.dial(class);
    if dial.is_empty() {
        return;
    }
    let man = dial[online.dial_idx.min(dial.len() - 1)];
    let Ok(path) = maneuver::sample_path(pose, man) else {
        return;
    };
    let end = *path.last().unwrap();
    let color = render::difficulty_color(man.difficulty);
    render::draw_path(&mut gizmos, &game, &path, color);
    render::draw_base(&mut gizmos, &game, end, class.footprint, color);
    render::draw_heading_arrow(&mut gizmos, &game, end, color);
    if arcs.0 {
        render::draw_firing_arc_with(
            &mut gizmos,
            &game,
            end,
            class.footprint,
            &snap.obstacles,
            0.7,
        );
        bullseye.0 = Some(end);
    }
    let (size, tf) = render::ship_visual(class, end, &game, 2.0);
    gsprite.image = art.0[class_idx].clone();
    gsprite.custom_size = Some(size);
    gsprite.color = Color::srgba(1.0, 1.0, 1.0, 0.35);
    *gtf = tf;
    *gvis = Visibility::Visible;
}

fn hud(online: Res<Online>, game: Res<Game>, mut hud: Query<&mut Text, With<HudText>>) {
    let Ok(mut text) = hud.single_mut() else {
        return;
    };
    if let Some(over) = &online.over {
        text.0 = format!("{over}\nEsc: back to menu");
        return;
    }
    let Some(snap) = &online.snap else {
        text.0 = online.status.clone();
        return;
    };
    let seat = online.my_seat();
    let team_of = |s: usize| snap.teams.get(s).copied().unwrap_or(s as u8);
    let init = if snap.initiative == seat {
        "you".to_string()
    } else if team_of(snap.initiative as usize) == online.team {
        "ally".to_string()
    } else {
        "enemy".to_string()
    };
    let name_of = |s: usize| {
        if s == seat as usize {
            "you".to_string()
        } else {
            online.players.get(s).cloned().unwrap_or_else(|| format!("seat {s}"))
        }
    };
    let committed: Vec<String> = (0..snap.committed.len())
        .map(|s| format!("{} {}", name_of(s), if snap.committed[s] { "✔" } else { "—" }))
        .collect();
    let committed = format!("committed: {}", committed.join(" / "));
    let mine: u32 =
        (0..snap.totals.len()).filter(|s| team_of(*s) == online.team).map(|s| snap.totals[s]).sum();
    let theirs: u32 =
        (0..snap.totals.len()).filter(|s| team_of(*s) != online.team).map(|s| snap.totals[s]).sum();
    let scenario = online.setup.as_ref().map(|s| format!(" | {}", s.summary())).unwrap_or_default();
    let mut lines = vec![format!(
        "TURN {} | {:?} | initiative: {init} ({mine} vs {theirs} pts) | {committed}{scenario}",
        snap.turn, snap.phase,
    )];
    if let Some(m) = &snap.mission {
        let mut line = format!(
            "MISSION {}: {} — your objective: {}",
            m.kind.number(),
            m.kind.name(),
            m.objective
        );
        match m.kind {
            MissionKind::AsteroidRun if snap.turn < sf_core::mission::REPAIR_ROUND => {
                line.push_str(&format!(
                    " | disabled ship repaired in round {}",
                    sf_core::mission::REPAIR_ROUND
                ));
            }
            MissionKind::DarkWhispers => {
                let on_board = m.satellites.iter().filter(|s| s.on_board()).count();
                let supply = m.satellites.iter().filter(|s| s.supply).count();
                line.push_str(&format!(
                    " | satellites: {on_board} on the board, {} scanned, {supply} in the supply",
                    m.satellites.len() - on_board - supply
                ));
            }
            _ => {}
        }
        if snap.phase == Phase::Placement && snap.turn > 1 {
            line.push_str(" | REINFORCEMENT: place the new ship within Range 1 of the edge");
        }
        lines.push(line);
    }
    if let Some(view) = online.sel.and_then(|id| snap.ships.iter().find(|v| v.id.0 == id)) {
        let class = &game.ships.classes[game.class_index(view.class)];
        let mut line = format!(
            "{} ({}, {} PS{}) — hull {}/{} shields {}/{} stress {} focus {} evade {}",
            view.callsign,
            class.name,
            view.pilot,
            view.skill,
            view.hull,
            view.max_hull,
            view.shields,
            view.max_shields,
            view.stress,
            view.focus,
            view.evade
        );
        if view.ion > 0 {
            line.push_str(&format!(" ion {}", view.ion));
        }
        if let Some(l) = view.lock {
            line.push_str(&format!(" lock {}", callsign(Some(snap), l.0)));
        }
        if let Some(l) = view.lock2 {
            line.push_str(&format!(" + lock {}", callsign(Some(snap), l.0)));
        }
        if view.on_asteroid {
            line.push_str(" | ON ASTEROID: no attack this round");
        }
        if let Some(m) = &snap.mission {
            if m.shuttle == Some(view.id) {
                line.push_str(" | SENATOR'S SHUTTLE: no actions, no attack, crits are hits");
            }
            if m.disabled == Some(view.id) && snap.turn < sf_core::mission::REPAIR_ROUND {
                line.push_str(" | DISABLED: speed 1-2 only until round 5");
            }
        }
        if view.satellites > 0 {
            line.push_str(&format!(" | carrying {} satellite(s)", view.satellites));
        }
        if view.escaped {
            line.push_str(" | ESCAPED the battlefield");
        }
        if !view.crits.is_empty() {
            let names: Vec<&str> = view.crits.iter().map(|c| c.name()).collect();
            line.push_str(&format!(" | crits: {}", names.join(", ")));
        }
        if let Some(a) = view.planned_action {
            line.push_str(&format!(" | action: {}", action_name(&game, Some(snap), a)));
        }
        if let Some(a) = view.planned_action2 {
            line.push_str(&format!(" | 2nd: {}", action_name(&game, Some(snap), a)));
        }
        if online.second_pick {
            line.push_str(" | SECOND ACTION: press an action key (1 clears)");
        }
        if let Some(b) = view.bomb {
            line.push_str(&format!(" | bomb: {} (drops on reveal)", card_name(&game, b)));
        }
        if !view.card_uses.is_empty() {
            let names: Vec<String> = view.card_uses.iter().map(|c| card_name(&game, *c)).collect();
            line.push_str(&format!(" | using: {}", names.join(", ")));
        }
        if snap.phase == Phase::Planning {
            let dial = game.dial(class);
            if !dial.is_empty() {
                let man = dial[online.dial_idx.min(dial.len() - 1)];
                let locked = view.stress > 0 && man.difficulty == Difficulty::Hard;
                line.push_str(&format!(
                    " | [{}/{}] {} {} ({}){}",
                    online.dial_idx.min(dial.len() - 1) + 1,
                    dial.len(),
                    render::steer_name(man.steer),
                    man.distance,
                    render::difficulty_label(man.difficulty),
                    if locked { " — STRESSED, red locked" } else { "" },
                ));
            }
        }
        lines.push(line);
        // Weapon readiness from the current positions (own ships only;
        // during Planning this is before anyone moves).
        if view.owner.0 == u32::from(seat) && view.pose.is_some() && !view.destroyed {
            let name = |id: ShipId| callsign(Some(snap), id.0);
            let parts: Vec<String> =
                weapon_status(&game.content, &snap.ships, &snap.obstacles, view)
                    .iter()
                    .map(|w| {
                        let state = match &w.state {
                            WeaponState::Ready { target, range, obstructed } => {
                                let o = if *obstructed { ", obstructed" } else { "" };
                                format!("ready vs {} R{range}{o}", name(*target))
                            }
                            WeaponState::NeedsLock { target } => {
                                format!("needs target lock on {}", name(*target))
                            }
                            WeaponState::NeedsFocus { target } => {
                                format!("needs focus token (vs {})", name(*target))
                            }
                            WeaponState::NoTarget => "no target in range/arc".to_string(),
                            WeaponState::Offline => "OFFLINE (weapons failure)".to_string(),
                            WeaponState::Grounded => "no attack (on an asteroid)".to_string(),
                        };
                        format!("{}: {state}", w.name)
                    })
                    .collect();
            let when = if snap.phase == Phase::Planning { " (before moving)" } else { "" };
            lines.push(format!("weapons{when}: {}", parts.join(" • ")));
        }
    }
    let help = match snap.phase {
        Phase::Placement => {
            "drag ships • Q/E or scroll rotates • N: rename • A: submit all • +/- zoom, right-drag pan, Home reset".to_string()
        }
        Phase::Planning => {
            // Only the selected ship's own action bar is offered.
            let bar = online
                .sel
                .and_then(|id| snap.ships.iter().find(|v| v.id.0 == id))
                .map(|v| v.actions.clone())
                .unwrap_or_default();
            let mut acts = vec!["1 Pass"];
            if bar.contains(&ActionKind::Focus) {
                acts.push("2 Focus");
            }
            if bar.contains(&ActionKind::Evade) {
                acts.push("3 Evade");
            }
            if bar.contains(&ActionKind::BarrelRoll) {
                acts.push("4/5 Roll");
            }
            if bar.contains(&ActionKind::TargetLock) {
                acts.push("6 Lock");
            }
            if bar.contains(&ActionKind::Boost) {
                acts.push("7/8/9 Boost");
            }
            let (reveal, mines) = online
                .sel
                .and_then(|id| snap.ships.iter().find(|v| v.id.0 == id))
                .map(|v| bomb_cards(&game, v))
                .unwrap_or_default();
            if !mines.is_empty() {
                acts.push("M Drop mine");
            }
            if snap.mission.as_ref().is_some_and(|m| {
                m.kind == MissionKind::PoliticalEscort && m.rebel_side == online.team
            }) {
                acts.push("P Protect shuttle");
            }
            let extras = online
                .sel
                .and_then(|id| snap.ships.iter().find(|v| v.id.0 == id))
                .map(|v| v.extras.clone())
                .unwrap_or_default();
            if extras.turn_boost {
                acts.push("-/= Turn boost");
            }
            if extras.far_roll {
                acts.push("[/] Far roll");
            }
            if !extras.card_actions.is_empty() {
                acts.push("K Card action");
            }
            if !extras.toggles.is_empty() {
                acts.push("U Use card");
            }
            let second = match extras.second {
                Some(k) => format!(" • 0 then a key: 2nd action ({})", second_action_label(k)),
                None => String::new(),
            };
            let bomb = if reveal.is_empty() { "" } else { " • B: bomb on reveal" };
            format!(
                "Tab: ship • Left/Right+Enter: maneuver • actions: {}{second}{bomb} • C: commit • X: resign",
                acts.join(" ")
            )
        }
        Phase::Combat => "combat resolving…".to_string(),
        Phase::GameOver => "game over".to_string(),
    };
    lines.push(help);
    if let Some((_, buf)) = &online.rename {
        lines.push(format!("CALLSIGN: {buf}_   (Enter confirms, Esc cancels)"));
    }
    if let Some(a) = &online.anim {
        let name = |id: u32| callsign(Some(snap), id);
        match &a.current {
            Some(AnimItem::Move(mv)) => {
                let result = match mv.action_result {
                    ActionResult::Performed => "".into(),
                    ActionResult::SkippedStressed => " (skipped: stressed)".to_string(),
                    ActionResult::SkippedBumped => " (skipped: bumped)".to_string(),
                    ActionResult::SkippedDamaged => " (sensors damaged)".to_string(),
                    ActionResult::Failed => " (failed)".to_string(),
                    ActionResult::SkippedNetted => " (netted: no action)".to_string(),
                    ActionResult::SkippedObstacle => " (hit an asteroid: no action)".to_string(),
                };
                let outcome = |r: ActionResult| match r {
                    ActionResult::Performed => "",
                    ActionResult::Failed => " (failed)",
                    _ => " (skipped)",
                };
                let pre = mv
                    .pre
                    .map(|(a, r)| {
                        format!(
                            "{}{} before moving, ",
                            action_name(&game, Some(snap), a),
                            outcome(r)
                        )
                    })
                    .unwrap_or_default();
                let second = mv
                    .second
                    .map(|(a, r)| {
                        format!(", then {}{}", action_name(&game, Some(snap), a), outcome(r))
                    })
                    .unwrap_or_default();
                let rocks = if mv.obstacles_hit.is_empty() { "" } else { " — OBSTACLE!" };
                lines.push(format!(
                    "{} {pre}flies {} {} — action: {}{result}{second}{rocks}",
                    name(mv.ship.0),
                    render::steer_name(mv.maneuver.steer),
                    mv.maneuver.distance,
                    action_name(&game, Some(snap), mv.action),
                ));
            }
            Some(AnimItem::Attack { line, .. }) => lines.push(line.clone()),
            Some(AnimItem::Detonation(d)) => lines.push(detonation_line(snap, d)),
            Some(AnimItem::Pull(p)) => lines.push(if p.swallowed {
                format!("{} is dragged into the black hole — SWALLOWED", name(p.ship.0))
            } else {
                format!("{} is pulled one unit toward the black hole", name(p.ship.0))
            }),
            Some(AnimItem::Prompt { attacker, options, unavailable }) => {
                let opts: Vec<String> = options
                    .iter()
                    .enumerate()
                    .map(|(n, o)| {
                        format!(
                            "{}) {} -> {} R{}{}",
                            n + 1,
                            weapon_name(&game, o.weapon),
                            name(o.target),
                            o.range,
                            if o.obstructed { " (obstructed)" } else { "" }
                        )
                    })
                    .collect();
                lines.push(format!(
                    "DECLARE TARGET for {}: click a highlighted ship or press  {}",
                    name(*attacker),
                    opts.join("   ")
                ));
                if !unavailable.is_empty() {
                    let why: Vec<String> =
                        unavailable.iter().map(|(w, r)| format!("{w} ({r})")).collect();
                    lines.push(format!("   not available now: {}", why.join(", ")));
                }
            }
            _ => {}
        }
        if let Some(w) = online.waiting_on
            && a.current.is_none()
        {
            lines.push(format!("Opponent is declaring a target for their {}…", name(w)));
        }
    }
    if online.anim.is_none() && !online.combat_log.is_empty() {
        lines.push("— last combat —".into());
        for l in online.combat_log.iter().take(4) {
            lines.push(l.clone());
        }
    }
    if !online.status.is_empty() {
        lines.push(online.status.clone());
    }
    text.0 = lines.join("\n");
}
