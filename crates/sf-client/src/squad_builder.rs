//! Squad builder screen: keyboard-driven grid of ships × slots with live
//! shared validation, the selected card's image (from a local card
//! directory) or its text, callsigns, and squads saved as RON in the
//! config directory. The built squad is what Create/Join send.

use std::collections::HashMap;
use std::path::PathBuf;

use bevy::asset::RenderAssetUsages;
use bevy::image::{CompressedImageFormats, ImageSampler, ImageType};
use bevy::input::ButtonState;
use bevy::input::keyboard::{Key, KeyboardInput};
use bevy::prelude::*;

use sf_core::pilot::PilotId;
use sf_core::ship::{Faction, ShipClass, ShipClassId};
use sf_core::squad::{Squad, SquadRules, SquadShip, validate_squad};
use sf_core::upgrade::{Slot, UpgradeEffect, UpgradeId};

use crate::render::{Game, HudText};
use crate::{Screen, pins};

/// One ship row: pilot plus a column per upgrade slot.
#[derive(Clone)]
struct Row {
    pilot: PilotId,
    slots: Vec<(Slot, Option<UpgradeId>)>,
    callsign: String,
}

#[derive(Resource)]
pub struct Builder {
    pub name: String,
    pub faction: Faction,
    rows: Vec<Row>,
    row: usize,
    /// 0 = pilot column, n = slot n-1.
    col: usize,
    typing: Option<Typing>,
    status: String,
    saved: Vec<String>,
    cards_dir: Option<PathBuf>,
    cache: HashMap<String, Option<Handle<Image>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Typing {
    Callsign,
    Name,
}

#[derive(Component)]
struct BuilderTag;

#[derive(Component)]
struct CardImage;

impl Default for Builder {
    fn default() -> Self {
        Self {
            name: "My Squad".into(),
            faction: Faction::RebelAlliance,
            rows: Vec::new(),
            row: 0,
            col: 0,
            typing: None,
            status: String::new(),
            saved: Vec::new(),
            cards_dir: find_cards_dir(),
            cache: HashMap::new(),
        }
    }
}

impl Builder {
    /// The squad as the server will see it.
    pub fn squad(&self) -> Squad {
        Squad {
            name: self.name.clone(),
            faction: self.faction,
            ships: self
                .rows
                .iter()
                .map(|r| SquadShip {
                    pilot: r.pilot,
                    upgrades: r.slots.iter().filter_map(|(_, u)| *u).collect(),
                    callsign: r.callsign.clone(),
                })
                .collect(),
        }
    }

    /// Squad to send with Create/Join: the built one if it validates,
    /// else None (server picks its basic fleet).
    pub fn squad_for_play(&self, game: &Game) -> Option<Squad> {
        let s = self.squad();
        (!s.ships.is_empty() && validate_squad(&s, &game.content, &SquadRules::default()).is_ok())
            .then_some(s)
    }

    pub fn summary(&self, game: &Game) -> String {
        let s = self.squad();
        if s.ships.is_empty() {
            return "Squad: none built — the server's basic fleet will be used".into();
        }
        match validate_squad(&s, &game.content, &SquadRules::default()) {
            Ok(v) => format!("Squad: {} — {} ships, {} pts", s.name, s.ships.len(), v.points),
            Err(e) => format!("Squad: {} — INVALID ({}), basic fleet will be used", s.name, e[0]),
        }
    }

    /// Re-scan the saved squads directory.
    pub fn refresh_saved(&mut self) {
        self.saved = list_saved();
    }

    /// Names of the saved squads and which one is current (if any).
    pub fn saved_names(&self) -> (&[String], Option<usize>) {
        let cur = self.saved.iter().position(|s| *s == sanitize(&self.name));
        (&self.saved, cur)
    }

    /// Load the previous/next saved squad (wrapping); it becomes the
    /// current squad for the next game.
    pub fn cycle_saved(&mut self, game: &Game, step: i32) -> String {
        if self.saved.is_empty() {
            return format!("no saved squads in {}", squads_dir().display());
        }
        let n = self.saved.len() as i32;
        let next = match self.saved_names().1.map(|i| i as i32) {
            Some(i) => (i + step).rem_euclid(n),
            None if step >= 0 => 0,
            None => n - 1,
        } as usize;
        let name = self.saved[next].clone();
        match read_squad(&squads_dir().join(format!("{name}.ron"))) {
            Ok(s) => {
                self.load_squad(game, s);
                let _ = write_squad(&current_path(), &self.squad());
                format!("squad {name} ({} of {n})", next + 1)
            }
            Err(e) => format!("load {name} failed: {e}"),
        }
    }

    fn load_squad(&mut self, game: &Game, s: Squad) {
        self.name = s.name;
        self.faction = s.faction;
        self.rows = s
            .ships
            .iter()
            .map(|e| {
                let mut r = Row { pilot: e.pilot, slots: Vec::new(), callsign: e.callsign.clone() };
                rebuild_slots(&mut r, game);
                // Place each upgrade in the first free matching column.
                for u in &e.upgrades {
                    if let Some(card) = game.content.upgrades.upgrade(*u)
                        && let Some(cell) =
                            r.slots.iter_mut().find(|(s, v)| *s == card.slot && v.is_none())
                    {
                        cell.1 = Some(*u);
                    }
                }
                r
            })
            .collect();
        self.row = 0;
        self.col = 0;
    }
}

/// Where card scans live: $STARFIGHT_CARDS, the dev-tree reference clone,
/// or <config>/cards. None → text-only builder.
fn find_cards_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(p) = std::env::var_os("STARFIGHT_CARDS") {
        candidates.push(PathBuf::from(p));
    }
    candidates.push(PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../reference/xwing-card-images/images"
    )));
    candidates.push(PathBuf::from("reference/xwing-card-images/images"));
    candidates.push(pins::config_dir().join("cards"));
    candidates.into_iter().find(|p| p.join("pilots").is_dir())
}

fn squads_dir() -> PathBuf {
    pins::config_dir().join("squads")
}

/// The squad that will be flown next: restored at startup so a player
/// builds and saves once, then just plays.
fn current_path() -> PathBuf {
    pins::config_dir().join("current_squad.ron")
}

fn read_squad(path: &PathBuf) -> Result<Squad, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    ron::from_str::<Squad>(&text).map_err(|e| e.to_string())
}

fn write_squad(path: &PathBuf, squad: &Squad) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let text = ron::ser::to_string_pretty(squad, ron::ser::PrettyConfig::default())
        .map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| e.to_string())
}

fn list_saved() -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(squads_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().into_owned()))
                .filter(|_| true)
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Slot columns for a row: printed bar, talent (pilot or R2-D6), then
/// the implicit modification and title. Existing picks are kept where
/// their slot survives.
fn rebuild_slots(r: &mut Row, game: &Game) {
    let content = &game.content;
    let Some(pilot) = content.pilots.pilot(r.pilot) else { return };
    let Some(class) = content.ships.class(pilot.class) else { return };
    let mut slots: Vec<Slot> = class.upgrade_bar.clone();
    let granted_talent = r
        .slots
        .iter()
        .filter_map(|(_, u)| *u)
        .filter_map(|u| content.upgrades.upgrade(u))
        .any(|u| u.effect == Some(UpgradeEffect::BarGainsTalent));
    if pilot.talent_slot || granted_talent {
        slots.push(Slot::Talent);
    }
    slots.extend(Slot::implicit());
    let old = std::mem::take(&mut r.slots);
    let mut picks: Vec<Option<UpgradeId>> = old.iter().map(|(_, u)| *u).collect();
    r.slots = slots
        .into_iter()
        .map(|slot| {
            let pick = picks.iter_mut().find_map(|p| {
                let u = (*p)?;
                let fits = content.upgrades.upgrade(u).map(|c| c.slot == slot).unwrap_or(false);
                if fits { p.take() } else { None }
            });
            (slot, pick)
        })
        .collect();
}

fn classes_of(game: &Game, faction: Faction) -> Vec<&ShipClass> {
    game.ships.classes.iter().filter(|c| c.faction == faction).collect()
}

/// Upgrade choices for a column: None plus every card of that slot the
/// ship may legally equip (validated against the rest of the squad).
fn choices_for(b: &Builder, game: &Game, row: usize, col: usize) -> Vec<Option<UpgradeId>> {
    let slot = b.rows[row].slots[col - 1].0;
    let mut out = vec![None];
    for card in game.content.upgrades.by_slot(slot) {
        let mut trial = b.rows.clone();
        trial[row].slots[col - 1].1 = Some(card.id);
        let mut t = Builder {
            rows: trial,
            cache: HashMap::new(),
            cards_dir: None,
            ..Builder { name: b.name.clone(), faction: b.faction, ..Default::default() }
        };
        t.rows[row].slots[col - 1].1 = Some(card.id);
        let squad = t.squad();
        let rules = SquadRules { max_points: u32::MAX, ..Default::default() };
        let ok = match validate_squad(&squad, &game.content, &rules) {
            Ok(_) => true,
            Err(errs) => !errs.iter().any(|e| {
                use sf_core::squad::SquadError as E;
                matches!(e, E::NoSlot { ship, .. } | E::Restricted { ship, .. } | E::LimitedTwice { ship, .. } if *ship == row)
                    || matches!(e, E::DuplicateUnique(_))
            }),
        };
        if ok {
            out.push(Some(card.id));
        }
    }
    out
}

pub fn plugin(app: &mut App) {
    app.init_resource::<Builder>()
        .add_systems(Startup, restore_current)
        .add_systems(OnEnter(Screen::Squad), enter)
        .add_systems(OnExit(Screen::Squad), exit)
        .add_systems(Update, (typing, input, show).chain().run_if(in_state(Screen::Squad)));
}

fn enter(mut commands: Commands, mut b: ResMut<Builder>) {
    b.refresh_saved();
    b.typing = None;
    b.status = match &b.cards_dir {
        Some(d) => format!("card images: {}", d.display()),
        None => {
            "no card images found (set STARFIGHT_CARDS or clone xwing-card-images into reference/)"
                .into()
        }
    };
    commands.spawn((
        BuilderTag,
        CardImage,
        ImageNode::default(),
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(12.0),
            top: Val::Px(12.0),
            width: Val::Px(300.0),
            height: Val::Px(418.0),
            ..default()
        },
        Visibility::Hidden,
    ));
}

fn exit(mut commands: Commands, tagged: Query<Entity, With<BuilderTag>>, b: Res<Builder>) {
    for e in &tagged {
        commands.entity(e).despawn();
    }
    // Whatever is in the builder when leaving is the squad for the next
    // game, and it survives restarts.
    let _ = write_squad(&current_path(), &b.squad());
}

/// Startup: reload the last squad so the menu is ready to play with it.
fn restore_current(mut b: ResMut<Builder>, game: Res<Game>) {
    if let Ok(s) = read_squad(&current_path()) {
        b.load_squad(&game, s);
    }
}

fn typing(mut b: ResMut<Builder>, mut events: EventReader<KeyboardInput>) {
    let Some(mode) = b.typing else {
        events.clear();
        return;
    };
    let max = if mode == Typing::Name { 32 } else { sf_core::ship::CALLSIGN_MAX };
    for ev in events.read() {
        if ev.state != ButtonState::Pressed {
            continue;
        }
        let row = b.row;
        let buf: &mut String = match mode {
            Typing::Name => &mut b.name,
            Typing::Callsign => match b.rows.get_mut(row) {
                Some(r) => &mut r.callsign,
                None => continue,
            },
        };
        match &ev.logical_key {
            Key::Enter | Key::Escape => {
                b.typing = None;
                return;
            }
            Key::Backspace => {
                buf.pop();
            }
            Key::Space => buf.push(' '),
            Key::Character(s) => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    if buf.chars().count() < max {
                        buf.push(c);
                    }
                }
            }
            _ => {}
        }
    }
}

fn input(
    mut b: ResMut<Builder>,
    game: Res<Game>,
    keys: Res<ButtonInput<KeyCode>>,
    mut next: ResMut<NextState<Screen>>,
) {
    if b.typing.is_some() {
        return;
    }
    if keys.just_pressed(KeyCode::Escape) || keys.just_pressed(KeyCode::Enter) {
        next.set(Screen::Menu);
        return;
    }
    // Faction: only switchable while empty (or clears the squad).
    if keys.just_pressed(KeyCode::KeyF) {
        b.faction = match b.faction {
            Faction::RebelAlliance => Faction::Empire,
            Faction::Empire => Faction::RebelAlliance,
        };
        b.rows.clear();
        b.row = 0;
        b.col = 0;
    }
    // Add a ship of the n-th class of this faction.
    let classes = classes_of(&game, b.faction);
    for (n, key) in
        [KeyCode::Digit1, KeyCode::Digit2, KeyCode::Digit3, KeyCode::Digit4].into_iter().enumerate()
    {
        if keys.just_pressed(key)
            && let Some(class) = classes.get(n)
            && let Some(basic) = game.content.pilots.basic_for(class.id)
        {
            let mut r = Row { pilot: basic.id, slots: Vec::new(), callsign: String::new() };
            rebuild_slots(&mut r, &game);
            b.rows.push(r);
            b.row = b.rows.len() - 1;
            b.col = 0;
        }
    }
    if keys.just_pressed(KeyCode::Delete) && !b.rows.is_empty() {
        let i = b.row;
        b.rows.remove(i);
        b.row = b.row.min(b.rows.len().saturating_sub(1));
        b.col = 0;
    }
    if keys.just_pressed(KeyCode::ArrowUp) && b.row > 0 {
        b.row -= 1;
        b.col = 0;
    }
    if keys.just_pressed(KeyCode::ArrowDown) && b.row + 1 < b.rows.len() {
        b.row += 1;
        b.col = 0;
    }
    if !b.rows.is_empty() {
        let ncols = b.rows[b.row].slots.len() + 1;
        if keys.just_pressed(KeyCode::ArrowLeft) && b.col > 0 {
            b.col -= 1;
        }
        if keys.just_pressed(KeyCode::ArrowRight) && b.col + 1 < ncols {
            b.col += 1;
        }
        // Q / E cycle the choice in the selected column.
        let step: i32 = i32::from(keys.just_pressed(KeyCode::KeyE))
            - i32::from(keys.just_pressed(KeyCode::KeyQ));
        if step != 0 {
            let (row, col) = (b.row, b.col);
            if col == 0 {
                let class = game.content.pilots.pilot(b.rows[row].pilot).map(|p| p.class);
                let roster: Vec<PilotId> = game
                    .content
                    .pilots
                    .roster(class.unwrap_or(ShipClassId(0)), None)
                    .iter()
                    .map(|p| p.id)
                    .collect();
                if let Some(i) = roster.iter().position(|p| *p == b.rows[row].pilot) {
                    let j = (i as i32 + step).rem_euclid(roster.len() as i32) as usize;
                    b.rows[row].pilot = roster[j];
                    let mut r = b.rows[row].clone();
                    rebuild_slots(&mut r, &game);
                    b.rows[row] = r;
                }
            } else {
                let choices = choices_for(&b, &game, row, col);
                let cur = b.rows[row].slots[col - 1].1;
                let i = choices.iter().position(|c| *c == cur).unwrap_or(0);
                let j = (i as i32 + step).rem_euclid(choices.len() as i32) as usize;
                b.rows[row].slots[col - 1].1 = choices[j];
                let mut r = b.rows[row].clone();
                rebuild_slots(&mut r, &game);
                b.rows[row] = r;
            }
        }
        if keys.just_pressed(KeyCode::KeyN) {
            b.typing = Some(Typing::Callsign);
        }
    }
    if keys.just_pressed(KeyCode::KeyM) {
        b.typing = Some(Typing::Name);
    }
    if keys.just_pressed(KeyCode::KeyS) {
        let squad = b.squad();
        let file = squads_dir().join(format!("{}.ron", sanitize(&squad.name)));
        b.status = match write_squad(&file, &squad) {
            Ok(()) => format!("saved {}", file.display()),
            Err(e) => format!("save failed: {e}"),
        };
        let _ = write_squad(&current_path(), &squad);
        b.saved = list_saved();
    }
    if keys.just_pressed(KeyCode::KeyL) {
        let msg = b.cycle_saved(&game, 1);
        b.status = format!("{msg} — L again for the next");
    }
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.is_empty() { "squad".into() } else { s }
}

/// Load (and cache) a card image from the cards directory.
fn card_handle(b: &mut Builder, images: &mut Assets<Image>, rel: &str) -> Option<Handle<Image>> {
    if let Some(h) = b.cache.get(rel) {
        return h.clone();
    }
    let handle = b.cards_dir.as_ref().and_then(|dir| {
        let png = dir.join(rel);
        let path = if png.is_file() { png } else { dir.join(rel.replace(".png", ".jpg")) };
        let bytes = std::fs::read(&path).ok()?;
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        Image::from_buffer(
            &bytes,
            ImageType::Extension(&ext),
            CompressedImageFormats::NONE,
            true,
            ImageSampler::Default,
            RenderAssetUsages::RENDER_WORLD,
        )
        .ok()
        .map(|img| images.add(img))
    });
    b.cache.insert(rel.to_string(), handle.clone());
    handle
}

fn show(
    mut b: ResMut<Builder>,
    game: Res<Game>,
    mut images: ResMut<Assets<Image>>,
    mut hud: Query<&mut Text, With<HudText>>,
    mut card: Query<(&mut ImageNode, &mut Visibility), With<CardImage>>,
) {
    let content = &game.content;
    let squad = b.squad();
    let verdict = validate_squad(&squad, content, &SquadRules::default());
    let points = squad.cost(content);
    let faction = match b.faction {
        Faction::RebelAlliance => "Rebel Alliance / Resistance",
        Faction::Empire => "Galactic Empire / First Order",
    };
    let mut lines = vec![
        format!("SQUAD BUILDER — {} [{faction}] — {points} / 100 pts", b.name),
        classes_of(&game, b.faction)
            .iter()
            .enumerate()
            .map(|(n, c)| format!("{}: add {}", n + 1, c.name))
            .collect::<Vec<_>>()
            .join("   ")
            + "   F: faction   Delete: remove ship",
        "↑/↓ ship  ←/→ column  Q/E change  N: callsign  M: squad name  S: save  L: load next  Esc: back".into(),
        String::new(),
    ];
    for (i, r) in b.rows.iter().enumerate() {
        let pilot = content.pilots.pilot(r.pilot);
        let class = pilot.and_then(|p| content.ships.class(p.class));
        let sel = i == b.row;
        let mut cells: Vec<String> = Vec::new();
        cells.push(format!(
            "{}{} PS{} ({})",
            if sel && b.col == 0 { "▶" } else { " " },
            pilot.map(|p| p.name.as_str()).unwrap_or("?"),
            pilot.map(|p| p.skill).unwrap_or(0),
            pilot.map(|p| p.cost).unwrap_or(0)
        ));
        for (k, (slot, pick)) in r.slots.iter().enumerate() {
            let mark = if sel && b.col == k + 1 { "▶" } else { " " };
            let text = match pick.and_then(|u| content.upgrades.upgrade(u)) {
                Some(u) => format!("{} ({})", u.name, u.cost),
                None => format!("[{slot:?}]"),
            };
            cells.push(format!("{mark}{text}"));
        }
        let callsign = if r.callsign.is_empty() {
            "(default callsign)".to_string()
        } else {
            r.callsign.clone()
        };
        lines.push(format!(
            "{} {} — {}  |  {}",
            if sel { ">" } else { " " },
            class.map(|c| c.name.as_str()).unwrap_or("?"),
            callsign,
            cells.join("  ")
        ));
    }
    if b.rows.is_empty() {
        lines.push("(no ships — press 1-3 to add)".into());
    }
    lines.push(String::new());
    match &verdict {
        Ok(v) => lines.push(format!("VALID — {} points", v.points)),
        Err(errs) => {
            for e in errs {
                lines.push(format!("!! {e}"));
            }
        }
    }
    if let Some(t) = b.typing {
        let what = match t {
            Typing::Callsign => "CALLSIGN",
            Typing::Name => "SQUAD NAME",
        };
        lines.push(format!("{what}: type, Enter/Esc to finish"));
    }
    lines.push(b.status.clone());

    // Selected card: image if available, text always.
    let mut rel: Option<String> = None;
    if let Some(r) = b.rows.get(b.row) {
        if b.col == 0 {
            if let Some(p) = content.pilots.pilot(r.pilot) {
                rel = content.pilots.card_image(&content.ships, p.id);
                let ability =
                    p.ability.map(|a| format!("{a:?}")).unwrap_or_else(|| "no ability".into());
                lines.push(String::new());
                lines.push(format!(
                    "{} — PS{} — {} pts — {:?} — {ability}",
                    p.name, p.skill, p.cost, p.source
                ));
            }
        } else if let Some((slot, pick)) = r.slots.get(b.col - 1) {
            lines.push(String::new());
            match pick.and_then(|u| content.upgrades.upgrade(u)) {
                Some(u) => {
                    rel = content.upgrades.card_image(u.id);
                    lines.push(format!("{} ({:?}, {} pts): {}", u.name, u.slot, u.cost, u.text));
                }
                None => lines.push(format!("{slot:?} slot: empty — Q/E to choose a card")),
            }
        }
    }
    let handle = rel.and_then(|r| card_handle(&mut b, &mut images, &r));
    if let Ok((mut node, mut vis)) = card.single_mut() {
        match handle {
            Some(h) => {
                node.image = h;
                *vis = Visibility::Visible;
            }
            None => *vis = Visibility::Hidden,
        }
    }
    if let Ok(mut t) = hud.single_mut() {
        t.0 = lines.join("\n");
    }
}
