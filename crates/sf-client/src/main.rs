//! M2 offline sandbox: board, deployment zones, mouse placement, and
//! dial-driven maneuver previews — all against sf-core, no networking.
//!
//! Controls:
//!   P            toggle Placement / Flight mode
//!   (placement)  drag ships with the mouse; scroll wheel rotates
//!   (flight)     Tab selects next ship, Left/Right cycle its dial,
//!                Enter executes the previewed maneuver

use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use std::f32::consts::FRAC_PI_2 as FRAC_PI_2_F32;
use std::f64::consts::FRAC_PI_2;

use sf_core::board::{Board, Seat};
use sf_core::data::{ManeuverDb, ShipDb};
use sf_core::geometry::{Pose, Vec2 as GVec2};
use sf_core::maneuver::{self, Difficulty, Maneuver, Steer};
use sf_core::rules::{self, PathObstruction};
use sf_core::ship::{ShipClass, ShipId};

/// Screen pixels per game unit.
const PX: f32 = 30.0;

fn main() {
    let assets = format!("{}/../../assets", env!("CARGO_MANIFEST_DIR"));
    let ships = std::fs::read_to_string(format!("{assets}/data/ships.ron")).expect("ships.ron");
    let dials =
        std::fs::read_to_string(format!("{assets}/data/maneuvers.ron")).expect("maneuvers.ron");
    let game = Game {
        board: Board { width: 20.0, height: 20.0, deploy_depth: 3.0 },
        ships: ShipDb::from_ron(&ships).expect("parse ships.ron"),
        dials: ManeuverDb::from_ron(&dials).expect("parse maneuvers.ron"),
    };

    App::new()
        .add_plugins(
            DefaultPlugins
                .set(AssetPlugin { file_path: assets, ..default() })
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "Star Fight — M2 Sandbox".into(),
                        resolution: (760.0_f32, 700.0_f32).into(),
                        ..default()
                    }),
                    ..default()
                }),
        )
        .insert_resource(ClearColor(Color::srgb(0.02, 0.02, 0.05)))
        .insert_resource(game)
        .insert_resource(Mode::Placement)
        .insert_resource(Sel::default())
        .insert_resource(CursorUnits(None))
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                track_cursor,
                toggle_mode,
                placement_input,
                flight_input,
                sync_ship_transforms,
                draw_overlays,
                update_hud,
            )
                .chain(),
        )
        .run();
}

#[derive(Resource)]
struct Game {
    board: Board,
    ships: ShipDb,
    dials: ManeuverDb,
}

impl Game {
    fn dial(&self, class: &ShipClass) -> &[Maneuver] {
        self.dials.set(class.maneuver_set).map(|s| s.maneuvers.as_slice()).unwrap_or(&[])
    }

    /// Board units → world (board centered on the origin).
    fn to_world(&self, p: GVec2) -> Vec2 {
        Vec2::new(
            (p.x - self.board.width / 2.0) as f32 * PX,
            (p.y - self.board.height / 2.0) as f32 * PX,
        )
    }

    fn to_units(&self, w: Vec2) -> GVec2 {
        GVec2::new(
            (w.x / PX) as f64 + self.board.width / 2.0,
            (w.y / PX) as f64 + self.board.height / 2.0,
        )
    }
}

#[derive(Resource, PartialEq, Eq, Clone, Copy)]
enum Mode {
    Placement,
    Flight,
}

#[derive(Resource, Default)]
struct Sel {
    ship: Option<Entity>,
    dial_idx: usize,
    /// While dragging: (ship, anchor offset from cursor).
    drag: Option<(Entity, GVec2)>,
}

#[derive(Resource)]
struct CursorUnits(Option<GVec2>);

#[derive(Component)]
struct ShipEnt {
    class_idx: usize,
    seat: Seat,
    pose: Pose,
    id: ShipId,
}

#[derive(Component)]
struct Ghost;

#[derive(Component)]
struct HudText;

#[derive(Resource)]
struct ClassArt(Vec<Handle<Image>>);

/// Sprite size + transform so the artwork's front-center pixel lands on the
/// pose anchor. Sprites face up; heading 0 = +X, so rotate by heading - 90°.
fn ship_visual(class: &ShipClass, pose: Pose, game: &Game, z: f32) -> (Vec2, Transform) {
    let (iw, ih) = class.sprite_px;
    let (ax, ay) = class.anchor_px;
    let upp = class.footprint.length / ih as f64; // units per sprite pixel
    let size = Vec2::new((iw as f64 * upp) as f32 * PX, class.footprint.length as f32 * PX);
    // Sprite-center offset from the anchor, in the ship's local frame
    // (+X ahead, +Y port). Down-image is backward; left-image is port.
    let center_local = GVec2::new(
        (ay as f64 - ih as f64 / 2.0) * upp,
        (ax as f64 - iw as f64 / 2.0) * upp,
    );
    let world = game.to_world(pose.local_to_world(center_local));
    let transform = Transform::from_translation(world.extend(z))
        .with_rotation(Quat::from_rotation_z(pose.heading as f32 - FRAC_PI_2_F32));
    (size, transform)
}

fn setup(mut commands: Commands, assets: Res<AssetServer>, game: Res<Game>) {
    commands.spawn(Camera2d);

    // Board background and deployment zones.
    let bw = game.board.width as f32 * PX;
    let bh = game.board.height as f32 * PX;
    let dd = game.board.deploy_depth as f32 * PX;
    commands.spawn((
        Sprite::from_color(Color::srgb(0.05, 0.06, 0.12), Vec2::new(bw, bh)),
        Transform::from_xyz(0.0, 0.0, -10.0),
    ));
    for (y, color) in [
        (-bh / 2.0 + dd / 2.0, Color::srgba(0.3, 0.9, 0.4, 0.08)),
        (bh / 2.0 - dd / 2.0, Color::srgba(0.9, 0.4, 0.3, 0.08)),
    ] {
        commands.spawn((
            Sprite::from_color(color, Vec2::new(bw, dd)),
            Transform::from_xyz(0.0, y, -9.0),
        ));
    }

    // Ship art, one handle per class.
    let art: Vec<Handle<Image>> =
        game.ships.classes.iter().map(|c| assets.load(c.sprite.clone())).collect();

    // Sandbox fleet: two TIEs south, two X-Wings north.
    let fleet = [
        (0, Seat::South, Pose::new(8.0, 2.0, FRAC_PI_2)),
        (0, Seat::South, Pose::new(12.0, 2.0, FRAC_PI_2)),
        (1, Seat::North, Pose::new(8.0, 18.0, -FRAC_PI_2)),
        (1, Seat::North, Pose::new(12.0, 18.0, -FRAC_PI_2)),
    ];
    for (n, (class_idx, seat, pose)) in fleet.into_iter().enumerate() {
        let class = &game.ships.classes[class_idx];
        let (size, transform) = ship_visual(class, pose, &game, 1.0);
        commands.spawn((
            Sprite { image: art[class_idx].clone(), custom_size: Some(size), ..default() },
            transform,
            ShipEnt { class_idx, seat, pose, id: ShipId(n as u32) },
        ));
    }

    // Ghost-ship preview (hidden until flight mode).
    commands.spawn((
        Sprite {
            image: art[0].clone(),
            color: Color::srgba(1.0, 1.0, 1.0, 0.35),
            ..default()
        },
        Transform::default(),
        Visibility::Hidden,
        Ghost,
    ));
    commands.insert_resource(ClassArt(art));

    commands.spawn((
        Text::new(""),
        TextFont { font_size: 14.0, ..default() },
        TextColor(Color::srgb(0.9, 0.9, 0.9)),
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(10.0),
            top: Val::Px(8.0),
            ..default()
        },
        HudText,
    ));
}

fn track_cursor(
    windows: Query<&Window, With<PrimaryWindow>>,
    camera: Query<(&Camera, &GlobalTransform), With<Camera2d>>,
    game: Res<Game>,
    mut cursor: ResMut<CursorUnits>,
) {
    cursor.0 = (|| {
        let pos = windows.single().ok()?.cursor_position()?;
        let (cam, cam_tf) = camera.single().ok()?;
        let world = cam.viewport_to_world_2d(cam_tf, pos).ok()?;
        Some(game.to_units(world))
    })();
}

fn toggle_mode(keys: Res<ButtonInput<KeyCode>>, mut mode: ResMut<Mode>, mut sel: ResMut<Sel>) {
    if keys.just_pressed(KeyCode::KeyP) {
        *mode = match *mode {
            Mode::Placement => Mode::Flight,
            Mode::Flight => Mode::Placement,
        };
        sel.drag = None;
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
    if buttons.just_pressed(MouseButton::Left) {
        if let Some(cur) = cursor.0 {
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
    }
    if buttons.just_released(MouseButton::Left) {
        sel.drag = None;
    }
    if let Some((entity, off)) = sel.drag {
        if let Ok((_, mut ship)) = ships.get_mut(entity) {
            if let Some(cur) = cursor.0 {
                ship.pose.anchor = cur + off;
            }
        }
    }
    // Rotation: scroll wheel or Q/E, applied to the dragged ship if any,
    // otherwise the ship under the cursor.
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
        if let Some(entity) = target {
            if let Ok((_, mut ship)) = ships.get_mut(entity) {
                // One notch / keypress = 15°.
                ship.pose.heading += steps as f64 * std::f64::consts::PI / 12.0;
            }
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
    // Selection: default to first ship, Tab cycles.
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
    if keys.just_pressed(KeyCode::Enter) {
        if let Ok(end) = maneuver::apply(ship.pose, dial[sel.dial_idx]) {
            ship.pose = end;
        }
    }
}

fn sync_ship_transforms(
    game: Res<Game>,
    mut ships: Query<(&ShipEnt, &mut Transform, &mut Sprite), Without<Ghost>>,
    mode: Res<Mode>,
) {
    for (ship, mut transform, mut sprite) in &mut ships {
        let class = &game.ships.classes[ship.class_idx];
        let (size, tf) = ship_visual(class, ship.pose, &game, 1.0);
        *transform = tf;
        sprite.custom_size = Some(size);
        // Placement legality tint.
        sprite.color = if *mode == Mode::Placement && !placement_ok(&game, ship) {
            Color::srgb(1.0, 0.4, 0.4)
        } else {
            Color::WHITE
        };
    }
}

fn placement_ok(game: &Game, ship: &ShipEnt) -> bool {
    // Checked against the zone only; ship-overlap tinting is drawn via the
    // preview outlines to keep this cheap.
    let class = &game.ships.classes[ship.class_idx];
    rules::placement_legal(&game.board, ship.seat, ship.pose, class.footprint, &[]).is_ok()
}

fn draw_overlays(
    mut gizmos: Gizmos,
    game: Res<Game>,
    mode: Res<Mode>,
    sel: Res<Sel>,
    ships: Query<(Entity, &ShipEnt)>,
    mut ghost: Query<(&mut Sprite, &mut Transform, &mut Visibility), With<Ghost>>,
    art: Res<ClassArt>,
) {
    // Board frame.
    let bsize = Vec2::new(game.board.width as f32 * PX, game.board.height as f32 * PX);
    gizmos.rect_2d(Isometry2d::IDENTITY, bsize, Color::srgb(0.45, 0.45, 0.6));

    // Base outlines for every ship.
    for (entity, ship) in &ships {
        let class = &game.ships.classes[ship.class_idx];
        let corners = rules::footprint_corners(ship.pose, class.footprint);
        let pts: Vec<Vec2> = corners
            .iter()
            .chain(std::iter::once(&corners[0]))
            .map(|&c| game.to_world(c))
            .collect();
        let color = match (ship.seat, sel.ship == Some(entity)) {
            (_, true) => Color::srgb(1.0, 0.9, 0.3),
            (Seat::South, _) => Color::srgba(0.3, 0.9, 0.4, 0.6),
            (Seat::North, _) => Color::srgba(0.9, 0.4, 0.3, 0.6),
        };
        gizmos.linestrip_2d(pts, color);
        // Bright front edge (corners 0-1) so facing is always readable.
        gizmos.line_2d(
            game.to_world(corners[0]),
            game.to_world(corners[1]),
            Color::srgb(0.5, 0.95, 1.0),
        );
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

    // Path line, colored by dial difficulty.
    let color = match man.difficulty {
        Difficulty::Easy => Color::srgb(0.35, 0.65, 1.0),
        Difficulty::Normal => Color::srgb(0.95, 0.95, 0.95),
        Difficulty::Hard => Color::srgb(1.0, 0.3, 0.3),
    };
    gizmos.linestrip_2d(path.iter().map(|p| game.to_world(p.anchor)), color);

    // Ghost sprite at the end pose (red-tinted if the path is obstructed).
    let others: Vec<_> = ships
        .iter()
        .filter(|(e, _)| Some(*e) != sel.ship)
        .map(|(_, s)| {
            (s.id, s.pose, game.ships.classes[s.class_idx].footprint)
        })
        .collect();
    let obstruction = rules::check_path(&game.board, &path, class.footprint, &others);
    let (size, tf) = ship_visual(class, end, &game, 2.0);
    gsprite.image = art.0[ship.class_idx].clone();
    gsprite.custom_size = Some(size);
    gsprite.color = if obstruction.is_some() {
        Color::srgba(1.0, 0.35, 0.35, 0.4)
    } else {
        Color::srgba(1.0, 1.0, 1.0, 0.35)
    };
    *gtf = tf;
    *gvis = Visibility::Visible;

    // End-pose base outline, bright front edge, and a heading arrow from
    // the nose — unmistakable even when a K-turn leaves the ship in place.
    let corners = rules::footprint_corners(end, class.footprint);
    let pts: Vec<Vec2> = corners
        .iter()
        .chain(std::iter::once(&corners[0]))
        .map(|&c| game.to_world(c))
        .collect();
    gizmos.linestrip_2d(pts, color);
    gizmos.line_2d(
        game.to_world(corners[0]),
        game.to_world(corners[1]),
        Color::srgb(0.5, 0.95, 1.0),
    );
    gizmos.arrow_2d(
        game.to_world(end.anchor),
        game.to_world(end.local_to_world(GVec2::new(0.8, 0.0))),
        color,
    );
}

fn steer_name(s: Steer) -> &'static str {
    match s {
        Steer::Straight => "Straight",
        Steer::BankLeft => "Bank Left",
        Steer::BankRight => "Bank Right",
        Steer::TurnLeft => "Turn Left",
        Steer::TurnRight => "Turn Right",
        Steer::TallonLeft => "Tallon Roll Left",
        Steer::TallonRight => "Tallon Roll Right",
        Steer::KTurn => "Koiogran Turn",
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
            "PLACEMENT — drag ships; scroll or Q/E rotates (hover or drag); P for flight mode"
                .into()
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
            let color = match man.difficulty {
                Difficulty::Easy => "BLUE",
                Difficulty::Normal => "WHITE",
                Difficulty::Hard => "RED",
            };
            let status = maneuver::sample_path(ship.pose, man)
                .ok()
                .and_then(|path| {
                    let others: Vec<_> = ships
                        .iter()
                        .filter(|(e, _)| Some(*e) != sel.ship)
                        .map(|(_, s)| (s.id, s.pose, game.ships.classes[s.class_idx].footprint))
                        .collect();
                    rules::check_path(&game.board, &path, class.footprint, &others)
                })
                .map(|o| match o {
                    PathObstruction::OffBoard { .. } => "  !! LEAVES BOARD".to_string(),
                    PathObstruction::ShipCollision { ship, .. } => {
                        format!("  !! COLLIDES with ship {}", ship.0)
                    }
                })
                .unwrap_or_default();
            format!(
                "FLIGHT — {} | [{}/{}] {} {} ({color}){status}\nTab: next ship  \u{2190}/\u{2192}: dial  Enter: execute  P: placement",
                class.name,
                idx + 1,
                dial.len(),
                steer_name(man.steer),
                man.distance,
            )
        }
    };
}
