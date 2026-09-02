//! Main menu: name / server / password / fingerprint / join code fields,
//! and Create / Join / Sandbox actions. The Server field accepts the
//! `starfight://host:port/#fingerprint` join string printed by the server
//! (Ctrl+V pastes); pins are remembered per host:port after the first
//! successful connection.

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use sf_proto::messages::ClientMsg;
use sf_proto::tls::{Target, parse_target};

use crate::online::Online;
use crate::render::{Game, HudText};
use crate::squad_builder::Builder;
use crate::{Screen, net, pins};

#[derive(Resource)]
pub struct MenuForm {
    pub name: String,
    pub addr: String,
    pub password: String,
    pub fingerprint: String,
    pub code: String,
    pub focus: Field,
    pub error: String,
}

impl MenuForm {
    /// Last-used name/server from the config file; a command-line join
    /// string (`sf-client starfight://…`) overrides the server.
    pub fn load(arg_addr: Option<String>) -> Self {
        let saved = pins::load_menu();
        Self {
            name: saved.get("name").cloned().unwrap_or_else(|| "pilot".into()),
            addr: arg_addr
                .or_else(|| saved.get("server").cloned())
                .unwrap_or_else(|| "127.0.0.1:7777".into()),
            password: String::new(),
            fingerprint: String::new(),
            code: String::new(),
            focus: Field::Name,
            error: String::new(),
        }
    }
}

#[derive(Component, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Name,
    Addr,
    Password,
    Fingerprint,
    Code,
}

const FIELDS: [Field; 5] =
    [Field::Name, Field::Addr, Field::Password, Field::Fingerprint, Field::Code];

#[derive(Component, Clone, Copy)]
pub enum MenuAction {
    Create,
    Join,
    Sandbox,
    Squad,
    PrevSquad,
    NextSquad,
}

#[derive(Component)]
struct MenuTag;

#[derive(Component)]
struct FieldLabel(Field);

#[derive(Component)]
struct ErrorLabel;

#[derive(Component)]
struct SquadLabel;

pub fn plugin(app: &mut App) {
    app.insert_resource(MenuForm::load(std::env::args().nth(1)))
        .add_systems(OnEnter(Screen::Menu), spawn_menu)
        .add_systems(OnExit(Screen::Menu), despawn_menu)
        .add_systems(Update, (typing, buttons, refresh).chain().run_if(in_state(Screen::Menu)));
}

const IDLE: Color = Color::srgba(0.12, 0.14, 0.22, 0.92);
const FOCUS: Color = Color::srgba(0.2, 0.26, 0.42, 0.95);
const ACTION: Color = Color::srgba(0.15, 0.3, 0.2, 0.92);

fn spawn_menu(
    mut commands: Commands,
    mut hud: Query<&mut Text, With<HudText>>,
    mut builder: ResMut<Builder>,
) {
    if let Ok(mut t) = hud.single_mut() {
        t.0.clear();
    }
    builder.refresh_saved();
    commands
        .spawn((
            MenuTag,
            Node {
                position_type: PositionType::Absolute,
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                flex_direction: FlexDirection::Column,
                align_items: AlignItems::Center,
                justify_content: JustifyContent::Center,
                row_gap: Val::Px(10.0),
                ..default()
            },
        ))
        .with_children(|root| {
            root.spawn((
                Text::new("STAR FIGHT"),
                TextFont { font_size: 42.0, ..default() },
                TextColor(Color::srgb(0.9, 0.9, 1.0)),
            ));
            for field in FIELDS {
                root.spawn((
                    Button,
                    field,
                    Node {
                        width: Val::Px(420.0),
                        padding: UiRect::axes(Val::Px(12.0), Val::Px(8.0)),
                        ..default()
                    },
                    BackgroundColor(IDLE),
                ))
                .with_children(|b| {
                    b.spawn((
                        Text::new(""),
                        FieldLabel(field),
                        TextFont { font_size: 17.0, ..default() },
                        TextColor(Color::srgb(0.9, 0.9, 0.9)),
                        // Join strings / fingerprints have no word
                        // boundaries: wrap anywhere so they stay inside
                        // the field instead of running off the window.
                        TextLayout::new_with_linebreak(LineBreak::AnyCharacter),
                        Node { max_width: Val::Px(396.0), ..default() },
                    ));
                });
            }
            root.spawn((Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(10.0),
                ..default()
            },))
                .with_children(|row| {
                    for (action, label) in [
                        (MenuAction::Create, "Create Game"),
                        (MenuAction::Join, "Join Game"),
                        (MenuAction::Sandbox, "Offline Sandbox"),
                        (MenuAction::Squad, "Squad Builder"),
                    ] {
                        row.spawn((
                            Button,
                            action,
                            Node {
                                padding: UiRect::axes(Val::Px(16.0), Val::Px(10.0)),
                                ..default()
                            },
                            BackgroundColor(ACTION),
                        ))
                        .with_children(|b| {
                            b.spawn((
                                Text::new(label),
                                TextFont { font_size: 17.0, ..default() },
                                TextColor(Color::srgb(0.85, 1.0, 0.9)),
                            ));
                        });
                    }
                });
            // Squad picker: ◀ [current squad summary] ▶
            root.spawn((Node {
                flex_direction: FlexDirection::Row,
                column_gap: Val::Px(8.0),
                align_items: AlignItems::Center,
                ..default()
            },))
                .with_children(|row| {
                    for (action, label) in
                        [(MenuAction::PrevSquad, "◀"), (MenuAction::NextSquad, "▶")]
                    {
                        if matches!(action, MenuAction::NextSquad) {
                            row.spawn((
                                Text::new(""),
                                SquadLabel,
                                TextFont { font_size: 14.0, ..default() },
                                TextColor(Color::srgb(0.8, 0.9, 1.0)),
                            ));
                        }
                        row.spawn((
                            Button,
                            action,
                            Node {
                                padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                                ..default()
                            },
                            BackgroundColor(ACTION),
                        ))
                        .with_children(|b| {
                            b.spawn((
                                Text::new(label),
                                TextFont { font_size: 15.0, ..default() },
                                TextColor(Color::srgb(0.85, 1.0, 0.9)),
                            ));
                        });
                    }
                });
            root.spawn((
                Text::new(""),
                ErrorLabel,
                TextFont { font_size: 15.0, ..default() },
                TextColor(Color::srgb(1.0, 0.55, 0.5)),
            ));
            root.spawn((
                Text::new(
                    "click a field to edit • Tab switches fields • Ctrl+V pastes\n\
                     Server takes the starfight://host:port/#fingerprint join string",
                ),
                TextFont { font_size: 13.0, ..default() },
                TextColor(Color::srgba(0.8, 0.8, 0.9, 0.6)),
            ));
        });
}

fn despawn_menu(mut commands: Commands, menu: Query<Entity, With<MenuTag>>) {
    for e in &menu {
        commands.entity(e).despawn();
    }
}

fn typing(
    mut events: EventReader<KeyboardInput>,
    keys: Res<ButtonInput<KeyCode>>,
    mut form: ResMut<MenuForm>,
) {
    let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        if ctrl {
            if ev.key_code == KeyCode::KeyV {
                let f = form.focus;
                match arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
                    Ok(text) => {
                        let buf = field_mut(&mut form, f);
                        buf.clear();
                        buf.extend(text.trim().chars().filter(|c| !c.is_control()).take(200));
                    }
                    Err(e) => form.error = format!("clipboard: {e}"),
                }
            }
            continue;
        }
        match &ev.logical_key {
            Key::Tab => {
                let i = FIELDS.iter().position(|f| *f == form.focus).unwrap_or(0);
                form.focus = FIELDS[(i + 1) % FIELDS.len()];
            }
            Key::Backspace => {
                let f = form.focus;
                field_mut(&mut form, f).pop();
            }
            Key::Space => {
                let f = form.focus;
                field_mut(&mut form, f).push(' ');
            }
            Key::Character(s) => {
                let f = form.focus;
                let upper = f == Field::Code;
                let buf = field_mut(&mut form, f);
                for c in s.chars().filter(|c| !c.is_control()) {
                    if buf.len() < 200 {
                        buf.push(if upper { c.to_ascii_uppercase() } else { c });
                    }
                }
            }
            _ => {}
        }
    }
}

fn field_mut(form: &mut MenuForm, f: Field) -> &mut String {
    match f {
        Field::Name => &mut form.name,
        Field::Addr => &mut form.addr,
        Field::Password => &mut form.password,
        Field::Fingerprint => &mut form.fingerprint,
        Field::Code => &mut form.code,
    }
}

/// Resolve the typed server + fingerprint against the remembered pin for
/// that host:port. A remembered pin that contradicts the typed one is a
/// hard error — never a silent re-pin.
fn resolve_target(form: &MenuForm) -> Result<Target, String> {
    // Plaintext ws:// needs no pin; otherwise a remembered pin can stand in
    // for a missing fingerprint.
    let mut target = match parse_target(&form.addr, &form.fingerprint) {
        Ok(t) => t,
        Err(e) if form.fingerprint.trim().is_empty() => {
            let Ok(probe) = parse_target(&form.addr, "0000000000000000") else {
                return Err(e);
            };
            match pins::pin_for(&probe.key()) {
                Some(pin) => Target { fingerprint: Some(pin), ..probe },
                None => return Err(e),
            }
        }
        Err(e) => return Err(e),
    };
    if let (Some(typed), Some(saved)) = (&target.fingerprint, pins::pin_for(&target.key())) {
        if saved.starts_with(typed.as_str()) {
            target.fingerprint = Some(saved);
        } else if !typed.starts_with(saved.as_str()) {
            return Err(format!(
                "FINGERPRINT CHANGED for {}: remembered {}… but given {}…. If the server was \
                 reinstalled, delete its line in {}",
                target.key(),
                &saved[..16],
                &typed[..16],
                pins::pins_path().display()
            ));
        }
    }
    Ok(target)
}

fn buttons(
    interactions: Query<
        (&Interaction, Option<&Field>, Option<&MenuAction>),
        (Changed<Interaction>, With<Button>),
    >,
    mut form: ResMut<MenuForm>,
    mut online: ResMut<Online>,
    mut next: ResMut<NextState<Screen>>,
    mut builder: ResMut<Builder>,
    game: Res<Game>,
) {
    for (interaction, field, action) in &interactions {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if let Some(f) = field {
            form.focus = *f;
        }
        let Some(action) = action else { continue };
        match action {
            MenuAction::Sandbox => next.set(Screen::Sandbox),
            MenuAction::Squad => next.set(Screen::Squad),
            MenuAction::PrevSquad => form.error = builder.cycle_saved(&game, -1),
            MenuAction::NextSquad => form.error = builder.cycle_saved(&game, 1),
            MenuAction::Create | MenuAction::Join => {
                if form.name.trim().is_empty() {
                    form.error = "enter a name first".into();
                    continue;
                }
                let target = match resolve_target(&form) {
                    Ok(t) => t,
                    Err(e) => {
                        form.error = e;
                        continue;
                    }
                };
                let hello = ClientMsg::Hello {
                    proto_version: sf_proto::PROTOCOL_VERSION,
                    name: form.name.trim().into(),
                    password: form.password.clone(),
                };
                let squad = builder.squad_for_play(&game);
                let second = match action {
                    MenuAction::Create => ClientMsg::CreateGame { squad },
                    MenuAction::Join => {
                        if form.code.trim().is_empty() {
                            form.error = "enter the game code to join".into();
                            continue;
                        }
                        ClientMsg::JoinGame { code: form.code.trim().into(), squad }
                    }
                    _ => unreachable!(),
                };
                form.error.clear();
                pins::save_menu(form.name.trim(), form.addr.trim());
                *online = Online::default();
                online.status = format!("Connecting to {}…", target.key());
                online.net = Some(net::connect(target.clone(), vec![hello, second]));
                online.target = Some(target);
                next.set(Screen::Online);
            }
        }
    }
}

fn refresh(
    form: Res<MenuForm>,
    builder: Res<Builder>,
    game: Res<Game>,
    mut labels: Query<(&FieldLabel, &mut Text), (Without<ErrorLabel>, Without<SquadLabel>)>,
    mut error: Query<&mut Text, (With<ErrorLabel>, Without<SquadLabel>)>,
    mut squad: Query<&mut Text, (With<SquadLabel>, Without<ErrorLabel>)>,
    mut fields: Query<(&Field, &mut BackgroundColor), With<Button>>,
) {
    if let Ok(mut t) = squad.single_mut() {
        let (saved, cur) = builder.saved_names();
        let pos = match cur {
            Some(i) => format!("saved squad {} of {}", i + 1, saved.len()),
            None if saved.is_empty() => "no saved squads yet".into(),
            None => format!("{} saved squads", saved.len()),
        };
        t.0 = format!("{}\n{pos} — ◀ ▶ picks the squad to fly", builder.summary(&game));
    }
    for (label, mut text) in &mut labels {
        let masked = "•".repeat(form.password.chars().count());
        let (title, value) = match label.0 {
            Field::Name => ("Name", form.name.clone()),
            Field::Addr => ("Server", form.addr.clone()),
            Field::Password => ("Password", masked),
            Field::Fingerprint => ("Cert fingerprint", form.fingerprint.clone()),
            Field::Code => ("Game code", form.code.clone()),
        };
        let cursor = if form.focus == label.0 { "_" } else { "" };
        text.0 = format!("{title}: {value}{cursor}");
    }
    if let Ok(mut t) = error.single_mut() {
        t.0 = form.error.clone();
    }
    for (field, mut bg) in &mut fields {
        bg.0 = if *field == form.focus { FOCUS } else { IDLE };
    }
}
