//! Game setup: after pressing Create Game the host picks a scenario and
//! adjusts its numbers (obstacles, squad points, players, board) before
//! the connection is made. Keyboard-driven, rendered through the HUD text.

use bevy::prelude::*;

use sf_core::scenario::{BOARD_RANGE, GameSetup, MAX_TOKENS, POINTS_RANGE};
use sf_proto::messages::ClientMsg;
use sf_proto::tls::Target;

use crate::online::Online;
use crate::render::{Game, HudText};
use crate::{Screen, net};

/// What the menu prepared: where to connect and the messages that go
/// first (Hello, then CreateGame once the setup is chosen).
#[derive(Resource, Default)]
pub struct PendingCreate {
    pub target: Option<Target>,
    pub hello: Option<ClientMsg>,
    pub squad: Option<sf_core::squad::Squad>,
}

#[derive(Resource, Default)]
pub struct SetupForm {
    /// Selected preset (index into Content.scenarios).
    pub scenario: usize,
    pub setup: GameSetup,
    /// Highlighted number field (see FIELDS).
    pub field: usize,
}

const FIELDS: [&str; 7] = [
    "Asteroids",
    "Debris clouds",
    "Black holes",
    "Squad points",
    "Players",
    "Board width",
    "Board height",
];

pub fn plugin(app: &mut App) {
    app.init_resource::<PendingCreate>()
        .init_resource::<SetupForm>()
        .add_systems(OnEnter(Screen::Setup), enter)
        .add_systems(
            Update,
            (input.run_if(crate::glossary::closed), show).chain().run_if(in_state(Screen::Setup)),
        );
}

fn enter(mut form: ResMut<SetupForm>, game: Res<Game>) {
    form.field = 0;
    if let Some(s) = game.content.scenarios.scenarios.get(form.scenario) {
        form.setup = GameSetup::from(s);
    }
}

fn input(
    keys: Res<ButtonInput<KeyCode>>,
    mut form: ResMut<SetupForm>,
    game: Res<Game>,
    mut pending: ResMut<PendingCreate>,
    mut online: ResMut<Online>,
    mut next: ResMut<NextState<Screen>>,
) {
    let presets = &game.content.scenarios.scenarios;
    if keys.just_pressed(KeyCode::Escape) {
        next.set(Screen::Menu);
        return;
    }
    if !presets.is_empty() {
        let n = presets.len();
        let mut pick = None;
        if keys.just_pressed(KeyCode::ArrowUp) {
            pick = Some((form.scenario + n - 1) % n);
        }
        if keys.just_pressed(KeyCode::ArrowDown) {
            pick = Some((form.scenario + 1) % n);
        }
        if let Some(i) = pick {
            form.scenario = i;
            form.setup = GameSetup::from(&presets[i]);
        }
    }
    let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
    if keys.just_pressed(KeyCode::Tab) {
        form.field = if shift {
            (form.field + FIELDS.len() - 1) % FIELDS.len()
        } else {
            (form.field + 1) % FIELDS.len()
        };
    }
    let delta: i32 = i32::from(keys.just_pressed(KeyCode::ArrowRight))
        - i32::from(keys.just_pressed(KeyCode::ArrowLeft));
    if delta != 0 {
        let field = form.field;
        let s = &mut form.setup;
        let bump_u8 = |v: u8, max: u8| (v as i32 + delta).clamp(0, max as i32) as u8;
        match field {
            0 => s.asteroids = bump_u8(s.asteroids, MAX_TOKENS - s.debris - s.black_holes),
            1 => s.debris = bump_u8(s.debris, MAX_TOKENS - s.asteroids - s.black_holes),
            2 => s.black_holes = bump_u8(s.black_holes, 2.min(MAX_TOKENS - s.asteroids - s.debris)),
            3 => {
                s.points = (s.points as i32 + delta * 10)
                    .clamp(POINTS_RANGE.0 as i32, POINTS_RANGE.1 as i32)
                    as u32
            }
            4 => s.players = bump_u8(s.players, 8).max(2),
            5 => {
                s.board_width =
                    (s.board_width + f64::from(delta) * 2.0).clamp(BOARD_RANGE.0, BOARD_RANGE.1)
            }
            _ => {
                s.board_height =
                    (s.board_height + f64::from(delta) * 2.0).clamp(BOARD_RANGE.0, BOARD_RANGE.1)
            }
        }
        // Edited numbers make it a custom variant of the preset.
        let base = presets.get(form.scenario).map(|p| p.name.clone()).unwrap_or_default();
        if !form.setup.scenario.ends_with("(custom)") {
            form.setup.scenario = format!("{base} (custom)");
        }
    }
    if keys.just_pressed(KeyCode::Enter) {
        if let Err(why) = form.setup.validate() {
            online.status = format!("setup: {why}");
            return;
        }
        let (Some(target), Some(hello)) = (pending.target.take(), pending.hello.take()) else {
            next.set(Screen::Menu);
            return;
        };
        let create =
            ClientMsg::CreateGame { squad: pending.squad.take(), setup: Some(form.setup.clone()) };
        *online = Online::default();
        online.setup = Some(form.setup.clone());
        online.status = format!("Connecting to {}…", target.key());
        online.net = Some(net::connect(target.clone(), vec![hello, create]));
        online.target = Some(target);
        next.set(Screen::Online);
    }
}

fn show(
    form: Res<SetupForm>,
    game: Res<Game>,
    online: Res<Online>,
    mut hud: Query<&mut Text, With<HudText>>,
) {
    let Ok(mut text) = hud.single_mut() else { return };
    let presets = &game.content.scenarios.scenarios;
    let mut lines = vec![
        "GAME SETUP — Up/Down: scenario • Tab: field • Left/Right: adjust • Enter: create the game • Esc: back".to_string(),
        String::new(),
    ];
    for (i, s) in presets.iter().enumerate() {
        lines.push(format!("{} {}", if i == form.scenario { ">" } else { " " }, s.name));
    }
    if let Some(s) = presets.get(form.scenario) {
        lines.push(String::new());
        lines.push(s.description.clone());
    }
    lines.push(String::new());
    let s = &form.setup;
    let values = [
        s.asteroids.to_string(),
        s.debris.to_string(),
        s.black_holes.to_string(),
        s.points.to_string(),
        s.players.to_string(),
        s.board_width.to_string(),
        s.board_height.to_string(),
    ];
    for (i, (name, value)) in FIELDS.iter().zip(values.iter()).enumerate() {
        let (o, c) = if i == form.field { ("[", "]") } else { (" ", " ") };
        lines.push(format!("  {o}{name}: {value}{c}"));
    }
    lines.push(String::new());
    lines.push(format!("Will create: {}", s.summary()));
    if s.players != 2 {
        lines.push("(only 2-player games can be created for now)".into());
    }
    if !online.status.is_empty() {
        lines.push(online.status.clone());
    }
    text.0 = lines.join("\n");
}
