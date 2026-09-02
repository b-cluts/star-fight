//! Squads: what a player brings to a game — pilots with upgrades and
//! callsigns — and the shared validation both client (live feedback in
//! the builder) and server (the guarantee on join) run.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::data::Content;
use crate::pilot::{PilotId, Source};
use crate::ship::{Faction, SizeClass, default_callsign, validate_callsign};
use crate::upgrade::{Restriction, Slot, UpgradeEffect, UpgradeId};

/// One ship in a squad.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SquadShip {
    pub pilot: PilotId,
    #[serde(default)]
    pub upgrades: Vec<UpgradeId>,
    /// Empty = server assigns the squad default ("Red-2").
    #[serde(default)]
    pub callsign: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Squad {
    pub name: String,
    pub faction: Faction,
    pub ships: Vec<SquadShip>,
}

/// Scenario limits a game session imposes on squads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SquadRules {
    pub max_points: u32,
    /// Allowed source packs for pilots (upgrades follow); None = all.
    #[serde(default)]
    pub sources: Option<Vec<Source>>,
    pub max_ships: u8,
}

impl Default for SquadRules {
    fn default() -> Self {
        Self { max_points: 100, sources: None, max_ships: 12 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SquadError {
    Empty,
    TooManyShips { max: u8 },
    OverBudget { points: u32, max: u32 },
    UnknownPilot(PilotId),
    UnknownUpgrade(UpgradeId),
    WrongFaction { ship: usize },
    SourceNotAllowed { ship: usize },
    DuplicateUnique(String),
    NoSlot { ship: usize, upgrade: String, slot: Slot },
    LimitedTwice { ship: usize, upgrade: String },
    Restricted { ship: usize, upgrade: String, why: String },
    BadCallsign { ship: usize, why: String },
    DuplicateCallsign(String),
}

impl std::fmt::Display for SquadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SquadError::Empty => write!(f, "squad has no ships"),
            SquadError::TooManyShips { max } => write!(f, "more than {max} ships"),
            SquadError::OverBudget { points, max } => {
                write!(f, "{points} points exceeds the {max}-point limit")
            }
            SquadError::UnknownPilot(p) => write!(f, "unknown pilot {p:?}"),
            SquadError::UnknownUpgrade(u) => write!(f, "unknown upgrade {u:?}"),
            SquadError::WrongFaction { ship } => {
                write!(f, "ship {} is not of your faction", ship + 1)
            }
            SquadError::SourceNotAllowed { ship } => {
                write!(f, "ship {}'s pilot is not from an allowed set", ship + 1)
            }
            SquadError::DuplicateUnique(n) => write!(f, "{n} is unique and appears twice"),
            SquadError::NoSlot { ship, upgrade, slot } => {
                write!(f, "ship {} has no free {slot:?} slot for {upgrade}", ship + 1)
            }
            SquadError::LimitedTwice { ship, upgrade } => {
                write!(f, "ship {} carries limited card {upgrade} twice", ship + 1)
            }
            SquadError::Restricted { ship, upgrade, why } => {
                write!(f, "ship {} cannot equip {upgrade}: {why}", ship + 1)
            }
            SquadError::BadCallsign { ship, why } => write!(f, "ship {}: {why}", ship + 1),
            SquadError::DuplicateCallsign(c) => write!(f, "callsign {c} used twice"),
        }
    }
}

/// Validated totals, handed back on success.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SquadSummary {
    pub points: u32,
}

impl Squad {
    /// A squad of basic pilots for the given classes, unnamed callsigns.
    pub fn basic(content: &Content, name: &str, pilots: &[PilotId]) -> Self {
        let faction = pilots
            .first()
            .and_then(|p| content.pilots.pilot(*p))
            .and_then(|p| content.ships.class(p.class))
            .map(|c| c.faction)
            .unwrap_or(Faction::RebelAlliance);
        Self {
            name: name.into(),
            faction,
            ships: pilots
                .iter()
                .map(|&pilot| SquadShip { pilot, upgrades: Vec::new(), callsign: String::new() })
                .collect(),
        }
    }

    /// Points of pilots + upgrades (unknown ids count 0; validate first).
    pub fn cost(&self, content: &Content) -> u32 {
        self.ships.iter().map(|s| ship_cost(content, s)).sum()
    }

    /// Callsigns with the squad default filled in for blanks.
    pub fn callsigns(&self, squad_name: &str) -> Vec<String> {
        self.ships
            .iter()
            .enumerate()
            .map(|(n, s)| {
                let c = s.callsign.trim();
                if c.is_empty() { default_callsign(squad_name, n) } else { c.to_string() }
            })
            .collect()
    }
}

pub fn ship_cost(content: &Content, ship: &SquadShip) -> u32 {
    let pilot = content.pilots.pilot(ship.pilot).map(|p| p.cost as u32).unwrap_or(0);
    let ups: u32 = ship
        .upgrades
        .iter()
        .filter_map(|u| content.upgrades.upgrade(*u))
        .map(|u| u.cost as u32)
        .sum();
    pilot + ups
}

/// Every problem with a squad, or its point total.
pub fn validate_squad(
    squad: &Squad,
    content: &Content,
    rules: &SquadRules,
) -> Result<SquadSummary, Vec<SquadError>> {
    let mut errors = Vec::new();
    if squad.ships.is_empty() {
        errors.push(SquadError::Empty);
    }
    if squad.ships.len() > rules.max_ships as usize {
        errors.push(SquadError::TooManyShips { max: rules.max_ships });
    }
    let mut uniques: HashMap<String, usize> = HashMap::new();
    let mut callsigns: HashMap<String, usize> = HashMap::new();

    for (i, ship) in squad.ships.iter().enumerate() {
        let Some(pilot) = content.pilots.pilot(ship.pilot) else {
            errors.push(SquadError::UnknownPilot(ship.pilot));
            continue;
        };
        let Some(class) = content.ships.class(pilot.class) else {
            errors.push(SquadError::UnknownPilot(ship.pilot));
            continue;
        };
        if class.faction != squad.faction {
            errors.push(SquadError::WrongFaction { ship: i });
        }
        if let Some(allowed) = &rules.sources
            && !allowed.contains(&pilot.source)
        {
            errors.push(SquadError::SourceNotAllowed { ship: i });
        }
        if pilot.unique {
            *uniques.entry(unique_key(&pilot.name)).or_default() += 1;
        }
        if !ship.callsign.trim().is_empty() {
            match validate_callsign(&ship.callsign) {
                Ok(c) => *callsigns.entry(c.to_ascii_lowercase()).or_default() += 1,
                Err(why) => errors.push(SquadError::BadCallsign { ship: i, why }),
            }
        }

        // Slots: printed bar + implicit, plus slots granted by equipped
        // cards (R2-D6 grants a talent slot).
        let mut printed: Vec<Slot> = class.upgrade_bar.clone();
        if pilot.talent_slot {
            printed.push(Slot::Talent);
        }
        let mut slots: HashMap<Slot, u8> = HashMap::new();
        for s in printed.iter().copied().chain(Slot::implicit()) {
            *slots.entry(s).or_default() += 1;
        }
        let cards: Vec<_> =
            ship.upgrades.iter().filter_map(|u| content.upgrades.upgrade(*u)).collect();
        for u in &cards {
            if u.effect == Some(UpgradeEffect::BarGainsTalent) {
                *slots.entry(Slot::Talent).or_default() += 1;
            }
        }
        // Action icons including ones granted by modifications.
        let mut actions = class.action_bar.clone();
        for u in &cards {
            match u.effect {
                Some(UpgradeEffect::BarGainsTargetLock) => {
                    actions.push(crate::action::ActionKind::TargetLock)
                }
                Some(UpgradeEffect::BarGainsBoost) => {
                    actions.push(crate::action::ActionKind::Boost)
                }
                Some(UpgradeEffect::BarGainsBarrelRoll) => {
                    actions.push(crate::action::ActionKind::BarrelRoll)
                }
                _ => {}
            }
        }
        let mut seen_limited: Vec<UpgradeId> = Vec::new();
        for id in &ship.upgrades {
            let Some(u) = content.upgrades.upgrade(*id) else {
                errors.push(SquadError::UnknownUpgrade(*id));
                continue;
            };
            let free = slots.entry(u.slot).or_default();
            if *free == 0 {
                errors.push(SquadError::NoSlot { ship: i, upgrade: u.name.clone(), slot: u.slot });
            } else {
                *free -= 1;
            }
            if u.limited {
                if seen_limited.contains(id) {
                    errors.push(SquadError::LimitedTwice { ship: i, upgrade: u.name.clone() });
                }
                seen_limited.push(*id);
            }
            if u.unique {
                *uniques.entry(unique_key(&u.name)).or_default() += 1;
            }
            for r in &u.restrictions {
                let ok = match r {
                    Restriction::SmallShipOnly => class.size == SizeClass::Small,
                    Restriction::LargeShipOnly => class.size == SizeClass::Large,
                    Restriction::ShipOnly(sub) => class.xws.contains(sub.as_str()),
                    Restriction::FactionOnly(f) => class.faction == *f,
                    Restriction::SkillAbove(s) => pilot.skill > *s,
                    Restriction::SkillAtMost(s) => pilot.skill <= *s,
                    Restriction::RequiresAction(a) => actions.contains(a),
                    Restriction::LacksAction(a) => !actions.contains(a),
                    Restriction::RequiresSlots(list) => list.iter().all(|s| printed.contains(s)),
                    Restriction::LacksSlot(s) => !printed.contains(s),
                    Restriction::AgilityBelow(a) => class.agility < *a,
                };
                if !ok {
                    errors.push(SquadError::Restricted {
                        ship: i,
                        upgrade: u.name.clone(),
                        why: format!("{r:?}"),
                    });
                }
            }
        }
    }
    for (name, n) in uniques {
        if n > 1 {
            errors.push(SquadError::DuplicateUnique(name));
        }
    }
    for (c, n) in callsigns {
        if n > 1 {
            errors.push(SquadError::DuplicateCallsign(c));
        }
    }
    let points = squad.cost(content);
    if points > rules.max_points {
        errors.push(SquadError::OverBudget { points, max: rules.max_points });
    }
    if errors.is_empty() { Ok(SquadSummary { points }) } else { Err(errors) }
}

/// Unique names match ignoring the quotation marks and case ("Poe
/// Dameron" PS8 and PS9 are the same unique name).
fn unique_key(name: &str) -> String {
    name.chars().filter(|c| *c != '"').collect::<String>().to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ship::ShipClassId;

    fn content() -> Content {
        Content::load_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data")).unwrap()
    }
    fn pilot(c: &Content, xws: &str) -> PilotId {
        c.pilots.pilots.iter().find(|p| p.xws == xws).unwrap().id
    }
    fn up(c: &Content, xws: &str) -> UpgradeId {
        c.upgrades.upgrades.iter().find(|u| u.xws == xws).unwrap().id
    }
    fn ship(c: &Content, p: &str, ups: &[&str]) -> SquadShip {
        SquadShip {
            pilot: pilot(c, p),
            upgrades: ups.iter().map(|u| up(c, u)).collect(),
            callsign: String::new(),
        }
    }
    fn errs(r: Result<SquadSummary, Vec<SquadError>>) -> Vec<SquadError> {
        r.err().unwrap_or_default()
    }

    #[test]
    fn basic_fleets_validate_and_cost_the_pilot_sum() {
        let c = content();
        let academy = c.pilots.basic_for(ShipClassId(1)).unwrap().id;
        let s = Squad::basic(&c, "swarm", &[academy; 8]);
        assert_eq!(s.faction, Faction::Empire);
        assert_eq!(validate_squad(&s, &c, &SquadRules::default()).unwrap().points, 96);
        assert_eq!(s.callsigns("Obsidian")[1], "Obsidian-2");
    }

    #[test]
    fn a_legal_resistance_squad_with_upgrades() {
        let c = content();
        let s = Squad {
            name: "Black Squadron".into(),
            faction: Faction::RebelAlliance,
            ships: vec![
                ship(
                    &c,
                    "poedameron-swx57",
                    &[
                        "veteraninstincts",
                        "bb8",
                        "protontorpedoes",
                        "weaponsguidance",
                        "autothrusters",
                        "blackone",
                    ],
                ),
                ship(&c, "bluesquadronnovice", &["r2astromech", "integratedastromech"]),
            ],
        };
        // 33+1+2+4+2+2+1 = 45, 24+1+0 = 25
        let ok = validate_squad(&s, &c, &SquadRules::default()).unwrap();
        assert_eq!(ok.points, 70);
    }

    #[test]
    fn slot_unique_limited_and_restriction_errors() {
        let c = content();
        let s = Squad {
            name: "bad".into(),
            faction: Faction::RebelAlliance,
            ships: vec![
                // Novice has no talent slot; two torpedoes for one slot;
                // Black One needs PS > 6; Extra Munitions twice (Limited).
                ship(
                    &c,
                    "bluesquadronnovice",
                    &[
                        "wired",
                        "protontorpedoes",
                        "plasmatorpedoes",
                        "blackone",
                        "extramunitions",
                        "extramunitions",
                    ],
                ),
                // Two Poes are one unique name.
                ship(&c, "poedameron", &[]),
                ship(&c, "poedameron-swx57", &[]),
                // Wrong faction.
                ship(&c, "academypilot", &[]),
            ],
        };
        let e = errs(validate_squad(&s, &c, &SquadRules::default()));
        assert!(
            e.iter().any(|x| matches!(x, SquadError::NoSlot { ship: 0, slot: Slot::Talent, .. })),
            "{e:?}"
        );
        assert!(
            e.iter().any(|x| matches!(x, SquadError::NoSlot { ship: 0, slot: Slot::Torpedo, .. })),
            "{e:?}"
        );
        assert!(e.iter().any(|x| matches!(x, SquadError::Restricted { ship: 0, .. })), "{e:?}");
        assert!(e.iter().any(|x| matches!(x, SquadError::LimitedTwice { ship: 0, .. })), "{e:?}");
        assert!(
            e.iter().any(|x| matches!(x, SquadError::DuplicateUnique(n) if n == "poe dameron")),
            "{e:?}"
        );
        assert!(e.iter().any(|x| matches!(x, SquadError::WrongFaction { ship: 3 })), "{e:?}");
        assert!(e.iter().any(|x| matches!(x, SquadError::OverBudget { .. })), "{e:?}");
    }

    #[test]
    fn granted_slots_and_icons_count_and_source_rules_apply() {
        let c = content();
        // R2-D6 grants a talent slot to a PS4 Red Squadron Veteran? No —
        // he already has one (LacksSlot(Talent) fails); the PS2 Novice is
        // too low (SkillAbove(2) fails). Poe PS8 has a talent slot too.
        // Use Jess Pava (PS3, no talent slot): R2-D6 + Wired is legal.
        let ok = Squad {
            name: "jess".into(),
            faction: Faction::RebelAlliance,
            ships: vec![ship(&c, "jesspava", &["r2d6", "wired"])],
        };
        assert!(validate_squad(&ok, &c, &SquadRules::default()).is_ok());
        let bad = Squad {
            name: "novice".into(),
            faction: Faction::RebelAlliance,
            ships: vec![ship(&c, "bluesquadronnovice", &["r2d6"])],
        };
        assert!(
            errs(validate_squad(&bad, &c, &SquadRules::default()))
                .iter()
                .any(|x| matches!(x, SquadError::Restricted { .. }))
        );
        // Engine Upgrade grants boost, which Autothrusters needs (TIE/ln).
        let tie = Squad {
            name: "tie".into(),
            faction: Faction::Empire,
            ships: vec![ship(&c, "howlrunner", &["autothrusters"])],
        };
        assert!(validate_squad(&tie, &c, &SquadRules::default()).is_err());
        // Core-set-only scenario refuses Howlrunner (TIE Fighter expansion).
        let core = SquadRules {
            sources: Some(vec![Source::CoreSet, Source::OriginalCoreSet]),
            ..Default::default()
        };
        let e = errs(validate_squad(&tie, &c, &core));
        assert!(e.iter().any(|x| matches!(x, SquadError::SourceNotAllowed { ship: 0 })));
        // Callsigns: bad and duplicate.
        let mut named =
            Squad::basic(&c, "x", &[pilot(&c, "academypilot"), pilot(&c, "academypilot")]);
        named.ships[0].callsign = "Alpha".into();
        named.ships[1].callsign = "alpha".into();
        let e = errs(validate_squad(&named, &c, &SquadRules::default()));
        assert!(e.iter().any(|x| matches!(x, SquadError::DuplicateCallsign(_))), "{e:?}");
    }
}
