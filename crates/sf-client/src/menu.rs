//! Main menu: name / server address / join code fields, and
//! Create / Join / Sandbox actions.

use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::input::ButtonState;
use bevy::prelude::*;

use sf_proto::messages::ClientMsg;

use crate::online::Online;
use crate::render::HudText;
use crate::{net, Screen};

#[derive(Resource)]
pub struct MenuForm {
    pub name: String,
    pub addr: String,
    pub code: String,
    pub focus: Field,
    pub error: String,
}

impl Default for MenuForm {
    fn default() -> Self {
        Self {
            name: "pilot".into(),
            addr: "ws://127.0.0.1:7777".into(),
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
    Code,
}

#[derive(Component, Clone, Copy)]
pub enum MenuAction {
    Create,
    Join,
    Sandbox,
}

#[derive(Component)]
struct MenuTag;

#[derive(Component)]
struct FieldLabel(Field);

#[derive(Component)]
struct ErrorLabel;

pub fn plugin(app: &mut App) {
    app.init_resource::<MenuForm>()
        .add_systems(OnEnter(Screen::Menu), spawn_menu)
        .add_systems(OnExit(Screen::Menu), despawn_menu)
        .add_systems(
            Update,
            (typing, buttons, refresh).chain().run_if(in_state(Screen::Menu)),
        );
}

const IDLE: Color = Color::srgba(0.12, 0.14, 0.22, 0.92);
const FOCUS: Color = Color::srgba(0.2, 0.26, 0.42, 0.95);
const ACTION: Color = Color::srgba(0.15, 0.3, 0.2, 0.92);

fn spawn_menu(mut commands: Commands, mut hud: Query<&mut Text, With<HudText>>) {
    if let Ok(mut t) = hud.single_mut() {
        t.0.clear();
    }
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
            for field in [Field::Name, Field::Addr, Field::Code] {
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
            root.spawn((
                Text::new(""),
                ErrorLabel,
                TextFont { font_size: 15.0, ..default() },
                TextColor(Color::srgb(1.0, 0.55, 0.5)),
            ));
            root.spawn((
                Text::new("click a field to edit • Tab switches fields"),
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

fn typing(mut events: EventReader<KeyboardInput>, mut form: ResMut<MenuForm>) {
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        match &ev.logical_key {
            Key::Tab => {
                form.focus = match form.focus {
                    Field::Name => Field::Addr,
                    Field::Addr => Field::Code,
                    Field::Code => Field::Name,
                };
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
                    if buf.len() < 48 {
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
        Field::Code => &mut form.code,
    }
}

fn buttons(
    interactions: Query<
        (&Interaction, Option<&Field>, Option<&MenuAction>),
        (Changed<Interaction>, With<Button>),
    >,
    mut form: ResMut<MenuForm>,
    mut online: ResMut<Online>,
    mut next: ResMut<NextState<Screen>>,
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
            MenuAction::Create | MenuAction::Join => {
                if form.name.trim().is_empty() {
                    form.error = "enter a name first".into();
                    continue;
                }
                let hello = ClientMsg::Hello {
                    proto_version: sf_proto::PROTOCOL_VERSION,
                    name: form.name.trim().into(),
                    password: String::new(),
                };
                let second = match action {
                    MenuAction::Create => ClientMsg::CreateGame,
                    MenuAction::Join => {
                        if form.code.trim().is_empty() {
                            form.error = "enter the game code to join".into();
                            continue;
                        }
                        ClientMsg::JoinGame { code: form.code.trim().into() }
                    }
                    MenuAction::Sandbox => unreachable!(),
                };
                form.error.clear();
                *online = Online::default();
                online.status = format!("Connecting to {}…", form.addr.trim());
                online.net = Some(net::connect(form.addr.trim().into(), vec![hello, second]));
                next.set(Screen::Online);
            }
        }
    }
}

fn refresh(
    form: Res<MenuForm>,
    mut labels: Query<(&FieldLabel, &mut Text), Without<ErrorLabel>>,
    mut error: Query<&mut Text, With<ErrorLabel>>,
    mut fields: Query<(&Field, &mut BackgroundColor), With<Button>>,
) {
    for (label, mut text) in &mut labels {
        let (title, value) = match label.0 {
            Field::Name => ("Name", &form.name),
            Field::Addr => ("Server", &form.addr),
            Field::Code => ("Game code", &form.code),
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
