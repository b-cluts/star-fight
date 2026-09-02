//! Connected play against the server: placement, secret planning, and
//! animated turn resolution driven by server messages. The client mirrors
//! what the server tells it and never mutates game state itself.

use bevy::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::f64::consts::FRAC_PI_2;

use sf_core::action::{ActionKind, ActionResult, BoostDir, PlannedAction, Side};
use sf_core::board::Seat;
use sf_core::game::{AttackRecord, MoveRecord, Phase, ShipView};
use sf_core::geometry::{Pose, Vec2 as GVec2};
use sf_core::maneuver::{self, Difficulty};
use sf_core::rules;
use sf_core::ship::ShipId;
use sf_proto::messages::{ClientMsg, ServerMsg};

use crate::Screen;
use crate::net::{NetEvent, NetHandle};
use crate::pins;
use crate::render::{self, ClassArt, CursorUnits, Game, Ghost, HudText, ShowArcs};

/// Path samples flown per second during resolution animation
/// (samples are 0.1 units apart → 4 units/second).
const ANIM_SAMPLES_PER_SEC: f32 = 40.0;

/// Seconds per attack in the combat animation.
const ATTACK_DUR: f32 = 1.1;
/// Fraction of an attack spent in bolt flight (the rest: impact / fade).
const FLY_FRAC: f32 = 0.55;

pub struct Snap {
    pub phase: Phase,
    pub turn: u32,
    pub ships: Vec<ShipView>,
    pub committed: [bool; 2],
    pub initiative: u8,
    pub totals: [u32; 2],
}

/// One step of the turn playback queue, fed by server messages as the
/// combat streams in (attacks can arrive while moves still animate).
#[derive(Clone)]
pub enum AnimItem {
    Move(MoveRecord),
    Attack {
        rec: AttackRecord,
        line: String,
    },
    /// The server asks us to declare a target for `attacker`.
    Prompt {
        attacker: u32,
        candidates: Vec<(u32, u8)>,
    },
    /// The opponent is declaring a target for their `attacker`.
    Waiting {
        attacker: u32,
    },
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
}

impl Anim {
    fn new() -> Self {
        Self {
            queue: VecDeque::new(),
            current: None,
            t: 0.0,
            end_poses: HashMap::new(),
            attack_no: 0,
        }
    }

    fn push(&mut self, item: AnimItem) {
        if let AnimItem::Move(m) = &item {
            self.end_poses.insert(m.ship.0, m.end);
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
    pub opponent: String,
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
    /// Human-readable summary of last turn's combat, shown in the HUD.
    pub combat_log: Vec<String>,
    /// Active Declare Target prompt: (attacker, candidates) awaiting input.
    pub prompt: Option<(u32, Vec<(u32, u8)>)>,
    /// Opponent's ship currently declaring a target (for the HUD).
    pub waiting_on: Option<u32>,
    /// Placement: callsign being typed for (ship, buffer).
    pub rename: Option<(u32, String)>,
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
            poll_net,
            sync_ships,
            animate,
            rename_input,
            placement_input,
            planning_input,
            target_input,
            leave_keys,
            draw,
            hud,
        )
            .chain()
            .run_if(in_state(Screen::Online)),
    );
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
            NetEvent::Msg(msg) => match msg {
                ServerMsg::Welcome { .. } | ServerMsg::Pong => {}
                ServerMsg::GameCreated { code } => {
                    online.status = format!("Game code: {code}  —  waiting for opponent…");
                    online.code = Some(code);
                }
                ServerMsg::GameStart { seat, opponent, board } => {
                    game.board = board;
                    online.seat = Some(seat);
                    online.status = format!("Matched with {opponent} — place your ships");
                    online.opponent = opponent;
                }
                ServerMsg::Snapshot { phase, turn, ships, committed, initiative, squad_totals } => {
                    if phase != Phase::Placement {
                        online.overrides.clear();
                        online.drag = None;
                    }
                    if online.sel.is_none() {
                        let seat = online.my_seat();
                        online.sel =
                            ships.iter().find(|s| s.owner.0 == seat as u32).map(|s| s.id.0);
                    }
                    let snap =
                        Snap { phase, turn, ships, committed, initiative, totals: squad_totals };
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
                ServerMsg::MovementResult { moves, events } => {
                    online.status.clear();
                    online.waiting_on = None;
                    online.combat_log = events;
                    let anim = online.anim.get_or_insert_with(Anim::new);
                    for m in moves {
                        anim.push(AnimItem::Move(m));
                    }
                }
                ServerMsg::AttackResult { attack, events } => {
                    let line = attack_line(&online, &game, &attack);
                    online.combat_log.push(line.clone());
                    online.combat_log.extend(events);
                    online.waiting_on = None;
                    online
                        .anim
                        .get_or_insert_with(Anim::new)
                        .push(AnimItem::Attack { rec: attack, line });
                }
                ServerMsg::ChooseTarget { attacker, candidates } => {
                    let candidates = candidates.into_iter().map(|(id, r)| (id.0, r)).collect();
                    online
                        .anim
                        .get_or_insert_with(Anim::new)
                        .push(AnimItem::Prompt { attacker: attacker.0, candidates });
                }
                ServerMsg::OpponentChoosing { attacker } => {
                    online
                        .anim
                        .get_or_insert_with(Anim::new)
                        .push(AnimItem::Waiting { attacker: attacker.0 });
                }
                ServerMsg::TurnEnd { events } => {
                    online.combat_log.extend(events);
                    online.anim.get_or_insert_with(Anim::new).push(AnimItem::TurnEnd);
                }
                ServerMsg::GameOver { winner, reason } => {
                    let text = match (winner, online.seat) {
                        (Some(w), Some(s)) if w == s => format!("VICTORY — {reason}"),
                        (Some(_), _) => format!("DEFEAT — {reason}"),
                        (None, _) => format!("DRAW — {reason}"),
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
                if online.over.is_none() {
                    online.over = Some(format!("Connection closed: {e}"));
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
    let (y, heading) =
        if seat == 0 { (1.5, FRAC_PI_2) } else { (game.board.height - 1.5, -FRAC_PI_2) };
    let mut seeds = Vec::new();
    let mut x = 5.0;
    for view in snap.ships.iter().filter(|s| s.owner.0 == seat as u32) {
        if view.pose.is_none() && !online.overrides.contains_key(&view.id.0) {
            seeds.push((view.id.0, Pose::new(x, y, heading)));
        }
        x += 4.0;
    }
    for (id, pose) in seeds {
        online.overrides.insert(id, pose);
    }
}

/// One-line narration of an attack for the combat log / HUD.
fn attack_line(online: &Online, _game: &Game, a: &AttackRecord) -> String {
    let name = |id: u32| callsign(online.snap.as_ref(), id);
    let landed = a.hits + a.crits;
    let mut line = format!("{} → {} @R{}: ", name(a.attacker.0), name(a.defender.0), a.range);
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
    let Some((_, candidates)) = online.prompt.clone() else {
        return;
    };
    let mut choice: Option<u32> = None;
    let digits = [
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
    ];
    for (n, key) in digits.iter().enumerate() {
        if keys.just_pressed(*key)
            && let Some((id, _)) = candidates.get(n)
        {
            choice = Some(*id);
        }
    }
    if choice.is_none()
        && buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
        && let (Some(a), Some(snap)) = (&online.anim, &online.snap)
    {
        for (id, _) in &candidates {
            let Some(p) = a.end_pose(*id, snap) else {
                continue;
            };
            let Some(view) = snap.ships.iter().find(|v| v.id.0 == *id) else {
                continue;
            };
            let fp = game.ships.classes[game.class_index(view.class)].footprint;
            if rules::point_in_footprint(p, fp, cur) {
                choice = Some(*id);
                break;
            }
        }
    }
    if let Some(target) = choice {
        online.send(ClientMsg::DeclareTarget { target: ShipId(target) });
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
                    a.attack_no += 1;
                    a.current = None;
                    continue;
                }
                return;
            }
            AnimItem::Prompt { attacker, candidates } => {
                // Holds here until target_input answers and clears `current`.
                if prompt.is_none() {
                    *prompt = Some((attacker, candidates));
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
    let bar = {
        let Some(snap) = &online.snap else { return };
        let Some(view) = snap.ships.iter().find(|v| v.id.0 == selected) else {
            return;
        };
        game.ships.classes[game.class_index(view.class)].action_bar.clone()
    };
    let plan_action = |online: &mut Online, action: PlannedAction| {
        online.lock_pick = false;
        online.send(ClientMsg::PlanAction { ship_id: ShipId(selected), action });
    };
    if keys.just_pressed(KeyCode::Digit1) {
        plan_action(&mut online, PlannedAction::Pass);
    }
    if keys.just_pressed(KeyCode::Digit2) && bar.contains(&ActionKind::Focus) {
        plan_action(&mut online, PlannedAction::Focus);
    }
    if keys.just_pressed(KeyCode::Digit3) && bar.contains(&ActionKind::Evade) {
        plan_action(&mut online, PlannedAction::Evade);
    }
    if keys.just_pressed(KeyCode::Digit4) && bar.contains(&ActionKind::BarrelRoll) {
        plan_action(&mut online, PlannedAction::BarrelRoll(Side::Left));
    }
    if keys.just_pressed(KeyCode::Digit5) && bar.contains(&ActionKind::BarrelRoll) {
        plan_action(&mut online, PlannedAction::BarrelRoll(Side::Right));
    }
    if keys.just_pressed(KeyCode::Digit6) && bar.contains(&ActionKind::TargetLock) {
        online.lock_pick = true;
        online.status = "Target lock: click an enemy ship".into();
    }
    if bar.contains(&ActionKind::Boost) {
        if keys.just_pressed(KeyCode::Digit7) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::Straight));
        }
        if keys.just_pressed(KeyCode::Digit8) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::BankLeft));
        }
        if keys.just_pressed(KeyCode::Digit9) {
            plan_action(&mut online, PlannedAction::Boost(BoostDir::BankRight));
        }
    }
    if online.lock_pick
        && buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
    {
        let target = online.snap.as_ref().and_then(|snap| {
            snap.ships
                .iter()
                .filter(|v| v.owner.0 != seat as u32 && !v.destroyed)
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
    let start = game.to_world(atk_pose.anchor);
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
    if hit {
        // Bolt flight until impact.
        if p < FLY_FRAC {
            let head = start + (aim - start) * (p / FLY_FRAC);
            draw_volley(gizmos, start, head, dir, nbolts, bolt_color, 1.0);
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
            gizmos.circle_2d(target, 6.0 + q * 26.0, flash);
            gizmos.circle_2d(target, 3.0 + q * 14.0, Color::srgba(1.0, 1.0, 0.9, alpha));
            if rec.defender_destroyed {
                gizmos.circle_2d(target, 10.0 + q * 55.0, Color::srgba(1.0, 0.45, 0.1, alpha));
            }
        }
    } else {
        // Fly past the target and fade out on the way.
        let beyond = aim + dir * 3.0 * render::PX;
        let head = start + (beyond - start) * p;
        let fade = if p > 0.55 { 1.0 - (p - 0.55) / 0.45 } else { 1.0 };
        draw_volley(gizmos, start, head, dir, nbolts, bolt_color, fade);
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

fn action_name(snap: Option<&Snap>, a: PlannedAction) -> String {
    match a {
        PlannedAction::Pass => "Pass".into(),
        PlannedAction::Focus => "Focus".into(),
        PlannedAction::Evade => "Evade".into(),
        PlannedAction::BarrelRoll(Side::Left) => "Barrel Roll L".into(),
        PlannedAction::BarrelRoll(Side::Right) => "Barrel Roll R".into(),
        PlannedAction::Boost(BoostDir::Straight) => "Boost".into(),
        PlannedAction::Boost(BoostDir::BankLeft) => "Boost L".into(),
        PlannedAction::Boost(BoostDir::BankRight) => "Boost R".into(),
        PlannedAction::TargetLock(id) => format!("Lock {}", callsign(snap, id.0)),
    }
}

fn leave_keys(
    online: Res<Online>,
    keys: Res<ButtonInput<KeyCode>>,
    mut next: ResMut<NextState<Screen>>,
) {
    if keys.just_pressed(KeyCode::Escape) && online.over.is_some() {
        next.set(Screen::Menu);
    }
}

fn draw(
    mut gizmos: Gizmos,
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
    render::draw_board(&mut gizmos, &game);
    let Ok((mut gsprite, mut gtf, mut gvis)) = ghost.single_mut() else {
        return;
    };
    *gvis = Visibility::Hidden;
    let Some(snap) = &online.snap else { return };
    let seat = online.my_seat();
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
        let selected = own && online.sel == Some(view.id.0);
        let mut color = match (own, selected) {
            (_, true) => Color::srgb(1.0, 0.9, 0.3),
            (true, _) => Color::srgba(0.3, 0.9, 0.4, 0.7),
            (false, _) => Color::srgba(0.9, 0.4, 0.3, 0.7),
        };
        // Placement legality tint for own provisional poses.
        if own && snap.phase == Phase::Placement {
            let zone_seat = if seat == 0 { Seat::South } else { Seat::North };
            if rules::placement_legal(&game.board, zone_seat, pose, class.footprint, &[]).is_err() {
                color = Color::srgb(1.0, 0.35, 0.35);
            }
        }
        render::draw_base(&mut gizmos, &game, pose, class.footprint, color);
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
            Some(AnimItem::Prompt { attacker, candidates }) => {
                if let (Some(ap), Some(fp)) = (a.end_pose(*attacker, snap), fp_of(*attacker)) {
                    render::draw_firing_arc(&mut gizmos, &game, ap, fp, 0.6);
                    bullseye.0 = Some(ap);
                }
                let hi = Color::srgb(1.0, 0.95, 0.2);
                for (id, _) in candidates {
                    if let (Some(p), Some(fp)) = (a.end_pose(*id, snap), fp_of(*id)) {
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
        render::draw_firing_arc(&mut gizmos, &game, end, class.footprint, 0.7);
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
    let init = if snap.initiative == seat { "you" } else { "opponent" };
    let committed = format!(
        "committed: you {} / opp {}",
        if snap.committed[seat as usize] { "✔" } else { "—" },
        if snap.committed[1 - seat as usize] { "✔" } else { "—" },
    );
    let mut lines = vec![format!(
        "TURN {} | {:?} | initiative: {init} ({} vs {} pts) | {committed}",
        snap.turn,
        snap.phase,
        snap.totals[seat as usize],
        snap.totals[1 - seat as usize],
    )];
    if let Some(view) = online.sel.and_then(|id| snap.ships.iter().find(|v| v.id.0 == id)) {
        let class = &game.ships.classes[game.class_index(view.class)];
        let mut line = format!(
            "{} ({}, {} PS{}) — hull {}/{} shields {}/{} stress {} focus {} evade {}",
            view.callsign,
            class.name,
            view.pilot,
            view.skill,
            view.hull,
            class.hull,
            view.shields,
            class.shields,
            view.stress,
            view.focus,
            view.evade
        );
        if let Some(l) = view.lock {
            line.push_str(&format!(" lock {}", callsign(Some(snap), l.0)));
        }
        if !view.crits.is_empty() {
            let names: Vec<&str> = view.crits.iter().map(|c| c.name()).collect();
            line.push_str(&format!(" | crits: {}", names.join(", ")));
        }
        if let Some(a) = view.planned_action {
            line.push_str(&format!(" | action: {}", action_name(Some(snap), a)));
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
    }
    let help = match snap.phase {
        Phase::Placement => {
            "drag ships • Q/E or scroll rotates • N: rename • A: submit all • +/- zoom, right-drag pan, Home reset"
        }
        Phase::Planning => {
            "Tab: ship • ←/→+Enter: maneuver • actions: 1 Pass 2 Focus 3 Evade 4/5 Roll 6 Lock 7/8/9 Boost • C: commit • X: resign"
        }
        Phase::Combat => "combat resolving…",
        Phase::GameOver => "game over",
    };
    lines.push(help.into());
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
                };
                lines.push(format!(
                    "{} flies {} {} — action: {}{result}",
                    name(mv.ship.0),
                    render::steer_name(mv.maneuver.steer),
                    mv.maneuver.distance,
                    action_name(Some(snap), mv.action),
                ));
            }
            Some(AnimItem::Attack { line, .. }) => lines.push(line.clone()),
            Some(AnimItem::Prompt { attacker, candidates }) => {
                let opts: Vec<String> = candidates
                    .iter()
                    .enumerate()
                    .map(|(n, (id, r))| format!("{}) {} R{}", n + 1, name(*id), r))
                    .collect();
                lines.push(format!(
                    "DECLARE TARGET for {}: click a highlighted ship or press  {}",
                    name(*attacker),
                    opts.join("   ")
                ));
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
