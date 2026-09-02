//! Shared rendering/resources used by both the offline sandbox and the
//! connected online mode.

use bevy::input::mouse::MouseMotion;
use bevy::prelude::*;
use bevy::render::camera::Viewport;
use bevy::window::PrimaryWindow;
use std::f32::consts::FRAC_PI_2 as FRAC_PI_2_F32;

use sf_core::board::Board;
use sf_core::data::{ManeuverDb, ShipDb};
use sf_core::geometry::{Footprint, Pose, Vec2 as GVec2};
use sf_core::maneuver::{Difficulty, Maneuver, Steer};
use sf_core::rules;
use sf_core::ship::{ShipClass, ShipClassId};

/// Screen pixels per game unit.
pub const PX: f32 = 30.0;

#[derive(Resource)]
pub struct Game {
    pub board: Board,
    pub ships: ShipDb,
    pub dials: ManeuverDb,
}

impl Game {
    pub fn dial(&self, class: &ShipClass) -> &[Maneuver] {
        self.dials.set(class.maneuver_set).map(|s| s.maneuvers.as_slice()).unwrap_or(&[])
    }

    pub fn class_index(&self, id: ShipClassId) -> usize {
        self.ships.classes.iter().position(|c| c.id == id).unwrap_or(0)
    }

    /// Board units → world (board centered on the origin).
    pub fn to_world(&self, p: GVec2) -> Vec2 {
        Vec2::new(
            (p.x - self.board.width / 2.0) as f32 * PX,
            (p.y - self.board.height / 2.0) as f32 * PX,
        )
    }

    pub fn to_units(&self, w: Vec2) -> GVec2 {
        GVec2::new(
            (w.x / PX) as f64 + self.board.width / 2.0,
            (w.y / PX) as f64 + self.board.height / 2.0,
        )
    }
}

#[derive(Resource, Default)]
pub struct Sel {
    pub ship: Option<Entity>,
    pub dial_idx: usize,
    /// While dragging: (ship, anchor offset from cursor).
    pub drag: Option<(Entity, GVec2)>,
}

#[derive(Resource)]
pub struct CursorUnits(pub Option<GVec2>);

/// Firing-arc overlay toggle (F key). On by default.
#[derive(Resource)]
pub struct ShowArcs(pub bool);

/// One image handle per ship class, indexed like `Game::ships.classes`.
#[derive(Resource)]
pub struct ClassArt(pub Vec<Handle<Image>>);

/// The single translucent preview ship, shared by both modes.
#[derive(Component)]
pub struct Ghost;

#[derive(Component)]
pub struct HudText;

/// Dark quad marking the play area (kept sized to the board).
#[derive(Component)]
pub struct BoardQuad;

/// Translucent quad shading the bullseye lane on maneuver previews.
#[derive(Component)]
pub struct BullseyeShade;

/// Pose whose bullseye lane should be shaded this frame (None = hidden).
/// The active mode's draw system writes it; `apply_bullseye` renders it.
#[derive(Resource, Default)]
pub struct BullseyePreview(pub Option<Pose>);

pub fn apply_bullseye(
    preview: Res<BullseyePreview>,
    game: Res<Game>,
    mut shades: Query<(&mut Sprite, &mut Transform, &mut Visibility), With<BullseyeShade>>,
) {
    for (mut sprite, mut tf, mut vis) in &mut shades {
        match preview.0 {
            None => *vis = Visibility::Hidden,
            Some(pose) => {
                let len = sf_core::combat::BULLSEYE_LENGTH_UNITS;
                let wid = sf_core::combat::BULLSEYE_WIDTH_UNITS;
                sprite.custom_size = Some(Vec2::new(len as f32 * PX, wid as f32 * PX));
                let center = game.to_world(pose.local_to_world(GVec2::new(len / 2.0, 0.0)));
                *tf = Transform::from_translation(center.extend(0.5))
                    .with_rotation(Quat::from_rotation_z(pose.heading as f32));
                *vis = Visibility::Visible;
            }
        }
    }
}

/// Sprite size + transform so the artwork's front-center pixel lands on the
/// pose anchor. Sprites face up; heading 0 = +X, so rotate by heading - 90°.
pub fn ship_visual(class: &ShipClass, pose: Pose, game: &Game, z: f32) -> (Vec2, Transform) {
    let (iw, ih) = class.sprite_px;
    let (ax, ay) = class.anchor_px;
    let upp = class.footprint.length / ih as f64; // units per sprite pixel
    let size = Vec2::new((iw as f64 * upp) as f32 * PX, class.footprint.length as f32 * PX);
    let center_local =
        GVec2::new((ay as f64 - ih as f64 / 2.0) * upp, (ax as f64 - iw as f64 / 2.0) * upp);
    let world = game.to_world(pose.local_to_world(center_local));
    let transform = Transform::from_translation(world.extend(z))
        .with_rotation(Quat::from_rotation_z(pose.heading as f32 - FRAC_PI_2_F32));
    (size, transform)
}

pub fn steer_name(s: Steer) -> &'static str {
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

pub fn difficulty_color(d: Difficulty) -> Color {
    match d {
        Difficulty::Easy => Color::srgb(0.35, 0.65, 1.0),
        Difficulty::Normal => Color::srgb(0.95, 0.95, 0.95),
        Difficulty::Hard => Color::srgb(1.0, 0.3, 0.3),
    }
}

pub fn difficulty_label(d: Difficulty) -> &'static str {
    match d {
        Difficulty::Easy => "BLUE",
        Difficulty::Normal => "WHITE",
        Difficulty::Hard => "RED",
    }
}

/// Base outline plus a bright cyan front edge so facing always reads.
pub fn draw_base(gizmos: &mut Gizmos, game: &Game, pose: Pose, fp: Footprint, color: Color) {
    let corners = rules::footprint_corners(pose, fp);
    let pts: Vec<Vec2> =
        corners.iter().chain(std::iter::once(&corners[0])).map(|&c| game.to_world(c)).collect();
    gizmos.linestrip_2d(pts, color);
    gizmos.line_2d(
        game.to_world(corners[0]),
        game.to_world(corners[1]),
        Color::srgb(0.5, 0.95, 1.0),
    );
}

/// Board frame and translucent deployment-zone bands (gizmo lines).
pub fn draw_board(gizmos: &mut Gizmos, game: &Game) {
    let bsize = Vec2::new(game.board.width as f32 * PX, game.board.height as f32 * PX);
    gizmos.rect_2d(Isometry2d::IDENTITY, bsize, Color::srgb(0.45, 0.45, 0.6));
    let w = game.board.width;
    let h = game.board.height;
    let d = game.board.deploy_depth;
    for (y, color) in
        [(d, Color::srgba(0.3, 0.9, 0.4, 0.5)), (h - d, Color::srgba(0.9, 0.4, 0.3, 0.5))]
    {
        gizmos.line_2d(game.to_world(GVec2::new(0.0, y)), game.to_world(GVec2::new(w, y)), color);
    }
}

pub fn draw_path(gizmos: &mut Gizmos, game: &Game, path: &[Pose], color: Color) {
    gizmos.linestrip_2d(path.iter().map(|p| game.to_world(p.anchor)), color);
}

pub fn draw_heading_arrow(gizmos: &mut Gizmos, game: &Game, pose: Pose, color: Color) {
    gizmos.arrow_2d(
        game.to_world(pose.anchor),
        game.to_world(pose.local_to_world(GVec2::new(0.8, 0.0))),
        color,
    );
}

/// Ghost overlay of the front firing arc: a 90° cone from the base center
/// with the three range-band boundaries.
pub fn draw_firing_arc(gizmos: &mut Gizmos, game: &Game, pose: Pose, fp: Footprint, alpha: f32) {
    let center = sf_core::combat::base_center(pose, fp);
    let half_len = fp.length / 2.0;
    let outer =
        half_len + sf_core::combat::MAX_RANGE_BAND as f64 * sf_core::combat::RANGE_BAND_UNITS;
    let color = Color::srgba(1.0, 0.75, 0.2, alpha);
    let faint = Color::srgba(1.0, 0.75, 0.2, alpha * 0.45);

    let point_at = |angle: f64, r: f64| {
        game.to_world(GVec2::new(center.x + r * angle.cos(), center.y + r * angle.sin()))
    };
    let (a0, a1) =
        (pose.heading - std::f64::consts::FRAC_PI_4, pose.heading + std::f64::consts::FRAC_PI_4);
    for a in [a0, a1] {
        gizmos.line_2d(point_at(a, half_len), point_at(a, outer), color);
    }
    for band in 1..=sf_core::combat::MAX_RANGE_BAND {
        let r = half_len + band as f64 * sf_core::combat::RANGE_BAND_UNITS;
        let pts: Vec<Vec2> =
            (0..=24).map(|i| point_at(a0 + (a1 - a0) * i as f64 / 24.0, r)).collect();
        gizmos
            .linestrip_2d(pts, if band == sf_core::combat::MAX_RANGE_BAND { color } else { faint });
    }
}

pub fn track_cursor(
    windows: Query<&Window, With<PrimaryWindow>>,
    camera: Query<(&Camera, &GlobalTransform), With<MainCam>>,
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

pub fn toggle_arcs(keys: Res<ButtonInput<KeyCode>>, mut arcs: ResMut<ShowArcs>) {
    if keys.just_pressed(KeyCode::KeyF) {
        arcs.0 = !arcs.0;
    }
}

pub fn sync_board_quad(game: Res<Game>, mut quads: Query<&mut Sprite, With<BoardQuad>>) {
    if !game.is_changed() {
        return;
    }
    for mut sprite in &mut quads {
        sprite.custom_size =
            Some(Vec2::new(game.board.width as f32 * PX, game.board.height as f32 * PX));
    }
}

/// The player's main camera (pan/zoom); the minimap camera is separate.
#[derive(Component)]
pub struct MainCam;

/// Inset overview camera, shown only when the board doesn't fit the view.
#[derive(Component)]
pub struct MiniCam;

/// Main-view control: `scale` = world px per screen px (1 = native),
/// `pan` = world-space offset of the view center.
#[derive(Resource)]
pub struct ViewCtl {
    pub scale: f32,
    pub pan: Vec2,
}

impl Default for ViewCtl {
    fn default() -> Self {
        Self { scale: 1.0, pan: Vec2::ZERO }
    }
}

/// Minimap size and margin in logical pixels.
const MINI_PX: f32 = 180.0;
const MINI_MARGIN: f32 = 12.0;

/// +/- zoom, right-mouse drag pans, Home resets.
pub fn view_input(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut motion: EventReader<MouseMotion>,
    mut view: ResMut<ViewCtl>,
) {
    if keys.just_pressed(KeyCode::Equal) || keys.just_pressed(KeyCode::NumpadAdd) {
        view.scale = (view.scale * 0.8).max(0.25);
    }
    if keys.just_pressed(KeyCode::Minus) || keys.just_pressed(KeyCode::NumpadSubtract) {
        view.scale = (view.scale / 0.8).min(4.0);
    }
    if keys.just_pressed(KeyCode::Home) {
        *view = ViewCtl::default();
    }
    let drag: Vec2 = motion.read().map(|m| m.delta).sum();
    if buttons.pressed(MouseButton::Right) && drag != Vec2::ZERO {
        // Screen y grows downward; world y grows upward.
        view.pan.x -= drag.x * view.scale;
        view.pan.y += drag.y * view.scale;
    }
}

/// Apply the view control to the main camera and decide whether the
/// minimap is needed (board not fully visible); size/position it and draw
/// the main view's footprint on it.
pub fn apply_view(
    view: Res<ViewCtl>,
    game: Res<Game>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut main_cam: Query<(&mut Transform, &mut Projection), (With<MainCam>, Without<MiniCam>)>,
    mut mini_cam: Query<(&mut Camera, &mut Projection), (With<MiniCam>, Without<MainCam>)>,
    mut gizmos: Gizmos,
) {
    let Ok(window) = windows.single() else { return };
    if let Ok((mut tf, mut proj)) = main_cam.single_mut() {
        tf.translation = view.pan.extend(tf.translation.z);
        if let Projection::Orthographic(o) = &mut *proj {
            o.scale = view.scale;
        }
    }
    let visible = Vec2::new(window.width(), window.height()) * view.scale;
    let board = Vec2::new(game.board.width as f32 * PX, game.board.height as f32 * PX);
    let fits = (view.pan.x - visible.x / 2.0) <= -board.x / 2.0
        && (view.pan.x + visible.x / 2.0) >= board.x / 2.0
        && (view.pan.y - visible.y / 2.0) <= -board.y / 2.0
        && (view.pan.y + visible.y / 2.0) >= board.y / 2.0;

    let Ok((mut cam, mut proj)) = mini_cam.single_mut() else { return };
    if fits {
        cam.is_active = false;
        return;
    }
    let sf = window.scale_factor();
    let size = (MINI_PX * sf) as u32;
    let (pw, ph) = (window.physical_width(), window.physical_height());
    if pw <= size || ph <= size {
        cam.is_active = false;
        return;
    }
    let margin = (MINI_MARGIN * sf) as u32;
    cam.is_active = true;
    cam.viewport = Some(Viewport {
        physical_position: UVec2::new(pw - size - margin, ph - size - margin),
        physical_size: UVec2::new(size, size),
        ..default()
    });
    if let Projection::Orthographic(o) = &mut *proj {
        // Fit the whole board (with a little margin) into the inset.
        o.scale = board.max_element() * 1.12 / MINI_PX;
    }
    // Main-view footprint, visible on the inset (coincides with the main
    // view's own edges, so it's invisible there).
    gizmos.rect_2d(
        Isometry2d::from_translation(view.pan),
        visible,
        Color::srgba(1.0, 1.0, 1.0, 0.8),
    );
}
