use serde::{Deserialize, Serialize};

use crate::action::{ActionKind, PlannedAction};
use crate::geometry::{Footprint, Pose};
use crate::maneuver::ManeuverSetId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShipClassId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShipId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerId(pub u32);

/// Faction, cosmetic for now (laser bolt colors: Rebel Alliance red,
/// Empire green) — squad building will restrict by faction later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Faction {
    RebelAlliance,
    Empire,
}

impl Faction {
    /// XWS faction directory name (card image layout).
    pub fn xws(self) -> &'static str {
        match self {
            Faction::RebelAlliance => "rebels",
            Faction::Empire => "imperial",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeClass {
    /// 40 × 40 mm base — 1 × 1 game units (matches a speed-1 straight).
    Small,
    /// 60 × 60 mm base — 1.5 × 1.5 game units.
    Medium,
    /// 80 × 80 mm base — 2 × 2 game units (matches a speed-2 straight).
    Large,
    /// 80 × 192 mm base — 2 wide × 4.8 long game units.
    Huge,
}

impl SizeClass {
    /// Standard base footprint (length along heading × width) in game units.
    pub fn base_footprint(self) -> Footprint {
        match self {
            SizeClass::Small => Footprint { length: 1.0, width: 1.0 },
            SizeClass::Medium => Footprint { length: 1.5, width: 1.5 },
            SizeClass::Large => Footprint { length: 2.0, width: 2.0 },
            SizeClass::Huge => Footprint { length: 4.8, width: 2.0 },
        }
    }
}

/// A kind of ship, loaded from `assets/data/ships.ron`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipClass {
    pub id: ShipClassId,
    pub name: String,
    /// XWS ship identifier (e.g. "tiefighter", "t70xwing").
    pub xws: String,
    pub faction: Faction,
    pub size: SizeClass,
    pub footprint: Footprint,
    pub maneuver_set: ManeuverSetId,
    /// Primary weapon attack dice at standard range (front 90° arc).
    pub attack_dice: u8,
    /// Defense (agility) dice rolled against attacks.
    pub agility: u8,
    /// Hull points. Damage past the shields comes here, and only hull
    /// damage can inflict critical effects.
    pub hull: u8,
    /// Shield points: absorbed first, and while any remain the ship
    /// cannot suffer critical hits.
    pub shields: u8,
    /// Actions this ship may perform (one per turn, after moving).
    pub action_bar: Vec<ActionKind>,
    /// Board sprite asset path (orthographic top-down), e.g. "ships/scout.png".
    pub sprite: String,
    /// Sprite image dimensions (width, height) in pixels.
    pub sprite_px: (u32, u32),
    /// Pixel coordinates of the front-center point in the sprite.
    pub anchor_px: (u32, u32),
    /// Optional showcase art for UI (fleet panel, ship selection) — may be
    /// a perspective render; never used on the board.
    #[serde(default)]
    pub portrait: Option<String>,
}

/// One ship in play — runtime state owned by `game::GameState`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipState {
    pub id: ShipId,
    pub owner: PlayerId,
    pub class: ShipClassId,
    /// Pilot card flying this ship (skill, cost, ability).
    pub pilot: crate::pilot::PilotId,
    /// Squad callsign, e.g. "Red-leader", "Obsidian-2". Server-assigned
    /// default; the owner may rename during Placement.
    pub callsign: String,
    /// None until placed during the Placement phase.
    pub pose: Option<Pose>,
    pub hull: u8,
    pub shields: u8,
    pub stress: u8,
    /// Secretly planned maneuver (index into the ship's dial) — never
    /// revealed to the opponent before resolution.
    pub plan: Option<u8>,
    /// Secretly planned action, executed right after the maneuver
    /// (defaults to Pass at commit if unset).
    pub planned_action: Option<PlannedAction>,
    /// Focus tokens (public). Cleared in the End phase.
    pub focus: u8,
    /// Evade tokens (public). Cleared in the End phase.
    pub evade: u8,
    /// Acquired target lock (public). Persists until re-locked or spent.
    pub lock: Option<ShipId>,
    /// Active (faceup) critical effects — public information.
    pub crits: Vec<crate::crit::CritEffect>,
    pub destroyed: bool,
}

/// Longest accepted callsign (characters).
pub const CALLSIGN_MAX: usize = 20;

/// Squad name for the `nth` squad (0-based) of a faction in a game: the
/// first Rebel squad is Red, a second one Gold; Imperial squadrons are
/// Obsidian then Onyx — so mirror matches stay distinguishable while a
/// lone squad always gets the classic name.
pub fn default_squad_name(faction: Faction, nth: usize) -> &'static str {
    match (faction, nth % 2) {
        (Faction::Empire, 0) => "Obsidian",
        (Faction::Empire, _) => "Onyx",
        (Faction::RebelAlliance, 0) => "Red",
        (Faction::RebelAlliance, _) => "Gold",
    }
}

/// One squad name per seat, given each seat's faction.
pub fn squad_names(factions: &[Faction]) -> Vec<&'static str> {
    factions
        .iter()
        .enumerate()
        .map(|(seat, f)| {
            let nth = factions[..seat].iter().filter(|g| *g == f).count();
            default_squad_name(*f, nth)
        })
        .collect()
}

/// `n`-th ship (0-based) of a squad: the first is the leader, the rest
/// are numbered from 2.
pub fn default_callsign(squad: &str, n: usize) -> String {
    if n == 0 { format!("{squad}-leader") } else { format!("{squad}-{}", n + 1) }
}

/// Trim and validate a player-typed callsign: 1..=CALLSIGN_MAX printable
/// characters, no control characters.
pub fn validate_callsign(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("callsign cannot be empty".into());
    }
    if s.chars().count() > CALLSIGN_MAX {
        return Err(format!("callsign longer than {CALLSIGN_MAX} characters"));
    }
    if s.chars().any(|c| c.is_control()) {
        return Err("callsign contains control characters".into());
    }
    Ok(s.to_string())
}

#[cfg(test)]
mod callsign_tests {
    use super::*;

    #[test]
    fn defaults_name_leader_then_numbers() {
        assert_eq!(default_callsign("Red", 0), "Red-leader");
        assert_eq!(default_callsign("Red", 1), "Red-2");
        assert_eq!(default_squad_name(Faction::Empire, 0), "Obsidian");
        assert_eq!(default_squad_name(Faction::RebelAlliance, 1), "Gold");
        assert_eq!(squad_names(&[Faction::Empire, Faction::RebelAlliance]), ["Obsidian", "Red"]);
        assert_eq!(squad_names(&[Faction::RebelAlliance, Faction::RebelAlliance]), ["Red", "Gold"]);
    }

    #[test]
    fn validation_trims_and_bounds() {
        assert_eq!(validate_callsign("  Rogue-3 ").unwrap(), "Rogue-3");
        assert!(validate_callsign("   ").is_err());
        assert!(validate_callsign(&"x".repeat(CALLSIGN_MAX + 1)).is_err());
        assert!(validate_callsign("bad\u{7}name").is_err());
    }
}
