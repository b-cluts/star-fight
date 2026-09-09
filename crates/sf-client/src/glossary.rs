//! In-game glossary: an overlay reachable from every screen (F1 or the
//! "?" button) listing ship classes, pilots, upgrade cards, tokens and
//! actions, damage cards and rules terms with their full text. Pure UI —
//! it never touches game state, and the game's own key handling is paused
//! while it is open (see `closed`).

use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use sf_core::data::GlossaryCategory;
use sf_core::maneuver::Difficulty;

use crate::render::{self, Game};

const CATS: [&str; 6] = ["Ships", "Pilots", "Upgrades", "Tokens & Actions", "Damage", "Rules"];
/// Entry rows shown at once.
const ROWS: usize = 16;

#[derive(Resource, Default)]
pub struct Glossary {
    pub open: bool,
    cat: usize,
    filter: String,
    cursor: usize,
}

#[derive(Component)]
struct Panel;
#[derive(Component)]
struct PanelText;
#[derive(Component)]
struct HelpButton;

/// Run condition for the screens' own key handling.
pub fn closed(g: Res<Glossary>) -> bool {
    !g.open
}

pub fn plugin(app: &mut App) {
    app.init_resource::<Glossary>()
        .add_systems(Startup, spawn)
        .add_systems(Update, (button, show))
        // After every screen's input for the frame, so the Esc that closes
        // the glossary is never also seen by a screen as "leave".
        .add_systems(PostUpdate, input);
}

fn spawn(mut commands: Commands) {
    commands
        .spawn((
            Button,
            HelpButton,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(10.0),
                bottom: Val::Px(10.0),
                padding: UiRect::axes(Val::Px(10.0), Val::Px(4.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.12, 0.14, 0.22, 0.9)),
            GlobalZIndex(5),
        ))
        .with_children(|b| {
            b.spawn((
                Text::new("? glossary (F1)"),
                TextFont { font_size: 13.0, ..default() },
                TextColor(Color::srgb(0.85, 0.85, 0.95)),
            ));
        });
    commands
        .spawn((
            Panel,
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(3.0),
                top: Val::Percent(3.0),
                width: Val::Percent(94.0),
                height: Val::Percent(94.0),
                padding: UiRect::all(Val::Px(14.0)),
                overflow: Overflow::clip(),
                ..default()
            },
            BackgroundColor(Color::srgba(0.03, 0.04, 0.08, 0.97)),
            Visibility::Hidden,
            GlobalZIndex(10),
        ))
        .with_children(|p| {
            p.spawn((
                PanelText,
                Text::new(""),
                TextFont { font_size: 14.0, ..default() },
                TextColor(Color::srgb(0.9, 0.9, 0.9)),
                TextLayout::new_with_linebreak(LineBreak::WordOrCharacter),
                Node { width: Val::Percent(100.0), ..default() },
            ));
        });
}

fn button(
    mut g: ResMut<Glossary>,
    q: Query<&Interaction, (Changed<Interaction>, With<HelpButton>)>,
) {
    for i in &q {
        if *i == Interaction::Pressed {
            g.open = !g.open;
        }
    }
}

fn input(
    mut g: ResMut<Glossary>,
    keys: Res<ButtonInput<KeyCode>>,
    mut events: EventReader<KeyboardInput>,
    game: Res<Game>,
) {
    if keys.just_pressed(KeyCode::F1) {
        g.open = !g.open;
        events.clear();
        return;
    }
    if !g.open {
        events.clear();
        return;
    }
    let len = entries(&game, g.cat, &g.filter).len();
    let last = len.saturating_sub(1);
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        match &ev.logical_key {
            Key::Escape => g.open = false,
            Key::ArrowLeft => {
                g.cat = (g.cat + CATS.len() - 1) % CATS.len();
                g.cursor = 0;
            }
            Key::ArrowRight | Key::Tab => {
                g.cat = (g.cat + 1) % CATS.len();
                g.cursor = 0;
            }
            Key::ArrowUp => g.cursor = g.cursor.saturating_sub(1),
            Key::ArrowDown => g.cursor = (g.cursor + 1).min(last),
            Key::PageUp => g.cursor = g.cursor.saturating_sub(ROWS),
            Key::PageDown => g.cursor = (g.cursor + ROWS).min(last),
            Key::Home => g.cursor = 0,
            Key::End => g.cursor = last,
            Key::Backspace => {
                g.filter.pop();
                g.cursor = 0;
            }
            Key::Space => {
                g.filter.push(' ');
                g.cursor = 0;
            }
            Key::Character(s) => {
                let s: String = s.chars().filter(|c| !c.is_control()).collect();
                if !s.is_empty() && g.filter.len() < 40 {
                    g.filter.push_str(&s);
                    g.cursor = 0;
                }
            }
            _ => {}
        }
    }
}

/// (list line, detail text) for the current tab, filtered.
fn entries(game: &Game, cat: usize, filter: &str) -> Vec<(String, String)> {
    let content = &game.content;
    let mut out: Vec<(String, String)> = match cat {
        0 => content
            .ships
            .classes
            .iter()
            .map(|c| {
                let dial = game
                    .dial(c)
                    .iter()
                    .map(|m| {
                        format!(
                            "{} {} ({})",
                            render::steer_name(m.steer),
                            m.distance,
                            colour(m.difficulty)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let actions =
                    c.action_bar.iter().map(|a| format!("{a:?}")).collect::<Vec<_>>().join(", ");
                let slots =
                    c.upgrade_bar.iter().map(|s| format!("{s:?}")).collect::<Vec<_>>().join(", ");
                let turret = if c.turret_primary { " (turret: fires all round)" } else { "" };
                (
                    format!("{} — {:?}, {:?} base", c.name, c.faction, c.size),
                    format!(
                        "Attack {}{turret}, agility {}, hull {}, shields {}.\nBase {} x {} units.\nActions: {actions}.\nUpgrade slots: {slots} (plus Modification and Title).\nDial: {dial}.",
                        c.attack_dice,
                        c.agility,
                        c.hull,
                        c.shields,
                        c.footprint.length,
                        c.footprint.width,
                    ),
                )
            })
            .collect(),
        1 => {
            let mut pilots: Vec<_> = content.pilots.pilots.iter().collect();
            pilots.sort_by_key(|a| (a.class.0, std::cmp::Reverse(a.skill)));
            pilots
                .into_iter()
                .map(|p| {
                    let class = content.ships.class(p.class).map(|c| c.name.as_str()).unwrap_or("?");
                    let ability = match p.ability {
                        Some(a) => format!("{}\n{}", a.text(), automated(a.implemented())),
                        None => "No pilot ability.".to_string(),
                    };
                    let stats = p
                        .stats
                        .map(|s| {
                            format!(
                                "\nPrinted stats: attack {}, agility {}, hull {}, shields {}.",
                                s.attack, s.agility, s.hull, s.shields
                            )
                        })
                        .unwrap_or_default();
                    (
                        format!(
                            "{} — {class} PS{} {} pts{}",
                            p.name,
                            p.skill,
                            p.cost,
                            if p.unique { " (unique)" } else { "" }
                        ),
                        format!(
                            "{class}, pilot skill {}, {} points, {:?}{}.{stats}\n{ability}",
                            p.skill,
                            p.cost,
                            p.source,
                            if p.talent_slot { ", talent slot" } else { "" }
                        ),
                    )
                })
                .collect()
        }
        2 => {
            let mut cards: Vec<_> = content.upgrades.upgrades.iter().collect();
            cards.sort_by(|a, b| format!("{:?}", a.slot).cmp(&format!("{:?}", b.slot)).then(a.name.cmp(&b.name)));
            cards
                .into_iter()
                .map(|u| {
                    let attack = u
                        .attack
                        .map(|a| {
                            format!(
                                "\nAttack: {} dice, Range {}-{}, requires {:?}{}.",
                                a.dice,
                                a.range_min,
                                a.range_max,
                                a.requires,
                                if a.discard_to_fire { ", discarded when fired" } else { "" }
                            )
                        })
                        .unwrap_or_default();
                    let restrictions = if u.restrictions.is_empty() {
                        String::new()
                    } else {
                        format!("\nRestrictions: {:?}.", u.restrictions)
                    };
                    let flags = format!(
                        "{}{}",
                        if u.unique { " (unique)" } else { "" },
                        if u.limited { " (limited)" } else { "" }
                    );
                    (
                        format!("{} — {:?} {} pts{flags}", u.name, u.slot, u.cost),
                        format!(
                            "{:?} slot, {} points.{restrictions}{attack}\n{}\n{}",
                            u.slot,
                            u.cost,
                            u.text,
                            automated(u.effect.is_some_and(|e| e.implemented()))
                        ),
                    )
                })
                .collect()
        }
        _ => {
            let wanted: &[GlossaryCategory] = match cat {
                3 => &[GlossaryCategory::Tokens, GlossaryCategory::Actions],
                4 => &[GlossaryCategory::Damage],
                _ => &[GlossaryCategory::Rules],
            };
            let mut list: Vec<(String, String)> = content
                .glossary
                .entries
                .iter()
                .filter(|e| wanted.contains(&e.category))
                .map(|e| (e.name.clone(), e.text.clone()))
                .collect();
            if wanted.contains(&GlossaryCategory::Rules) {
                use sf_core::mission::MissionKind;
                use sf_core::ship::Faction;
                for kind in MissionKind::ALL {
                    list.push((
                        format!("Mission {}: {}", kind.number(), kind.name()),
                        format!(
                            "{} REBEL VICTORY: {} IMPERIAL VICTORY: {}",
                            kind.rules_text(),
                            kind.objective(Faction::RebelAlliance),
                            kind.objective(Faction::Empire)
                        ),
                    ));
                }
            }
            list
        }
    };
    let f = filter.trim().to_lowercase();
    if !f.is_empty() {
        out.retain(|(name, text)| {
            name.to_lowercase().contains(&f) || text.to_lowercase().contains(&f)
        });
    }
    out
}

fn colour(d: Difficulty) -> &'static str {
    match d {
        Difficulty::Easy => "green",
        Difficulty::Normal => "white",
        Difficulty::Hard => "red",
    }
}

fn automated(yes: bool) -> &'static str {
    if yes {
        "[automated by the rules engine]"
    } else {
        "[NOT yet automated — apply by agreement]"
    }
}

fn show(
    g: Res<Glossary>,
    game: Res<Game>,
    mut panel: Query<&mut Visibility, With<Panel>>,
    mut text: Query<&mut Text, With<PanelText>>,
) {
    let Ok(mut vis) = panel.single_mut() else { return };
    *vis = if g.open { Visibility::Visible } else { Visibility::Hidden };
    if !g.open {
        return;
    }
    let Ok(mut t) = text.single_mut() else { return };
    let tabs: Vec<String> = CATS
        .iter()
        .enumerate()
        .map(|(i, c)| if i == g.cat { format!("[{c}]") } else { format!(" {c} ") })
        .collect();
    let list = entries(&game, g.cat, &g.filter);
    let cursor = g.cursor.min(list.len().saturating_sub(1));
    let mut lines = vec![
        format!("GLOSSARY   {}", tabs.join("  ")),
        "Left/Right: tab • Up/Down, PageUp/PageDown: select • type to filter, Backspace clears • Esc / F1: close".into(),
        format!("filter: {}_    ({} entries)", g.filter, list.len()),
        String::new(),
    ];
    if list.is_empty() {
        lines.push("(nothing matches)".into());
    } else {
        let start = cursor.saturating_sub(ROWS / 2).min(list.len().saturating_sub(ROWS));
        for (i, (name, _)) in list.iter().enumerate().skip(start).take(ROWS) {
            lines.push(format!("{} {name}", if i == cursor { ">" } else { " " }));
        }
        if start + ROWS < list.len() {
            lines.push(format!("  … {} more", list.len() - start - ROWS));
        }
        lines.push(String::new());
        lines.push("────────────────────────────────────────".into());
        lines.push(list[cursor].0.clone());
        lines.push(list[cursor].1.clone());
    }
    t.0 = lines.join("\n");
}
