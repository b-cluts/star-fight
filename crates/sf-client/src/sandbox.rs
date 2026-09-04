//! Offline sandbox: free placement and dial-flying against sf-core, no
//! server. Deliberately looser than the real game (P toggles modes at
//! will) — it's a development playground.

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use std::f64::consts::FRAC_PI_2;

use sf_core::board::Seat;
use sf_core::geometry::Pose;
use sf_core::maneuver;
use sf_core::rules::{self, PathObstruction};
use sf_core::ship::ShipId;

use crate::Screen;
use crate::render::{self, ClassArt, CursorUnits, Game, Ghost, HudText, Sel, ShowArcs};

#[derive(Resource, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Placement,
    Flight,
}

#[derive(Component)]
struct SandboxTag;

#[derive(Component)]
struct ShipEnt {
    class_idx: usize,
    seat: Seat,
    pose: Pose,
    id: ShipId,
    callsign: String,
}

pub fn plugin(app: &mut App) {
    app.insert_resource(Mode::Placement)
        .add_systems(OnEnter(Screen::Sandbox), enter_sandbox)
        .add_systems(OnExit(Screen::Sandbox), exit_sandbox)
        .add_systems(
            Update,
            (
                toggle_mode,
                placement_input,
                flight_input,
                sync_ship_transforms,
                draw_overlays,
                update_hud,
            )
                .chain()
                .run_if(in_state(Screen::Sandbox)),
        );
}

fn enter_sandbox(
    mut commands: Commands,
    game: Res<Game>,
    art: Res<ClassArt>,
    mut sel: ResMut<Sel>,
    mut mode: ResMut<Mode>,
) {
    *sel = Sel::default();
    *mode = Mode::Placement;
    let fleet = [
        (0, Seat::South, Pose::new(8.0, 2.0, FRAC_PI_2)),
        (8, Seat::South, Pose::new(12.0, 2.0, FRAC_PI_2)), // TIE Interceptor (class index 8)
        (5, Seat::North, Pose::new(8.0, 18.0, -FRAC_PI_2)), // YT-1300 (class index 5)
        (3, Seat::North, Pose::new(12.0, 18.0, -FRAC_PI_2)), // Y-Wing (class index 3)
    ];
    let squads = sf_core::ship::squad_names(&[
        game.ships.classes[fleet[0].0].faction,
        game.ships.classes[fleet[2].0].faction,
    ]);
    for (n, (class_idx, seat, pose)) in fleet.into_iter().enumerate() {
        let class = &game.ships.classes[class_idx];
        let (size, transform) = render::ship_visual(class, pose, &game, 1.0);
        let callsign = sf_core::ship::default_callsign(squads[n / 2], n % 2);
        commands.spawn((
            Sprite { image: art.0[class_idx].clone(), custom_size: Some(size), ..default() },
            transform,
            ShipEnt { class_idx, seat, pose, id: ShipId(n as u32), callsign },
            SandboxTag,
        ));
    }
}

fn exit_sandbox(
    mut commands: Commands,
    tagged: Query<Entity, With<SandboxTag>>,
    mut ghost: Query<&mut Visibility, With<Ghost>>,
) {
    for e in &tagged {
        commands.entity(e).despawn();
    }
    if let Ok(mut vis) = ghost.single_mut() {
        *vis = Visibility::Hidden;
    }
}

fn toggle_mode(
    keys: Res<ButtonInput<KeyCode>>,
    mut mode: ResMut<Mode>,
    mut sel: ResMut<Sel>,
    mut next: ResMut<NextState<Screen>>,
) {
    if keys.just_pressed(KeyCode::KeyP) {
        *mode = match *mode {
            Mode::Placement => Mode::Flight,
            Mode::Flight => Mode::Placement,
        };
        sel.drag = None;
    }
    if keys.just_pressed(KeyCode::Escape) {
        next.set(Screen::Menu);
    }
}

fn placement_input(
    mode: Res<Mode>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut wheel: EventReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    cursor: Res<CursorUnits>,
    game: Res<Game>,
    mut sel: ResMut<Sel>,
    mut ships: Query<(Entity, &mut ShipEnt)>,
) {
    if *mode != Mode::Placement {
        wheel.clear();
        return;
    }
    if buttons.just_pressed(MouseButton::Left)
        && let Some(cur) = cursor.0
    {
        for (entity, ship) in &ships {
            let fp = game.ships.classes[ship.class_idx].footprint;
            if rules::point_in_footprint(ship.pose, fp, cur) {
                let off = ship.pose.anchor - cur;
                sel.drag = Some((entity, off));
                sel.ship = Some(entity);
                break;
            }
        }
    }
    if buttons.just_released(MouseButton::Left) {
        sel.drag = None;
    }
    if let Some((entity, off)) = sel.drag
        && let Ok((_, mut ship)) = ships.get_mut(entity)
        && let Some(cur) = cursor.0
    {
        ship.pose.anchor = cur + off;
    }
    // Rotation: scroll wheel or Q/E, on the dragged ship else the hovered one.
    let scroll: f32 = wheel.read().map(|e| e.y).sum();
    let mut steps = if scroll > 0.0 {
        1.0
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
        let target = sel.drag.map(|(e, _)| e).or_else(|| {
            let cur = cursor.0?;
            ships.iter().find_map(|(e, s)| {
                let fp = game.ships.classes[s.class_idx].footprint;
                rules::point_in_footprint(s.pose, fp, cur).then_some(e)
            })
        });
        if let Some(entity) = target
            && let Ok((_, mut ship)) = ships.get_mut(entity)
        {
            ship.pose.heading += steps * std::f64::consts::PI / 12.0;
        }
    }
}

fn flight_input(
    mode: Res<Mode>,
    keys: Res<ButtonInput<KeyCode>>,
    game: Res<Game>,
    mut sel: ResMut<Sel>,
    mut ships: Query<(Entity, &mut ShipEnt)>,
) {
    if *mode != Mode::Flight {
        return;
    }
    let mut order: Vec<Entity> = ships.iter().map(|(e, _)| e).collect();
    order.sort();
    if order.is_empty() {
        return;
    }
    let current = sel.ship.filter(|e| order.contains(e)).unwrap_or(order[0]);
    let mut selected = current;
    if keys.just_pressed(KeyCode::Tab) {
        let i = order.iter().position(|&e| e == current).unwrap_or(0);
        selected = order[(i + 1) % order.len()];
        sel.dial_idx = 0;
    }
    sel.ship = Some(selected);

    let Ok((_, mut ship)) = ships.get_mut(selected) else { return };
    let dial = game.dial(&game.ships.classes[ship.class_idx]);
    if dial.is_empty() {
        return;
    }
    if keys.just_pressed(KeyCode::ArrowRight) {
        sel.dial_idx = (sel.dial_idx + 1) % dial.len();
    }
    if keys.just_pressed(KeyCode::ArrowLeft) {
        sel.dial_idx = (sel.dial_idx + dial.len() - 1) % dial.len();
    }
    if keys.just_pressed(KeyCode::Enter)
        && let Ok(end) = maneuver::apply(ship.pose, dial[sel.dial_idx])
    {
        ship.pose = end;
    }
}

fn sync_ship_transforms(
    game: Res<Game>,
    mode: Res<Mode>,
    mut ships: Query<(&ShipEnt, &mut Transform, &mut Sprite), Without<Ghost>>,
) {
    for (ship, mut transform, mut sprite) in &mut ships {
        let class = &game.ships.classes[ship.class_idx];
        let (size, tf) = render::ship_visual(class, ship.pose, &game, 1.0);
        *transform = tf;
        sprite.custom_size = Some(size);
        let legal =
            rules::placement_legal(&game.board, ship.seat, ship.pose, class.footprint, &[]).is_ok();
        sprite.color = if *mode == Mode::Placement && !legal {
            Color::srgb(1.0, 0.4, 0.4)
        } else {
            Color::WHITE
        };
    }
}

fn draw_overlays(
    mut gizmos: Gizmos,
    game: Res<Game>,
    mode: Res<Mode>,
    sel: Res<Sel>,
    arcs: Res<ShowArcs>,
    art: Res<ClassArt>,
    ships: Query<(Entity, &ShipEnt)>,
    mut ghost: Query<(&mut Sprite, &mut Transform, &mut Visibility), With<Ghost>>,
    mut bullseye: ResMut<render::BullseyePreview>,
    cursor: Res<CursorUnits>,
    mut hover: ResMut<render::Hover>,
) {
    bullseye.0 = None;
    render::draw_board(&mut gizmos, &game);
    hover.0 = render::hovered(
        cursor.0,
        ships.iter().map(|(_, s)| (s.callsign.as_str(), &game.ships.classes[s.class_idx], s.pose)),
    );
    for (entity, ship) in &ships {
        let class = &game.ships.classes[ship.class_idx];
        let color = match (ship.seat, sel.ship == Some(entity)) {
            (_, true) => Color::srgb(1.0, 0.9, 0.3),
            (Seat::South, _) => Color::srgba(0.3, 0.9, 0.4, 0.6),
            (Seat::North, _) => Color::srgba(0.9, 0.4, 0.3, 0.6),
        };
        render::draw_base(&mut gizmos, &game, ship.pose, class.footprint, color);
    }

    let Ok((mut gsprite, mut gtf, mut gvis)) = ghost.single_mut() else { return };
    if *mode != Mode::Flight {
        *gvis = Visibility::Hidden;
        return;
    }
    let Some((_, ship)) = sel.ship.and_then(|e| ships.get(e).ok()) else {
        *gvis = Visibility::Hidden;
        return;
    };
    let class = &game.ships.classes[ship.class_idx];
    let dial = game.dial(class);
    if dial.is_empty() {
        *gvis = Visibility::Hidden;
        return;
    }
    let man = dial[sel.dial_idx.min(dial.len() - 1)];
    let Ok(path) = maneuver::sample_path(ship.pose, man) else { return };
    let end = *path.last().unwrap();
    let color = render::difficulty_color(man.difficulty);
    render::draw_path(&mut gizmos, &game, &path, color);

    let others: Vec<_> = ships
        .iter()
        .filter(|(e, _)| Some(*e) != sel.ship)
        .map(|(_, s)| (s.id, s.pose, game.ships.classes[s.class_idx].footprint))
        .collect();
    let obstruction = rules::check_path(&game.board, &path, class.footprint, &others);
    let (size, tf) = render::ship_visual(class, end, &game, 2.0);
    gsprite.image = art.0[ship.class_idx].clone();
    gsprite.custom_size = Some(size);
    gsprite.color = if obstruction.is_some() {
        Color::srgba(1.0, 0.35, 0.35, 0.4)
    } else {
        Color::srgba(1.0, 1.0, 1.0, 0.35)
    };
    *gtf = tf;
    *gvis = Visibility::Visible;

    render::draw_base(&mut gizmos, &game, end, class.footprint, color);
    render::draw_heading_arrow(&mut gizmos, &game, end, color);
    if arcs.0 {
        render::draw_firing_arc(&mut gizmos, &game, end, class.footprint, 0.7);
        bullseye.0 = Some(end);
    }
}

fn update_hud(
    game: Res<Game>,
    mode: Res<Mode>,
    sel: Res<Sel>,
    ships: Query<(Entity, &ShipEnt)>,
    mut hud: Query<&mut Text, With<HudText>>,
) {
    let Ok(mut text) = hud.single_mut() else { return };
    text.0 = match *mode {
        Mode::Placement => {
            "SANDBOX PLACEMENT — drag ships; scroll or Q/E rotates; P: flight; +/- zoom, right-drag pan; Esc: menu".into()
        }
        Mode::Flight => {
            let Some((_, ship)) = sel.ship.and_then(|e| ships.get(e).ok()) else {
                return;
            };
            let class = &game.ships.classes[ship.class_idx];
            let dial = game.dial(class);
            if dial.is_empty() {
                return;
            }
            let idx = sel.dial_idx.min(dial.len() - 1);
            let man = dial[idx];
            let others: Vec<_> = ships
                .iter()
                .filter(|(e, _)| Some(*e) != sel.ship)
                .map(|(_, s)| (s.id, s.pose, game.ships.classes[s.class_idx].footprint))
                .collect();
            let status = maneuver::sample_path(ship.pose, man)
                .ok()
                .and_then(|path| rules::check_path(&game.board, &path, class.footprint, &others))
                .map(|o| match o {
                    PathObstruction::OffBoard { .. } => "  !! LEAVES BOARD".to_string(),
                    PathObstruction::ShipCollision { ship, .. } => {
                        format!("  !! COLLIDES with ship {}", ship.0)
                    }
                })
                .unwrap_or_default();
            format!(
                "SANDBOX FLIGHT — {} | [{}/{}] {} {} ({}){status}\nTab: ship  Left/Right: dial  Enter: fly  F: arcs  P: placement  Esc: menu",
                class.name,
                idx + 1,
                dial.len(),
                render::steer_name(man.steer),
                man.distance,
                render::difficulty_label(man.difficulty),
            )
        }
    };
}
