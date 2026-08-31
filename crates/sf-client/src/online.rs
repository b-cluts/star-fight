//! Connected play against the server: placement, secret planning, and
//! animated turn resolution driven by server messages. The client mirrors
//! what the server tells it and never mutates game state itself.

use bevy::prelude::*;
use std::collections::HashMap;
use std::f64::consts::FRAC_PI_2;

use sf_core::board::Seat;
use sf_core::game::{MoveRecord, Phase, ShipView};
use sf_core::geometry::{Pose, Vec2 as GVec2};
use sf_core::maneuver::{self, Difficulty};
use sf_core::rules;
use sf_core::ship::ShipId;
use sf_proto::messages::{ClientMsg, ServerMsg};

use crate::net::{NetEvent, NetHandle};
use crate::render::{self, ClassArt, CursorUnits, Game, Ghost, HudText, ShowArcs};
use crate::Screen;

/// Path samples flown per second during resolution animation
/// (samples are 0.1 units apart → 4 units/second).
const ANIM_SAMPLES_PER_SEC: f32 = 40.0;

pub struct Snap {
    pub phase: Phase,
    pub turn: u32,
    pub ships: Vec<ShipView>,
    pub committed: [bool; 2],
    pub initiative: u8,
    pub totals: [u32; 2],
}

pub struct Anim {
    pub moves: Vec<MoveRecord>,
    pub idx: usize,
    pub t: f32,
}

#[derive(Resource, Default)]
pub struct Online {
    pub net: Option<NetHandle>,
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
    app.init_resource::<Online>()
        .add_systems(OnExit(Screen::Online), exit_online)
        .add_systems(
            Update,
            (poll_net, sync_ships, animate, placement_input, planning_input, leave_keys, draw, hud)
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
                ServerMsg::TurnResult { moves } => {
                    online.status.clear();
                    online.anim = Some(Anim { moves, idx: 0, t: 0.0 });
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
    let (y, heading) = if seat == 0 {
        (1.5, FRAC_PI_2)
    } else {
        (game.board.height - 1.5, -FRAC_PI_2)
    };
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

fn sync_ships(
    mut commands: Commands,
    online: Res<Online>,
    game: Res<Game>,
    art: Res<ClassArt>,
    mut ships_q: Query<(Entity, &OnlineShip, &mut Sprite, &mut Transform, &mut Visibility)>,
) {
    let Some(snap) = &online.snap else { return };
    let animating: Option<u32> = online
        .anim
        .as_ref()
        .and_then(|a| a.moves.get(a.idx))
        .map(|m| m.ship.0);
    let mut existing: HashMap<u32, Entity> = HashMap::new();
    for (e, ship, ..) in &ships_q {
        existing.insert(ship.0, e);
    }
    for view in &snap.ships {
        let class_idx = game.class_index(view.class);
        let class = &game.ships.classes[class_idx];
        match existing.get(&view.id.0) {
            Some(&e) => {
                let Ok((_, _, mut sprite, mut tf, mut vis)) = ships_q.get_mut(e) else { continue };
                if Some(view.id.0) == animating {
                    continue; // animate() owns this transform right now
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
    let Online { anim, snap, pending_snap, .. } = &mut *online;
    let Some(a) = anim else { return };
    let Some(snapshot) = snap else { return };
    loop {
        let Some(mv) = a.moves.get(a.idx) else {
            // Animation finished: adopt the post-turn snapshot.
            *anim = None;
            if let Some(s) = pending_snap.take() {
                *snap = Some(s);
            }
            return;
        };
        a.t += time.delta_secs() * ANIM_SAMPLES_PER_SEC;
        let k = a.t as usize;
        let Some(view) = snapshot.ships.iter().find(|s| s.id.0 == mv.ship.0) else {
            a.idx += 1;
            a.t = 0.0;
            continue;
        };
        let class = &game.ships.classes[game.class_index(view.class)];
        let pose = if k < mv.path.len() { mv.path[k] } else { mv.end };
        for (ship, mut sprite, mut tf, mut vis) in &mut ships_q {
            if ship.0 == mv.ship.0 {
                let (size, t) = render::ship_visual(class, pose, &game, 1.5);
                sprite.custom_size = Some(size);
                *tf = t;
                *vis = if k >= mv.path.len() && mv.destroyed {
                    Visibility::Hidden
                } else {
                    Visibility::Visible
                };
            }
        }
        if k >= mv.path.len() {
            a.idx += 1;
            a.t = 0.0;
            continue;
        }
        return;
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
    if online.phase() != Some(Phase::Placement) || online.anim.is_some() || online.over.is_some() {
        wheel.clear();
        return;
    }
    let seat = online.my_seat();
    let own_views: Vec<ShipView> = online
        .snap
        .as_ref()
        .map(|s| s.ships.iter().filter(|v| v.owner.0 == seat as u32).cloned().collect())
        .unwrap_or_default();

    if buttons.just_pressed(MouseButton::Left) && let Some(cur) = cursor.0 {
        for view in &own_views {
            let Some(pose) = online.effective_pose(view) else { continue };
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
            online
                .overrides
                .insert(id, Pose { anchor: cur + off, heading: base.heading });
        }
    }

    // Rotation on the dragged (else hovered) own ship.
    let scroll: f32 = wheel.read().map(|e| e.y).sum();
    let mut steps = if scroll > 0.0 { 1.0f64 } else if scroll < 0.0 { -1.0 } else { 0.0 };
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
        let Some(view) = snap.ships.iter().find(|v| v.id.0 == selected) else { return };
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
    if keys.just_pressed(KeyCode::KeyC) {
        online.send(ClientMsg::CommitPlans);
    }
    if keys.just_pressed(KeyCode::KeyX) {
        online.send(ClientMsg::Resign);
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
) {
    render::draw_board(&mut gizmos, &game);
    let Ok((mut gsprite, mut gtf, mut gvis)) = ghost.single_mut() else { return };
    *gvis = Visibility::Hidden;
    let Some(snap) = &online.snap else { return };
    let seat = online.my_seat();

    for view in &snap.ships {
        let Some(pose) = online.effective_pose(view) else { continue };
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
            if rules::placement_legal(&game.board, zone_seat, pose, class.footprint, &[]).is_err()
            {
                color = Color::srgb(1.0, 0.35, 0.35);
            }
        }
        render::draw_base(&mut gizmos, &game, pose, class.footprint, color);
        if own && view.plan.is_some() && snap.phase == Phase::Planning {
            gizmos.circle_2d(
                game.to_world(pose.anchor),
                4.0,
                Color::srgb(0.4, 1.0, 0.9),
            );
        }
    }

    // Animation path overlay.
    if let Some(a) = &online.anim {
        if let Some(mv) = a.moves.get(a.idx) {
            render::draw_path(
                &mut gizmos,
                &game,
                &mv.path,
                render::difficulty_color(mv.maneuver.difficulty),
            );
        }
        return;
    }

    // Ghost preview while planning.
    if snap.phase != Phase::Planning {
        return;
    }
    let Some(sel) = online.sel else { return };
    let Some(view) = snap.ships.iter().find(|v| v.id.0 == sel) else { return };
    let Some(pose) = view.pose else { return };
    let class_idx = game.class_index(view.class);
    let class = &game.ships.classes[class_idx];
    let dial = game.dial(class);
    if dial.is_empty() {
        return;
    }
    let man = dial[online.dial_idx.min(dial.len() - 1)];
    let Ok(path) = maneuver::sample_path(pose, man) else { return };
    let end = *path.last().unwrap();
    let color = render::difficulty_color(man.difficulty);
    render::draw_path(&mut gizmos, &game, &path, color);
    render::draw_base(&mut gizmos, &game, end, class.footprint, color);
    render::draw_heading_arrow(&mut gizmos, &game, end, color);
    if arcs.0 {
        render::draw_firing_arc(&mut gizmos, &game, end, class.footprint, 0.7);
    }
    let (size, tf) = render::ship_visual(class, end, &game, 2.0);
    gsprite.image = art.0[class_idx].clone();
    gsprite.custom_size = Some(size);
    gsprite.color = Color::srgba(1.0, 1.0, 1.0, 0.35);
    *gtf = tf;
    *gvis = Visibility::Visible;
}

fn hud(
    online: Res<Online>,
    game: Res<Game>,
    mut hud: Query<&mut Text, With<HudText>>,
) {
    let Ok(mut text) = hud.single_mut() else { return };
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
        snap.turn, snap.phase, snap.totals[seat as usize], snap.totals[1 - seat as usize],
    )];
    if let Some(view) = online.sel.and_then(|id| snap.ships.iter().find(|v| v.id.0 == id)) {
        let class = &game.ships.classes[game.class_index(view.class)];
        let mut line = format!(
            "{} — hull {}/{} shields {}/{} stress {}",
            class.name, view.hull, class.hull, view.shields, class.shields, view.stress
        );
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
        Phase::Placement => "drag ships • Q/E or scroll rotates • A: submit all positions",
        Phase::Planning => "Tab: ship • ←/→: dial • Enter: plan • C: commit • F: arcs • X: resign",
        Phase::GameOver => "game over",
    };
    lines.push(help.into());
    if !online.status.is_empty() {
        lines.push(online.status.clone());
    }
    text.0 = lines.join("\n");
}
