//! Star Fight client: main menu → offline sandbox or connected play.

// Idiomatic Bevy trips these two lints constantly: ECS systems take many
// parameters and query types are structurally complex. Everything else is
// held to zero clippy warnings (warnings are treated as errors).
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

mod menu;
mod net;
mod online;
mod render;
mod sandbox;
mod starfield;

use bevy::prelude::*;
use bevy::render::settings::{Backends, RenderCreation, WgpuSettings};
use bevy::render::RenderPlugin;

use sf_core::board::Board;
use sf_core::data::{ManeuverDb, ShipDb};

use render::{BoardQuad, ClassArt, Game, Ghost, HudText, PX};

#[derive(States, Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum Screen {
    #[default]
    Menu,
    Sandbox,
    Online,
}

fn assets_dir() -> String {
    let dev = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets");
    if std::path::Path::new(dev).exists() { dev.to_string() } else { "assets".to_string() }
}

fn main() {
    let assets = assets_dir();
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
                // Vulkan / Metal / DX12 only: skipping the OpenGL probe avoids
                // a spurious EGL "eglCreateContext" error at startup on
                // Mesa/V3D (Raspberry Pi) and a wasted context creation.
                .set(RenderPlugin {
                    render_creation: RenderCreation::Automatic(WgpuSettings {
                        backends: Some(Backends::PRIMARY),
                        ..default()
                    }),
                    ..default()
                })
                .set(AssetPlugin { file_path: assets, ..default() })
                .set(WindowPlugin {
                    primary_window: Some(Window {
                        title: "Star Fight".into(),
                        resolution: (760.0_f32, 700.0_f32).into(),
                        ..default()
                    }),
                    ..default()
                }),
        )
        .insert_resource(ClearColor(Color::srgb(0.01, 0.01, 0.03)))
        .insert_resource(game)
        .insert_resource(render::Sel::default())
        .insert_resource(render::CursorUnits(None))
        .insert_resource(render::ShowArcs(true))
        .insert_resource(render::BullseyePreview::default())
        .insert_resource(render::ViewCtl::default())
        .init_state::<Screen>()
        .add_systems(Startup, global_setup)
        .add_systems(
            Update,
            (
                render::track_cursor,
                render::toggle_arcs,
                render::sync_board_quad,
                render::apply_bullseye,
                render::view_input,
                render::apply_view,
            ),
        )
        .add_plugins((menu::plugin, sandbox::plugin, online::plugin))
        .run();
}

fn global_setup(
    mut commands: Commands,
    game: Res<Game>,
    mut images: ResMut<Assets<Image>>,
    asset_server: Res<AssetServer>,
) {
    commands.spawn((Camera2d, render::MainCam, IsDefaultUiCamera));
    // Inset overview: activated by apply_view only when the board doesn't
    // fit the main view (zoomed/panned or oversized boards).
    commands.spawn((
        Camera2d,
        Camera {
            order: 1,
            is_active: false,
            clear_color: ClearColorConfig::Custom(Color::srgb(0.01, 0.01, 0.03)),
            ..default()
        },
        render::MiniCam,
    ));

    // Procedural space backdrop (different sky every launch).
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0xC0FFEE);
    commands.spawn((
        Sprite {
            image: images.add(starfield::starfield_image(1024, 1024, seed)),
            custom_size: Some(Vec2::splat(1500.0)),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, -30.0),
    ));

    // Dark quad marking the play area, kept sized to the board.
    commands.spawn((
        Sprite::from_color(
            Color::srgba(0.02, 0.03, 0.08, 0.82),
            Vec2::new(game.board.width as f32 * PX, game.board.height as f32 * PX),
        ),
        Transform::from_xyz(0.0, 0.0, -10.0),
        BoardQuad,
    ));

    // One image handle per ship class.
    let art: Vec<Handle<Image>> =
        game.ships.classes.iter().map(|c| asset_server.load(c.sprite.clone())).collect();
    commands.insert_resource(ClassArt(art));

    // Shared translucent preview ship.
    commands.spawn((
        Sprite { color: Color::srgba(1.0, 1.0, 1.0, 0.35), ..default() },
        Transform::default(),
        Visibility::Hidden,
        Ghost,
    ));

    // Bullseye lane shading for maneuver previews.
    commands.spawn((
        Sprite::from_color(Color::srgba(1.0, 0.75, 0.2, 0.16), Vec2::ONE),
        Transform::default(),
        Visibility::Hidden,
        render::BullseyeShade,
    ));

    // Shared HUD text.
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
