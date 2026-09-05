//! The authoritative game state and turn phase machine:
//!
//! `Placement -> [Planning -> Resolution]* -> GameOver`
//!
//! Every mutation goes through a validated command method returning
//! `Result<_, Rejection>`; the server applies these verbatim, the client
//! may use the same methods for optimistic UI checks.

use serde::{Deserialize, Serialize};

use crate::action::{self, ActionKind, ActionResult, PlannedAction};
use crate::board::{Board, Seat};
use crate::combat;
use crate::crit::{self, CritEffect};
use crate::data::Content;
use crate::dice::{AttackFace, DefenseFace};
use crate::geometry::{Footprint, Pose, Vec2};
use crate::maneuver::{self, Difficulty, Maneuver};
use crate::pilot::{PilotAbility, PilotId};
use crate::rules;
use crate::ship::{PlayerId, ShipClass, ShipClassId, ShipId, ShipState, StatBlock};
use crate::squad::Squad;
use crate::upgrade::{AttackRequirement, Slot, UpgradeEffect, UpgradeId};

/// The turn phase machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Placement,
    Planning,
    /// Combat is being resolved step by step (may be waiting on a
    /// player's Declare Target choice).
    Combat,
    GameOver,
}

/// Who holds initiative at setup. The lower squad-point total's player
/// takes it; on a tie, seat 0 (the game creator) rolls one red die —
/// Hit/Crit keeps it, Focus/Blank hands it to the opponent. ("Choosing"
/// is automated as choosing yourself.)
pub fn initiative_seat(totals: [u32; 2], tie_roll: crate::dice::AttackFace) -> usize {
    use crate::dice::AttackFace;
    match totals[0].cmp(&totals[1]) {
        std::cmp::Ordering::Less => 0,
        std::cmp::Ordering::Greater => 1,
        std::cmp::Ordering::Equal => match tie_roll {
            AttackFace::Hit | AttackFace::Crit => 0,
            AttackFace::Focus | AttackFace::Blank => 1,
        },
    }
}

/// Outcome of one point of normal damage.
enum DamagePoint {
    Shield,
    Hull,
    None,
}

/// Replacement for a red maneuver revealed while stressed: prefer the
/// slowest white straight, then any slowest white, then any non-red —
/// judging color AFTER crit modifiers.
fn substitute_non_red(dial: &[Maneuver], crits: &[CritEffect]) -> Option<Maneuver> {
    let eff = |m: &Maneuver| crit::effective_difficulty(crits, m);
    let pick = |pred: &dyn Fn(&Maneuver) -> bool| {
        dial.iter().filter(|m| pred(m)).min_by_key(|m| m.distance).copied()
    };
    pick(&|m| eff(m) == Difficulty::Normal && m.steer == crate::maneuver::Steer::Straight)
        .or_else(|| pick(&|m| eff(m) == Difficulty::Normal))
        .or_else(|| pick(&|m| eff(m) != Difficulty::Hard))
}

/// Movement phase order: LOWEST pilot skill moves first. At equal skill
/// the initiative player's ships go first; then ship id.
pub fn movement_order(ships: &[(ShipId, u8, PlayerId)], initiative: PlayerId) -> Vec<ShipId> {
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill, owner)| (skill, owner != initiative, id.0));
    v.into_iter().map(|(id, _, _)| id).collect()
}

/// Combat phase order: HIGHEST pilot skill fires first. At equal skill
/// the initiative player's ships fire first; then ship id.
pub fn combat_order(ships: &[(ShipId, u8, PlayerId)], initiative: PlayerId) -> Vec<ShipId> {
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill, owner)| (std::cmp::Reverse(skill), owner != initiative, id.0));
    v.into_iter().map(|(id, _, _)| id).collect()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Rejection {
    WrongPhase,
    NoSuchShip,
    NotYourShip,
    ShipDestroyed,
    OutOfZone,
    OverlapsShip,
    BadManeuverIndex,
    /// The action is not on this ship's action bar.
    ActionNotOnBar,
    /// Target locks need a living enemy ship as the target.
    BadLockTarget,
    /// Red maneuvers cannot be planned while stressed.
    StressedRedForbidden,
    /// All surviving ships need a plan before committing.
    PlansIncomplete,
    AlreadyCommitted,
    /// No attack is waiting for a target right now.
    NoPendingAttack,
    /// Not one of the eligible targets for the pending attack.
    BadTarget,
    /// Callsign empty, too long, or already used by another ship.
    BadCallsign(String),
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Rejection::WrongPhase => "not allowed in the current phase",
            Rejection::NoSuchShip => "no such ship",
            Rejection::NotYourShip => "that is not your ship",
            Rejection::ShipDestroyed => "that ship is destroyed",
            Rejection::OutOfZone => "outside your deployment zone",
            Rejection::OverlapsShip => "overlaps another ship",
            Rejection::BadManeuverIndex => "no such maneuver on this dial",
            Rejection::ActionNotOnBar => "that action is not on this ship's action bar",
            Rejection::BadLockTarget => "target lock needs a living enemy ship",
            Rejection::StressedRedForbidden => "stressed ships cannot fly red maneuvers",
            Rejection::PlansIncomplete => "every surviving ship needs a maneuver first",
            Rejection::AlreadyCommitted => "plans already committed this turn",
            Rejection::NoPendingAttack => "no attack is waiting for a target",
            Rejection::BadTarget => "that ship is not an eligible target",
            Rejection::BadCallsign(why) => return write!(f, "bad callsign: {why}"),
        };
        f.write_str(s)
    }
}

/// One ship's resolved movement, for animation and the record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MoveRecord {
    pub ship: ShipId,
    pub maneuver: Maneuver,
    /// Sampled poses actually flown (truncated at a bump).
    pub path: Vec<Pose>,
    pub end: Pose,
    /// Stopped short because another ship was in the way.
    pub bumped: bool,
    /// Flew off the board and is destroyed.
    pub destroyed: bool,
    /// Stress tokens after the maneuver.
    pub stress: u8,
    /// The action that was planned (Pass if none was).
    pub action: PlannedAction,
    pub action_result: ActionResult,
}

/// One resolved attack in the Combat phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttackRecord {
    pub attacker: ShipId,
    pub defender: ShipId,
    pub range: u8,
    /// The secondary weapon card fired, or None for the primary weapon.
    pub weapon: Option<UpgradeId>,
    /// Final attack faces after rerolls/conversions.
    pub attack_faces: Vec<AttackFace>,
    /// Final defense faces after conversions.
    pub defense_faces: Vec<DefenseFace>,
    pub lock_spent: bool,
    pub attacker_focus_spent: bool,
    pub defender_focus_spent: bool,
    pub evade_spent: bool,
    /// Defender was inside the attacker's bullseye lane (tokens denied).
    pub defender_in_bullseye: bool,
    /// Uncanceled results that landed.
    pub hits: u8,
    pub crits: u8,
    pub shields_lost: u8,
    pub hull_lost: u8,
    /// Crits that reached the hull (future: draw modifier effects).
    pub crits_to_hull: u8,
    pub defender_destroyed: bool,
}

/// Everything that happened when a turn resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecords {
    pub moves: Vec<MoveRecord>,
    pub attacks: Vec<AttackRecord>,
    /// Narrated side effects: crit draws, Console Fire burns, Stunned
    /// Pilot bumps, destructions from effects.
    pub events: Vec<String>,
}

/// What the Activation phase produced (returned by `commit_plans_begin`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationRecords {
    pub moves: Vec<MoveRecord>,
    pub events: Vec<String>,
}

/// One way an attacker may fire this round: a weapon and an eligible
/// target for it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttackOption {
    /// None = primary weapon; Some = an equipped secondary weapon card.
    pub weapon: Option<UpgradeId>,
    pub target: ShipId,
    pub range: u8,
    /// Base-to-base distance (nearest-target policy).
    pub dist: f64,
}

/// A declared shot: defender index, range band and weapon.
#[derive(Debug, Clone, Copy)]
struct Shot {
    d_idx: usize,
    range: u8,
    weapon: Option<UpgradeId>,
}

/// An attack whose owner must Declare Target (core rules p.10): more than
/// one (weapon, enemy) combination is eligible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAttack {
    pub attacker: ShipId,
    pub owner: PlayerId,
    pub options: Vec<AttackOption>,
}

/// Step-by-step Combat phase bookkeeping (lives in `GameState.combat`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CombatState {
    /// Remaining pilot-skill groups, highest first.
    groups: Vec<Vec<ShipId>>,
    /// Attackers still to act in the current group (alive at its start).
    current: Vec<ShipId>,
    pub pending: Option<PendingAttack>,
    attacks: Vec<AttackRecord>,
    events: Vec<String>,
    moves: Vec<MoveRecord>,
}

/// Result of advancing the Combat phase one step.
#[derive(Debug, Clone, PartialEq)]
pub enum CombatStep {
    /// The owner must choose among several eligible targets.
    NeedTarget(PendingAttack),
    /// One attack resolved (single eligible target, or a declared one).
    Attack(AttackRecord),
    /// Combat and the End phase are complete.
    Done(TurnRecords),
}

/// Per-player view of a ship. During Placement, opponent poses are hidden;
/// plans are only ever visible on the viewer's own ships.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipView {
    pub id: ShipId,
    pub owner: PlayerId,
    pub class: ShipClassId,
    pub callsign: String,
    /// Pilot card name and printed skill (crits may lower the effective
    /// skill; see `pilot_skill`).
    pub pilot: String,
    pub skill: u8,
    /// Equipped upgrade card names.
    pub upgrades: Vec<String>,
    /// Effective values after upgrades: hull/shield maxima, agility,
    /// and the action bar (for the planning keys).
    pub max_hull: u8,
    pub max_shields: u8,
    pub agility: u8,
    pub actions: Vec<ActionKind>,
    pub pose: Option<Pose>,
    pub hull: u8,
    pub shields: u8,
    pub stress: u8,
    pub focus: u8,
    pub evade: u8,
    pub lock: Option<ShipId>,
    /// Active critical effects — public, like faceup cards.
    pub crits: Vec<CritEffect>,
    pub destroyed: bool,
    pub plan: Option<u8>,
    /// Own ships only; None on opponent ships.
    pub planned_action: Option<PlannedAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub board: Board,
    pub phase: Phase,
    pub turn: u32,
    pub ships: Vec<ShipState>,
    pub committed: [bool; 2],
    pub winner: Option<PlayerId>,
    /// Holder of the initiative token (see `initiative_seat`).
    pub initiative: PlayerId,
    /// Squad-point totals per seat, for display.
    pub squad_totals: [u32; 2],
    /// Present while the Combat phase is being stepped through.
    pub combat: Option<CombatState>,
}

impl GameState {
    /// `fleets[0]` deploys South (seat 0), `fleets[1]` North (seat 1);
    /// each entry is a pilot card, which implies the ship class. Basic
    /// squads: no upgrades, default callsigns.
    pub fn new(
        board: Board,
        content: &Content,
        fleets: [&[PilotId]; 2],
        tie_roll: crate::dice::AttackFace,
    ) -> Result<Self, String> {
        let a = Squad::basic(content, "south", fleets[0]);
        let b = Squad::basic(content, "north", fleets[1]);
        Self::from_squads(board, content, [&a, &b], tie_roll)
    }

    /// Build a game from two (already validated) squads. `tie_roll` is
    /// one red die drawn by the server, used only when the squad totals
    /// are equal.
    pub fn from_squads(
        board: Board,
        content: &Content,
        squads: [&Squad; 2],
        tie_roll: crate::dice::AttackFace,
    ) -> Result<Self, String> {
        let pilot_of =
            |id: PilotId| content.pilots.pilot(id).ok_or_else(|| format!("unknown pilot {id:?}"));
        let factions = [squads[0].faction, squads[1].faction];
        let squad_names = crate::ship::squad_names(&factions);
        let mut ships = Vec::new();
        for (seat, squad) in squads.iter().enumerate() {
            let callsigns = squad.callsigns(squad_names[seat]);
            for (n, entry) in squad.ships.iter().enumerate() {
                let pilot_id = entry.pilot;
                let pilot = pilot_of(pilot_id)?;
                let class_id = pilot.class;
                let class = content
                    .ships
                    .class(class_id)
                    .ok_or_else(|| format!("unknown ship class {class_id:?}"))?;
                content
                    .dials
                    .set(class.maneuver_set)
                    .ok_or_else(|| format!("{} has no dial", class.name))?;
                for u in &entry.upgrades {
                    content.upgrades.upgrade(*u).ok_or_else(|| format!("unknown upgrade {u:?}"))?;
                }
                ships.push(ShipState {
                    id: ShipId(ships.len() as u32),
                    owner: PlayerId(seat as u32),
                    class: class_id,
                    pilot: pilot_id,
                    upgrades: entry.upgrades.clone(),
                    callsign: callsigns[n].clone(),
                    pose: None,
                    hull: class.hull,
                    shields: class.shields,
                    stress: 0,
                    plan: None,
                    planned_action: None,
                    focus: 0,
                    evade: 0,
                    lock: None,
                    crits: Vec::new(),
                    destroyed: false,
                });
            }
        }
        // Hull Upgrade / Shield Upgrade raise the starting values.
        let mut gs_probe = Self {
            board,
            phase: Phase::Placement,
            turn: 1,
            ships,
            committed: [false, false],
            winner: None,
            initiative: PlayerId(0),
            squad_totals: [0, 0],
            combat: None,
        };
        for i in 0..gs_probe.ships.len() {
            let (h, sh) = (
                gs_probe.max_hull(content, &gs_probe.ships[i]),
                gs_probe.max_shields(content, &gs_probe.ships[i]),
            );
            gs_probe.ships[i].hull = h;
            gs_probe.ships[i].shields = sh;
        }
        let ships = gs_probe.ships;
        let squad_totals = [squads[0].cost(content), squads[1].cost(content)];
        let initiative = PlayerId(initiative_seat(squad_totals, tie_roll) as u32);
        Ok(Self {
            board,
            phase: Phase::Placement,
            turn: 1,
            ships,
            committed: [false, false],
            winner: None,
            initiative,
            squad_totals,
            combat: None,
        })
    }

    fn seat_of(player: PlayerId) -> Seat {
        if player.0 == 0 { Seat::South } else { Seat::North }
    }

    fn class_of<'a>(&self, content: &'a Content, ship: &ShipState) -> &'a ShipClass {
        content.ships.class(ship.class).expect("classes validated in new()")
    }

    /// Effects of a ship's equipped upgrade cards.
    fn effects<'a>(
        &self,
        content: &'a Content,
        s: &'a ShipState,
    ) -> impl Iterator<Item = UpgradeEffect> + 'a {
        s.upgrades.iter().filter_map(|u| content.upgrades.upgrade(*u)).filter_map(|u| u.effect)
    }

    fn count_effect(&self, content: &Content, s: &ShipState, e: UpgradeEffect) -> u8 {
        self.effects(content, s).filter(|x| *x == e).count() as u8
    }

    /// Pilot skill after upgrades (Veteran Instincts +2, Adaptability
    /// ±1) and crits: a Damaged Cockpit drops it to 0.
    pub fn effective_skill(&self, content: &Content, s: &ShipState) -> u8 {
        if s.crits.contains(&CritEffect::DamagedCockpit) {
            return 0;
        }
        let base = content.pilots.pilot(s.pilot).map(|p| p.skill).unwrap_or(0) as i16;
        let up = 2 * self.count_effect(content, s, UpgradeEffect::SkillPlus2) as i16
            + self.count_effect(content, s, UpgradeEffect::SkillPlus1) as i16
            - self.count_effect(content, s, UpgradeEffect::SkillMinus1) as i16;
        (base + up).clamp(0, 12) as u8
    }

    /// Printed stats: the pilot card's own block when it has one, else
    /// the chassis values.
    pub fn printed(&self, content: &Content, s: &ShipState) -> StatBlock {
        content
            .pilots
            .pilot(s.pilot)
            .and_then(|p| p.stats)
            .unwrap_or_else(|| self.class_of(content, s).stats())
    }

    /// Hull value with Hull Upgrade.
    pub fn max_hull(&self, content: &Content, s: &ShipState) -> u8 {
        self.printed(content, s).hull + self.count_effect(content, s, UpgradeEffect::HullPlus1)
    }

    /// Shield value with Shield Upgrade.
    pub fn max_shields(&self, content: &Content, s: &ShipState) -> u8 {
        self.printed(content, s).shields + self.count_effect(content, s, UpgradeEffect::ShieldPlus1)
    }

    /// Agility with Stealth Device (+1 while equipped) and Structural
    /// Damage crits (−1 each).
    pub fn agility(&self, content: &Content, s: &ShipState) -> u8 {
        let structural =
            s.crits.iter().filter(|x| matches!(x, CritEffect::StructuralDamage)).count() as u8;
        (self.printed(content, s).agility
            + self.count_effect(content, s, UpgradeEffect::AgilityPlus1DiscardWhenHit))
        .saturating_sub(structural)
    }

    /// Action bar with icons granted by modifications (Targeting
    /// Computer, Engine Upgrade, Vectored Thrusters).
    pub fn action_bar(&self, content: &Content, s: &ShipState) -> Vec<ActionKind> {
        let mut bar = self.class_of(content, s).action_bar.clone();
        for e in self.effects(content, s) {
            let granted = match e {
                UpgradeEffect::BarGainsTargetLock => Some(ActionKind::TargetLock),
                UpgradeEffect::BarGainsBoost => Some(ActionKind::Boost),
                UpgradeEffect::BarGainsBarrelRoll => Some(ActionKind::BarrelRoll),
                UpgradeEffect::BarGainsEvade => Some(ActionKind::Evade),
                _ => None,
            };
            if let Some(a) = granted
                && !bar.contains(&a)
            {
                bar.push(a);
            }
        }
        bar
    }

    /// The pilot's card ability, unless an Injured Pilot crit has
    /// silenced it.
    fn ability(&self, content: &Content, s: &ShipState) -> Option<PilotAbility> {
        if s.crits.contains(&CritEffect::InjuredPilot) {
            return None;
        }
        content.pilots.pilot(s.pilot).and_then(|p| p.ability)
    }

    // ---- Attack pipeline hooks -------------------------------------
    // perform_attack_on runs: roll attack → modify attack (attacker's
    // free conversions, lock rerolls, focus spend) → roll defense → modify
    // defense (free conversions, focus spend, evade spend) → compare →
    // damage. Card effects plug into the two "modify" stages below.

    /// Is any corner or edge midpoint of a base inside the shooter's
    /// 90° forward arc?
    fn base_in_front_arc(shooter: Pose, shooter_fp: Footprint, corners: &[Vec2; 4]) -> bool {
        corners.iter().any(|&p| combat::in_front_arc(shooter, shooter_fp, p))
            || (0..4).any(|i| {
                let m = Vec2::new(
                    (corners[i].x + corners[(i + 1) % 4].x) / 2.0,
                    (corners[i].y + corners[(i + 1) % 4].y) / 2.0,
                );
                combat::in_front_arc(shooter, shooter_fp, m)
            })
    }

    /// Is ship `target` inside ship `shooter`'s forward firing arc?
    fn ship_in_front_arc(&self, content: &Content, shooter: usize, target: usize) -> bool {
        let (Some(s_pose), Some(t_pose)) = (self.ships[shooter].pose, self.ships[target].pose)
        else {
            return false;
        };
        let s_fp = self.class_of(content, &self.ships[shooter]).footprint;
        let t_fp = self.class_of(content, &self.ships[target]).footprint;
        Self::base_in_front_arc(s_pose, s_fp, &rules::footprint_corners(t_pose, t_fp))
    }

    /// Range band between two ships (None if either is off the board
    /// or they are beyond Range 3).
    fn range_between(&self, content: &Content, i: usize, j: usize) -> Option<u8> {
        let (Some(pi), Some(pj)) = (self.ships[i].pose, self.ships[j].pose) else { return None };
        let fi = self.class_of(content, &self.ships[i]).footprint;
        let fj = self.class_of(content, &self.ships[j]).footprint;
        combat::range_band_between(
            &rules::footprint_corners(pi, fi),
            &rules::footprint_corners(pj, fj),
        )
    }

    /// Other living friendly ships within Range 1 of ship `s`.
    fn friends_at_range1(&self, content: &Content, s: usize) -> Vec<usize> {
        (0..self.ships.len())
            .filter(|&o| {
                o != s
                    && self.ships[o].owner == self.ships[s].owner
                    && !self.ships[o].destroyed
                    && self.range_between(content, s, o) == Some(1)
            })
            .collect()
    }

    /// Rerolls a ship may take thanks to friends at Range 1: Jess Pava
    /// gets one per friend (attacking or defending); a friendly
    /// Howlrunner grants one more to a primary-weapon attack.
    fn friendly_rerolls(&self, content: &Content, s: usize, attacking: bool) -> u8 {
        let friends = self.friends_at_range1(content, s);
        let mut n = 0;
        if self.ability(content, &self.ships[s]) == Some(PilotAbility::RerollPerFriendlyRange1) {
            n += friends.len() as u8;
        }
        if attacking
            && friends.iter().any(|&f| {
                self.ability(content, &self.ships[f])
                    == Some(PilotAbility::FriendlyRerollAttackRange1)
            })
        {
            n += 1;
        }
        n
    }

    /// Reroll up to `n` attack dice: blanks first, then eyes when no
    /// focus token could convert them.
    fn reroll_attack_dice(
        &self,
        content: &Content,
        a_idx: usize,
        faces: &mut [AttackFace],
        n: u8,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        let eyes_too = self.ships[a_idx].focus == 0;
        let mut left = n;
        for want in [AttackFace::Blank, AttackFace::Focus] {
            if want == AttackFace::Focus && !eyes_too {
                break;
            }
            for f in faces.iter_mut() {
                if left > 0 && *f == want {
                    *f = AttackFace::from_d8(roll());
                    left -= 1;
                }
            }
        }
        if left < n {
            events.push(format!(
                "{}: rerolls {} attack dice (friends at Range 1)",
                self.label(content, a_idx),
                n - left
            ));
        }
    }

    /// Reroll up to `n` defense dice on the same policy.
    fn reroll_defense_dice(
        &self,
        content: &Content,
        d_idx: usize,
        faces: &mut [DefenseFace],
        n: u8,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        let eyes_too = self.ships[d_idx].focus == 0;
        let mut left = n;
        for want in [DefenseFace::Blank, DefenseFace::Focus] {
            if want == DefenseFace::Focus && !eyes_too {
                break;
            }
            for f in faces.iter_mut() {
                if left > 0 && *f == want {
                    *f = DefenseFace::from_d8(roll());
                    left -= 1;
                }
            }
        }
        if left < n {
            events.push(format!(
                "{}: rerolls {} defense dice (friends at Range 1)",
                self.label(content, d_idx),
                n - left
            ));
        }
    }

    /// Additional attack dice granted by the attacker's pilot ability,
    /// decided before the roll. Mauler Mithel (+1 at Range 1),
    /// Backstabber (+1 from outside the defender's arc), Scourge (+1
    /// against a damaged defender) and Zeta Leader (+1 for taking a
    /// stress token while unstressed — always accepted).
    fn extra_attack_dice(
        &mut self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        range: u8,
        events: &mut Vec<String>,
    ) -> u8 {
        let Some(ability) = self.ability(content, &self.ships[a_idx]) else { return 0 };
        let why = match ability {
            PilotAbility::ExtraAttackDieAtRange1 if range == 1 => "point blank",
            PilotAbility::ExtraAttackDieOutsideDefenderArc
                if !self.ship_in_front_arc(content, d_idx, a_idx) =>
            {
                "outside the defender's arc"
            }
            PilotAbility::ExtraAttackDieVsDamaged
                if self.ships[d_idx].hull < self.max_hull(content, &self.ships[d_idx]) =>
            {
                "defender already damaged"
            }
            PilotAbility::StressForExtraAttackDie if self.ships[a_idx].stress == 0 => {
                self.ships[a_idx].stress += 1;
                "takes stress"
            }
            _ => return 0,
        };
        events.push(format!("{}: ability — +1 attack die ({why})", self.label(content, a_idx)));
        1
    }

    /// Attacker-side free result changes before tokens are spent.
    /// Poe Dameron: with a focus token held, one focus result becomes a
    /// hit (the token itself is not spent). Winged Gundark: at Range 1
    /// one hit becomes a critical hit.
    fn free_attack_mods(
        &self,
        content: &Content,
        a_idx: usize,
        range: u8,
        faces: &mut [AttackFace],
        events: &mut Vec<String>,
    ) {
        match self.ability(content, &self.ships[a_idx]) {
            Some(PilotAbility::FocusToResult) if self.ships[a_idx].focus > 0 => {
                if let Some(f) = faces.iter_mut().find(|f| **f == AttackFace::Focus) {
                    *f = AttackFace::Hit;
                    events.push(format!(
                        "{}: ability — focus result to hit",
                        self.label(content, a_idx)
                    ));
                }
            }
            Some(PilotAbility::HitToCritAtRange1) if range == 1 => {
                if let Some(f) = faces.iter_mut().find(|f| **f == AttackFace::Hit) {
                    *f = AttackFace::Crit;
                    events.push(format!(
                        "{}: ability — hit result to critical hit",
                        self.label(content, a_idx)
                    ));
                }
            }
            _ => {}
        }
    }

    /// Free result changes printed on the weapon card being fired.
    fn weapon_attack_mods(
        &self,
        content: &Content,
        a_idx: usize,
        effect: Option<UpgradeEffect>,
        faces: &mut [AttackFace],
        events: &mut Vec<String>,
    ) {
        let mut change = |from: AttackFace, to: AttackFace, max: usize, what: &str| {
            let n = faces.iter_mut().filter(|f| **f == from).take(max).map(|f| *f = to).count();
            if n > 0 {
                events.push(format!("{}: weapon — {what}", self.label(content, a_idx)));
            }
        };
        match effect {
            // Proton Torpedoes: one focus result to a critical hit.
            Some(UpgradeEffect::TorpedoFocusToCrit) => {
                change(AttackFace::Focus, AttackFace::Crit, 1, "focus result to critical hit")
            }
            // Adv. Proton Torpedoes: up to 3 blanks to focus results.
            Some(UpgradeEffect::TorpedoBlanksToFocus) => {
                change(AttackFace::Blank, AttackFace::Focus, 3, "blanks to focus results")
            }
            // Concussion Missiles: one blank to a hit.
            Some(UpgradeEffect::MissileBlankToHit) => {
                change(AttackFace::Blank, AttackFace::Hit, 1, "blank result to hit")
            }
            // "Mangler" Cannon: one hit to a critical hit.
            Some(UpgradeEffect::CannonHitToCrit) => {
                change(AttackFace::Hit, AttackFace::Crit, 1, "hit result to critical hit")
            }
            _ => {}
        }
    }

    /// Omega Ace: spend a target lock on the defender and a focus token
    /// to turn every attack die into a critical hit. Always taken when
    /// both tokens are available — no modification can beat all crits.
    /// Returns (lock spent, focus spent).
    fn spend_for_all_crits(
        &mut self,
        content: &Content,
        a_idx: usize,
        defender: ShipId,
        faces: &mut [AttackFace],
        events: &mut Vec<String>,
    ) -> bool {
        let ship = &self.ships[a_idx];
        if self.ability(content, ship) != Some(PilotAbility::SpendLockAndFocusForAllCrits)
            || ship.lock != Some(defender)
            || ship.focus == 0
            || faces.is_empty()
        {
            return false;
        }
        self.ships[a_idx].lock = None;
        self.ships[a_idx].focus -= 1;
        faces.fill(AttackFace::Crit);
        events.push(format!(
            "{}: ability — lock and focus spent, all dice critical",
            self.label(content, a_idx)
        ));
        true
    }

    /// Defender-side free result changes before tokens are spent; only
    /// applied when damage would otherwise still land.
    fn free_defense_mods(
        &self,
        content: &Content,
        d_idx: usize,
        faces: &mut [DefenseFace],
        incoming: u8,
        events: &mut Vec<String>,
    ) {
        let evades = faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
        if evades < incoming
            && self.ability(content, &self.ships[d_idx]) == Some(PilotAbility::FocusToResult)
            && self.ships[d_idx].focus > 0
            && let Some(f) = faces.iter_mut().find(|f| **f == DefenseFace::Focus)
        {
            *f = DefenseFace::Evade;
            events.push(format!("{}: ability — focus result to evade", self.label(content, d_idx)));
        }
    }

    /// "If you are hit by an attack, discard this card": Stealth Device.
    fn discard_on_hit(&mut self, content: &Content, i: usize, events: &mut Vec<String>) {
        let discard: Vec<UpgradeId> = self.ships[i]
            .upgrades
            .iter()
            .copied()
            .filter(|u| {
                content.upgrades.upgrade(*u).and_then(|c| c.effect)
                    == Some(UpgradeEffect::AgilityPlus1DiscardWhenHit)
            })
            .collect();
        for u in discard {
            self.ships[i].upgrades.retain(|x| *x != u);
            let name = content.upgrades.upgrade(u).map(|c| c.name.clone()).unwrap_or_default();
            events.push(format!("{}: {name} discarded (hit)", self.label(content, i)));
        }
    }

    fn label(&self, _content: &Content, i: usize) -> String {
        self.ships[i].callsign.clone()
    }

    /// Rename an own ship during Placement (squad formation). Callsigns
    /// must be unique across the game so the narration stays unambiguous.
    pub fn rename(
        &mut self,
        player: PlayerId,
        ship: ShipId,
        callsign: &str,
    ) -> Result<(), Rejection> {
        if self.phase != Phase::Placement {
            return Err(Rejection::WrongPhase);
        }
        let i = self.ship_index(ship)?;
        if self.ships[i].owner != player {
            return Err(Rejection::NotYourShip);
        }
        let name = crate::ship::validate_callsign(callsign).map_err(Rejection::BadCallsign)?;
        if self.ships.iter().any(|s| s.id != ship && s.callsign.eq_ignore_ascii_case(&name)) {
            return Err(Rejection::BadCallsign(format!("{name} is already taken")));
        }
        self.ships[i].callsign = name;
        Ok(())
    }

    /// One point of normal damage: shields absorb first, then hull.
    fn damage_point(&mut self, i: usize) -> DamagePoint {
        let s = &mut self.ships[i];
        if s.shields > 0 {
            s.shields -= 1;
            DamagePoint::Shield
        } else if s.hull > 0 {
            s.hull -= 1;
            if s.hull == 0 {
                s.destroyed = true;
            }
            DamagePoint::Hull
        } else {
            DamagePoint::None
        }
    }

    /// Attach or immediately resolve one drawn crit effect. Returns any
    /// extra (shields, hull) damage it inflicted.
    fn apply_crit_effect(
        &mut self,
        content: &Content,
        i: usize,
        effect: CritEffect,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> (u8, u8) {
        let mut extra = (0u8, 0u8);
        let extra_point = |gs: &mut Self, extra: &mut (u8, u8)| match gs.damage_point(i) {
            DamagePoint::Shield => extra.0 += 1,
            DamagePoint::Hull => extra.1 += 1,
            DamagePoint::None => {}
        };
        match effect {
            CritEffect::DirectHit => {
                extra_point(self, &mut extra);
            }
            CritEffect::MinorExplosion => {
                if AttackFace::from_d8(roll()) == AttackFace::Hit {
                    events.push(format!(
                        "{}: the explosion flares — 1 more damage",
                        self.label(content, i)
                    ));
                    extra_point(self, &mut extra);
                }
            }
            CritEffect::MinorHullBreach => {
                if !self.ships[i].crits.is_empty() {
                    let removed = self.ships[i].crits.remove(0);
                    events.push(format!(
                        "{}: {} flips facedown",
                        self.label(content, i),
                        removed.name()
                    ));
                }
            }
            persistent => self.ships[i].crits.push(persistent),
        }
        if self.ships[i].destroyed {
            events.push(format!("{}: DESTROYED by critical damage", self.label(content, i)));
        }
        extra
    }

    fn ship_index(&self, id: ShipId) -> Result<usize, Rejection> {
        self.ships.iter().position(|s| s.id == id).ok_or(Rejection::NoSuchShip)
    }

    /// Place (or re-place — allowed freely until the phase ends) a ship in
    /// the owner's deployment zone.
    pub fn place_ship(
        &mut self,
        content: &Content,
        player: PlayerId,
        ship_id: ShipId,
        pose: Pose,
    ) -> Result<(), Rejection> {
        if self.phase != Phase::Placement {
            return Err(Rejection::WrongPhase);
        }
        let i = self.ship_index(ship_id)?;
        if self.ships[i].owner != player {
            return Err(Rejection::NotYourShip);
        }
        let fp = self.class_of(content, &self.ships[i]).footprint;
        // Legality vs the player's OWN placed ships only — zones are
        // disjoint, and checking the opponent's would leak hidden info.
        let own_placed: Vec<(ShipId, Pose, Footprint)> = self
            .ships
            .iter()
            .filter(|s| s.owner == player && s.id != ship_id)
            .filter_map(|s| s.pose.map(|p| (s.id, p, self.class_of(content, s).footprint)))
            .collect();
        rules::placement_legal(&self.board, Self::seat_of(player), pose, fp, &own_placed).map_err(
            |e| match e {
                rules::PlacementError::OutOfZone => Rejection::OutOfZone,
                rules::PlacementError::OverlapsShip(_) => Rejection::OverlapsShip,
            },
        )?;
        self.ships[i].pose = Some(pose);
        if self.ships.iter().all(|s| s.pose.is_some()) {
            self.phase = Phase::Planning;
        }
        Ok(())
    }

    /// Secretly assign a dial maneuver to one of the player's ships.
    pub fn plan_maneuver(
        &mut self,
        content: &Content,
        player: PlayerId,
        ship_id: ShipId,
        index: u8,
    ) -> Result<(), Rejection> {
        if self.phase != Phase::Planning {
            return Err(Rejection::WrongPhase);
        }
        if self.committed[player.0 as usize] {
            return Err(Rejection::AlreadyCommitted);
        }
        let i = self.ship_index(ship_id)?;
        if self.ships[i].owner != player {
            return Err(Rejection::NotYourShip);
        }
        if self.ships[i].destroyed {
            return Err(Rejection::ShipDestroyed);
        }
        let class = self.class_of(content, &self.ships[i]);
        let dial = &content.dials.set(class.maneuver_set).expect("validated in new()").maneuvers;
        let man = *dial.get(index as usize).ok_or(Rejection::BadManeuverIndex)?;
        // Crits can make normally-white maneuvers red (Damaged Engine /
        // Thrust Control Fire) — the stress rule uses the effective color.
        let difficulty = crit::effective_difficulty(&self.ships[i].crits, &man);
        if self.ships[i].stress > 0 && difficulty == Difficulty::Hard {
            return Err(Rejection::StressedRedForbidden);
        }
        self.ships[i].plan = Some(index);
        Ok(())
    }

    /// Secretly assign the ship's one action, executed right after its
    /// maneuver (defaults to Pass if never planned).
    pub fn plan_action(
        &mut self,
        content: &Content,
        player: PlayerId,
        ship_id: ShipId,
        planned: PlannedAction,
    ) -> Result<(), Rejection> {
        if self.phase != Phase::Planning {
            return Err(Rejection::WrongPhase);
        }
        if self.committed[player.0 as usize] {
            return Err(Rejection::AlreadyCommitted);
        }
        let i = self.ship_index(ship_id)?;
        if self.ships[i].owner != player {
            return Err(Rejection::NotYourShip);
        }
        if self.ships[i].destroyed {
            return Err(Rejection::ShipDestroyed);
        }
        if let Some(kind) = planned.kind()
            && !self.action_bar(content, &self.ships[i]).contains(&kind)
        {
            return Err(Rejection::ActionNotOnBar);
        }
        if let PlannedAction::TargetLock(target) = planned {
            let t = self.ship_index(target)?;
            if self.ships[t].owner == player || self.ships[t].destroyed {
                return Err(Rejection::BadLockTarget);
            }
        }
        self.ships[i].planned_action = Some(planned);
        Ok(())
    }

    /// Commit the player's plans. When both players have committed, the
    /// whole turn resolves with the automatic target policy (locked ship,
    /// else nearest eligible) — used by tests and offline play. Servers
    /// wanting the interactive Declare Target step use
    /// `commit_plans_begin` + `combat_step` + `declare_target` instead.
    pub fn commit_plans(
        &mut self,
        content: &Content,
        player: PlayerId,
        roll: &mut dyn FnMut() -> u8,
    ) -> Result<Option<TurnRecords>, Rejection> {
        if self.commit_plans_begin(content, player, roll)?.is_none() {
            return Ok(None);
        }
        loop {
            match self.combat_step(content, roll)? {
                CombatStep::NeedTarget(p) => {
                    let (target, weapon) = self.auto_target(&p).expect("options are non-empty");
                    self.declare_target(content, p.owner, target, weapon, roll)?;
                }
                CombatStep::Attack(_) => {}
                CombatStep::Done(rec) => return Ok(Some(rec)),
            }
        }
    }

    /// Commit the player's plans. When both players have committed, the
    /// Activation phase resolves immediately (movement + actions, plus
    /// Console Fire burns) and the game enters `Phase::Combat`; the
    /// returned moves and events can be sent to clients right away.
    pub fn commit_plans_begin(
        &mut self,
        content: &Content,
        player: PlayerId,
        roll: &mut dyn FnMut() -> u8,
    ) -> Result<Option<ActivationRecords>, Rejection> {
        if self.phase != Phase::Planning {
            return Err(Rejection::WrongPhase);
        }
        let seat = player.0 as usize;
        if self.committed[seat] {
            return Err(Rejection::AlreadyCommitted);
        }
        let incomplete =
            self.ships.iter().any(|s| s.owner == player && !s.destroyed && s.plan.is_none());
        if incomplete {
            return Err(Rejection::PlansIncomplete);
        }
        self.committed[seat] = true;
        if self.committed != [true, true] {
            return Ok(None);
        }
        let (moves, events) = self.resolve_movement(content, roll);

        // Combat order: highest pilot skill first (initiative breaks
        // ties), grouped by skill; each group's survivors are fixed when
        // the group starts (the simultaneous-attack rule).
        let combatants: Vec<(ShipId, u8, PlayerId)> = self
            .ships
            .iter()
            .filter(|s| !s.destroyed && s.pose.is_some())
            .map(|s| (s.id, self.effective_skill(content, s), s.owner))
            .collect();
        let skill_of = |id: ShipId| {
            combatants.iter().find(|(s, _, _)| *s == id).map(|(_, k, _)| *k).unwrap_or(0)
        };
        let mut groups: Vec<Vec<ShipId>> = Vec::new();
        for id in combat_order(&combatants, self.initiative) {
            match groups.last_mut() {
                Some(g) if skill_of(g[0]) == skill_of(id) => g.push(id),
                _ => groups.push(vec![id]),
            }
        }
        self.phase = Phase::Combat;
        self.combat = Some(CombatState {
            groups,
            current: Vec::new(),
            pending: None,
            attacks: Vec::new(),
            events: events.clone(),
            moves: moves.clone(),
        });
        Ok(Some(ActivationRecords { moves, events }))
    }

    /// Narrated events of the turn so far (for streaming deltas).
    pub fn combat_events(&self) -> &[String] {
        self.combat.as_ref().map(|c| c.events.as_slice()).unwrap_or(&[])
    }

    /// Advance the Combat phase until it needs a Declare Target choice,
    /// resolves one attack, or finishes the turn (End phase applied).
    pub fn combat_step(
        &mut self,
        content: &Content,
        roll: &mut dyn FnMut() -> u8,
    ) -> Result<CombatStep, Rejection> {
        if self.phase != Phase::Combat {
            return Err(Rejection::WrongPhase);
        }
        loop {
            let cs = self.combat.as_mut().ok_or(Rejection::WrongPhase)?;
            if let Some(p) = &cs.pending {
                return Ok(CombatStep::NeedTarget(p.clone()));
            }
            if cs.current.is_empty() {
                if cs.groups.is_empty() {
                    let cs = self.combat.take().expect("checked above");
                    self.finish_turn();
                    return Ok(CombatStep::Done(TurnRecords {
                        moves: cs.moves,
                        attacks: cs.attacks,
                        events: cs.events,
                    }));
                }
                let group = cs.groups.remove(0);
                let ships = &self.ships;
                cs.current = group
                    .into_iter()
                    .filter(|&id| ships.iter().any(|s| s.id == id && !s.destroyed))
                    .collect();
                continue;
            }
            let attacker = cs.current.remove(0);
            let Some(a_idx) = self.ships.iter().position(|s| s.id == attacker) else {
                continue;
            };
            let owner = self.ships[a_idx].owner;
            let options = self.attack_options(content, a_idx);
            match options.len() {
                0 => continue,
                1 => {
                    let o = options[0].clone();
                    let d_idx = self.ships.iter().position(|s| s.id == o.target).expect("option");
                    let mut ev = Vec::new();
                    let shot = Shot { d_idx, range: o.range, weapon: o.weapon };
                    let rec = self.perform_attack_on(content, a_idx, shot, roll, &mut ev);
                    let cs = self.combat.as_mut().expect("in combat");
                    cs.events.extend(ev);
                    cs.attacks.push(rec.clone());
                    return Ok(CombatStep::Attack(rec));
                }
                _ => {
                    let p = PendingAttack { attacker, owner, options };
                    self.combat.as_mut().expect("in combat").pending = Some(p.clone());
                    return Ok(CombatStep::NeedTarget(p));
                }
            }
        }
    }

    /// The owner's Declare Target choice for the pending attack: which
    /// enemy, and with which weapon (None = primary).
    pub fn declare_target(
        &mut self,
        content: &Content,
        player: PlayerId,
        target: ShipId,
        weapon: Option<UpgradeId>,
        roll: &mut dyn FnMut() -> u8,
    ) -> Result<AttackRecord, Rejection> {
        if self.phase != Phase::Combat {
            return Err(Rejection::WrongPhase);
        }
        let pending = self
            .combat
            .as_ref()
            .and_then(|c| c.pending.clone())
            .ok_or(Rejection::NoPendingAttack)?;
        if pending.owner != player {
            return Err(Rejection::NotYourShip);
        }
        let range = pending
            .options
            .iter()
            .find(|o| o.target == target && o.weapon == weapon)
            .ok_or(Rejection::BadTarget)?
            .range;
        let a_idx = self.ship_index(pending.attacker)?;
        let d_idx = self.ship_index(target)?;
        let mut ev = Vec::new();
        let shot = Shot { d_idx, range, weapon };
        let rec = self.perform_attack_on(content, a_idx, shot, roll, &mut ev);
        let cs = self.combat.as_mut().expect("checked above");
        cs.pending = None;
        cs.events.extend(ev);
        cs.attacks.push(rec.clone());
        Ok(rec)
    }

    /// Automatic choice: the primary weapon when it has any target
    /// (never spends ordnance unasked), at the locked ship if eligible,
    /// else the nearest. Returns (target, weapon).
    pub fn auto_target(&self, p: &PendingAttack) -> Option<(ShipId, Option<UpgradeId>)> {
        let lock = self.ships.iter().find(|s| s.id == p.attacker).and_then(|s| s.lock);
        let nearest = |it: &mut dyn Iterator<Item = &AttackOption>| {
            it.min_by(|a, b| a.dist.partial_cmp(&b.dist).unwrap_or(std::cmp::Ordering::Equal))
                .cloned()
        };
        let primary: Vec<&AttackOption> = p.options.iter().filter(|o| o.weapon.is_none()).collect();
        let pick = if primary.is_empty() {
            nearest(&mut p.options.iter())
        } else {
            primary
                .iter()
                .find(|o| Some(o.target) == lock)
                .map(|o| (*o).clone())
                .or_else(|| nearest(&mut primary.into_iter()))
        };
        pick.map(|o| (o.target, o.weapon))
    }

    /// Activation phase: reveal and fly all plans in movement order
    /// (lowest pilot skill first) with their actions, then the Console
    /// Fire burns that open the Combat phase.
    fn resolve_movement(
        &mut self,
        content: &Content,
        roll: &mut dyn FnMut() -> u8,
    ) -> (Vec<MoveRecord>, Vec<String>) {
        let order = movement_order(
            &self
                .ships
                .iter()
                .filter(|s| !s.destroyed && s.pose.is_some())
                .map(|s| (s.id, self.effective_skill(content, s), s.owner))
                .collect::<Vec<_>>(),
            self.initiative,
        );
        let mut records = Vec::new();
        let mut events: Vec<String> = Vec::new();
        for id in order {
            let i = self.ship_index(id).expect("ordered ids exist");
            let (fp, dial_id) = {
                let class = self.class_of(content, &self.ships[i]);
                (class.footprint, class.maneuver_set)
            };
            let dial = &content.dials.set(dial_id).expect("validated").maneuvers;
            let mut man = dial[self.ships[i].plan.expect("commit checked plans") as usize];
            // p.17: a ship that is ALREADY stressed when it reveals a red
            // maneuver doesn't fly it — the opposing player picks any
            // non-red replacement. Unreachable through normal play until
            // external stress sources exist (planning red while stressed
            // is rejected), and automated here as the slowest white
            // straight, an adversarial stand-in for the opponent's choice.
            // PROVISIONAL: the user may replace this policy.
            if self.ships[i].stress > 0
                && crit::effective_difficulty(&self.ships[i].crits, &man) == Difficulty::Hard
                && let Some(sub) = substitute_non_red(dial, &self.ships[i].crits)
            {
                man = sub;
            }
            let start = self.ships[i].pose.expect("placed");
            let path = maneuver::sample_path(start, man).expect("validated at plan time");

            // Everyone else still on the board, as obstacles.
            let obstacles: Vec<_> = self
                .ships
                .iter()
                .filter(|s| s.id != id && !s.destroyed)
                .filter_map(|s| {
                    s.pose.map(|p| rules::footprint_corners(p, self.class_of(content, s).footprint))
                })
                .collect();

            // Core rules p.17: ships move THROUGH occupied space freely —
            // only the FINAL position matters. A K-turn (or Tallon roll)
            // that would end overlapping is executed as the plain maneuver
            // of the same speed instead (no flip). If the final position
            // (still) overlaps, back up along the template to the last
            // clear pose; that ship "bumped" and forfeits its action.
            let overlaps = |pose: Pose| {
                let c = rules::footprint_corners(pose, fp);
                obstacles.iter().any(|oc| rules::obbs_overlap(&c, oc))
            };
            let mut used_path = path;
            if overlaps(*used_path.last().expect("paths are non-empty")) {
                let degraded = match man.steer {
                    maneuver::Steer::KTurn => Some(maneuver::Steer::Straight),
                    maneuver::Steer::TallonLeft => Some(maneuver::Steer::TurnLeft),
                    maneuver::Steer::TallonRight => Some(maneuver::Steer::TurnRight),
                    maneuver::Steer::SegnorLeft => Some(maneuver::Steer::BankLeft),
                    maneuver::Steer::SegnorRight => Some(maneuver::Steer::BankRight),
                    _ => None,
                };
                if let Some(steer) = degraded
                    && let Ok(p2) = maneuver::sample_path(start, Maneuver { steer, ..man })
                {
                    used_path = p2;
                }
            }
            let mut stop = used_path.len() - 1;
            let mut bumped = false;
            while stop > 0 && overlaps(used_path[stop]) {
                stop -= 1;
                bumped = true;
            }
            let end = used_path[stop];
            let fled = !rules::within_board(&self.board, &rules::footprint_corners(end, fp));

            {
                let ship = &mut self.ships[i];
                ship.pose = Some(end);
                ship.plan = None;
                // Stress by the EFFECTIVE color (crits can redden a maneuver).
                match crit::effective_difficulty(&ship.crits, &man) {
                    Difficulty::Hard => ship.stress += 1,
                    Difficulty::Easy => ship.stress = ship.stress.saturating_sub(1),
                    Difficulty::Normal => {}
                }
                if fled {
                    ship.destroyed = true;
                }
            }

            // Stunned Pilot: bumping costs a point of damage.
            if bumped
                && !self.ships[i].destroyed
                && self.ships[i].crits.contains(&CritEffect::StunnedPilot)
            {
                let label = self.label(content, i);
                self.damage_point(i);
                let died = if self.ships[i].destroyed { " — DESTROYED" } else { "" };
                events.push(format!("{label}: Stunned Pilot — 1 damage from the collision{died}"));
            }
            let destroyed = self.ships[i].destroyed;

            // Perform Action step: one action, right after moving. Stress,
            // bumping, destruction, or damaged sensors all forfeit it.
            let planned = self.ships[i].planned_action.take().unwrap_or(PlannedAction::Pass);
            let action_result = if destroyed {
                ActionResult::Failed
            } else if self.ships[i].stress > 0 {
                ActionResult::SkippedStressed
            } else if bumped {
                ActionResult::SkippedBumped
            } else if planned != PlannedAction::Pass
                && self.ships[i].crits.contains(&CritEffect::DamagedSensorArray)
            {
                ActionResult::SkippedDamaged
            } else {
                match planned {
                    PlannedAction::Pass => ActionResult::Performed,
                    PlannedAction::Focus => {
                        self.ships[i].focus += 1;
                        ActionResult::Performed
                    }
                    PlannedAction::Evade => {
                        self.ships[i].evade += 1;
                        ActionResult::Performed
                    }
                    PlannedAction::BarrelRoll(side) => {
                        let candidate = action::barrel_roll_pose(end, fp, side);
                        let corners = rules::footprint_corners(candidate, fp);
                        let clear = rules::within_board(&self.board, &corners)
                            && obstacles.iter().all(|oc| !rules::obbs_overlap(&corners, oc));
                        if clear {
                            self.ships[i].pose = Some(candidate);
                            ActionResult::Performed
                        } else {
                            ActionResult::Failed
                        }
                    }
                    PlannedAction::Boost(dir) => {
                        // Not a maneuver: no stress interaction. Blocked if
                        // it would overlap a ship or leave the board.
                        let boosted = maneuver::apply(
                            self.ships[i].pose.expect("just set"),
                            action::boost_maneuver(dir),
                        );
                        match boosted {
                            Ok(candidate) => {
                                let corners = rules::footprint_corners(candidate, fp);
                                let clear = rules::within_board(&self.board, &corners)
                                    && obstacles
                                        .iter()
                                        .all(|oc| !rules::obbs_overlap(&corners, oc));
                                if clear {
                                    self.ships[i].pose = Some(candidate);
                                    ActionResult::Performed
                                } else {
                                    ActionResult::Failed
                                }
                            }
                            Err(_) => ActionResult::Failed,
                        }
                    }
                    PlannedAction::TargetLock(target) => {
                        let in_range = self
                            .ship_index(target)
                            .ok()
                            .and_then(|t| {
                                let ts = &self.ships[t];
                                if ts.destroyed {
                                    return None;
                                }
                                let tp = ts.pose?;
                                let tfp = self.class_of(content, ts).footprint;
                                let my = rules::footprint_corners(
                                    self.ships[i].pose.expect("just set"),
                                    fp,
                                );
                                combat::range_band_between(&my, &rules::footprint_corners(tp, tfp))
                            })
                            .is_some();
                        if in_range {
                            self.ships[i].lock = Some(target);
                            ActionResult::Performed
                        } else {
                            ActionResult::Failed
                        }
                    }
                }
            };

            let stress = self.ships[i].stress;
            records.push(MoveRecord {
                ship: id,
                maneuver: man,
                path: used_path[..=stop].to_vec(),
                end,
                bumped,
                destroyed,
                stress,
                action: planned,
                action_result,
            });
        }

        // Console Fire burns at the start of each Combat phase: one attack
        // die per burning ship, 1 damage on a Hit.
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed || self.ships[i].pose.is_none() {
                continue;
            }
            if self.ships[i].crits.contains(&CritEffect::ConsoleFire)
                && AttackFace::from_d8(roll()) == AttackFace::Hit
            {
                let label = self.label(content, i);
                self.damage_point(i);
                let died = if self.ships[i].destroyed { " — DESTROYED" } else { "" };
                events.push(format!("{label}: Console Fire burns for 1{died}"));
            }
        }

        (records, events)
    }

    /// End phase and turn bookkeeping once combat is complete.
    fn finish_turn(&mut self) {
        // End phase: unspent focus and evade tokens are removed from all
        // ships; target locks persist, except locks on ships that are now
        // destroyed. Timed crits (Weapons Failure) tick down here.
        let dead: Vec<ShipId> = self.ships.iter().filter(|s| s.destroyed).map(|s| s.id).collect();
        for ship in &mut self.ships {
            ship.focus = 0;
            ship.evade = 0;
            if let Some(l) = ship.lock
                && dead.contains(&l)
            {
                ship.lock = None;
            }
            for c in ship.crits.iter_mut() {
                if let CritEffect::WeaponsFailure { rounds } = c {
                    *rounds = rounds.saturating_sub(1);
                }
            }
            ship.crits.retain(|c| !matches!(c, CritEffect::WeaponsFailure { rounds: 0 }));
        }

        self.committed = [false, false];
        self.turn += 1;
        let alive = |p: u32| self.ships.iter().any(|s| s.owner == PlayerId(p) && !s.destroyed);
        match (alive(0), alive(1)) {
            (true, true) => self.phase = Phase::Planning,
            (true, false) => {
                self.phase = Phase::GameOver;
                self.winner = Some(PlayerId(0));
            }
            (false, true) => {
                self.phase = Phase::GameOver;
                self.winner = Some(PlayerId(1));
            }
            (false, false) => {
                // Both final ships destroyed simultaneously: the player
                // with initiative wins (core rules p.13).
                self.phase = Phase::GameOver;
                self.winner = Some(self.initiative);
            }
        }
    }

    /// Eligible targets for an attacker: enemy, alive, any part of its
    /// base in the firing arc, range 1-3, bases not touching. Empty when
    /// the attacker's weapons have failed.
    /// Every (weapon, target) pair the ship may attack with right now:
    /// the primary weapon at Range 1-3 inside the arc (all around for a
    /// turret primary), plus each equipped secondary weapon within its
    /// printed range band — Turret cards ignore the arc, other slots
    /// need it — whose token requirement is met (a lock on that target,
    /// or a focus token). Touching ships cannot be targeted.
    fn attack_options(&self, content: &Content, a_idx: usize) -> Vec<AttackOption> {
        if self.ships[a_idx].crits.iter().any(|c| matches!(c, CritEffect::WeaponsFailure { .. })) {
            return Vec::new();
        }
        let Some(a_pose) = self.ships[a_idx].pose else { return Vec::new() };
        let class = self.class_of(content, &self.ships[a_idx]);
        let a_fp = class.footprint;
        let a_corners = rules::footprint_corners(a_pose, a_fp);
        // (weapon, min range, max range, needs arc, requirement)
        let mut weapons: Vec<(Option<UpgradeId>, u8, u8, bool, AttackRequirement)> =
            vec![(None, 1, 3, !class.turret_primary, AttackRequirement::Free)];
        for &u in &self.ships[a_idx].upgrades {
            if let Some(card) = content.upgrades.upgrade(u)
                && let Some(sw) = card.attack
                && matches!(card.slot, Slot::Torpedo | Slot::Missile | Slot::Cannon | Slot::Turret)
            {
                weapons.push((
                    Some(u),
                    sw.range_min,
                    sw.range_max,
                    card.slot != Slot::Turret,
                    sw.requires,
                ));
            }
        }
        let mut options = Vec::new();
        for s in &self.ships {
            if s.owner == self.ships[a_idx].owner || s.destroyed {
                continue;
            }
            let Some(pose) = s.pose else { continue };
            let fp = self.class_of(content, s).footprint;
            let corners = rules::footprint_corners(pose, fp);
            let dist = combat::base_distance(&a_corners, &corners);
            if dist <= 0.0 {
                continue; // touching bases cannot be targeted
            }
            let Some(band) = combat::range_band_between(&a_corners, &corners) else {
                continue;
            };
            let in_arc = Self::base_in_front_arc(a_pose, a_fp, &corners);
            for &(weapon, lo, hi, needs_arc, req) in &weapons {
                if band < lo || band > hi || (needs_arc && !in_arc) {
                    continue;
                }
                let armed = match req {
                    AttackRequirement::Free => true,
                    AttackRequirement::TargetLock => self.ships[a_idx].lock == Some(s.id),
                    AttackRequirement::Focus => self.ships[a_idx].focus > 0,
                };
                if armed {
                    options.push(AttackOption { weapon, target: s.id, range: band, dist });
                }
            }
        }
        options
    }

    /// Resolve one declared attack (dice, token spending, damage, crits).
    /// Token policy: spend the lock to reroll misses, focus when eyes
    /// matter, evade when damage would otherwise land.
    fn perform_attack_on(
        &mut self,
        content: &Content,
        a_idx: usize,
        shot: Shot,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> AttackRecord {
        let Shot { d_idx, range, weapon } = shot;
        let attacker = self.ships[a_idx].id;
        let defender = self.ships[d_idx].id;
        let a_pose = self.ships[a_idx].pose.expect("attackers are on the board");

        // Secondary weapon: its own dice, no range bonuses either way,
        // and the required token is spent up front when the card says so.
        let secondary = weapon
            .and_then(|u| content.upgrades.upgrade(u))
            .and_then(|c| c.attack.map(|sw| (c.name.clone(), sw)));
        let mut lock_spent = false;
        let mut attacker_focus_spent = false;
        if let Some((name, sw)) = &secondary {
            events.push(format!("{}: fires {name}", self.label(content, a_idx)));
            if sw.spend {
                match sw.requires {
                    AttackRequirement::TargetLock => {
                        self.ships[a_idx].lock = None;
                        lock_spent = true;
                    }
                    AttackRequirement::Focus => {
                        self.ships[a_idx].focus = self.ships[a_idx].focus.saturating_sub(1);
                        attacker_focus_spent = true;
                    }
                    AttackRequirement::Free => {}
                }
            }
        }
        let a_dice = match &secondary {
            Some((_, sw)) => sw.dice,
            None => self.printed(content, &self.ships[a_idx]).attack,
        };
        let range_bonus = u8::from(range == 1 && secondary.is_none());
        let weapon_effect = weapon.and_then(|u| content.upgrades.upgrade(u)).and_then(|c| c.effect);
        // Dorsal Turret: +1 die at Range 1. Proton Rockets: + agility (max 3).
        let weapon_extra = match weapon_effect {
            Some(UpgradeEffect::TurretDorsalExtraDieAtRange1) if range == 1 => 1,
            Some(UpgradeEffect::RocketExtraDiceByAgility) => {
                self.agility(content, &self.ships[a_idx]).min(3)
            }
            _ => 0,
        };
        if weapon_extra > 0 {
            events.push(format!(
                "{}: +{weapon_extra} attack dice (weapon)",
                self.label(content, a_idx)
            ));
        }

        // Roll attack dice (+1 at range 1). Weapon Malfunction drops one
        // die per copy; a Blinded Pilot fires 0 dice once, then recovers.
        let blinded = self.ships[a_idx].crits.contains(&CritEffect::BlindedPilot);
        let malfunctions = self.ships[a_idx]
            .crits
            .iter()
            .filter(|c| matches!(c, CritEffect::WeaponMalfunction))
            .count() as u8;
        let n_atk = if blinded {
            let pos = self.ships[a_idx]
                .crits
                .iter()
                .position(|c| matches!(c, CritEffect::BlindedPilot))
                .expect("checked above");
            self.ships[a_idx].crits.remove(pos);
            events.push(format!(
                "{}: Blinded Pilot — fires wildly (0 dice), vision clears",
                self.label(content, a_idx)
            ));
            0
        } else {
            let extra = self.extra_attack_dice(content, a_idx, d_idx, range, events);
            (a_dice + range_bonus + extra + weapon_extra).saturating_sub(malfunctions)
        };
        let mut attack_faces: Vec<AttackFace> =
            (0..n_atk).map(|_| AttackFace::from_d8(roll())).collect();
        // Heavy Laser Cannon: crits become hits immediately after rolling.
        if weapon_effect == Some(UpgradeEffect::CannonCritsToHits) {
            for f in attack_faces.iter_mut().filter(|f| **f == AttackFace::Crit) {
                *f = AttackFace::Hit;
            }
        }

        // Denials. Omega Leader: an enemy he has locked cannot modify any
        // dice against him, and cannot modify any when he attacks it.
        // Dark Curse: attackers cannot spend focus tokens or reroll.
        let omega = PilotAbility::LockedEnemiesCannotModifyDice;
        let attacker_may_modify = !(self.ability(content, &self.ships[d_idx]) == Some(omega)
            && self.ships[d_idx].lock == Some(attacker));
        let defender_may_modify = !(self.ability(content, &self.ships[a_idx]) == Some(omega)
            && self.ships[a_idx].lock == Some(defender));
        let dark_curse = self.ability(content, &self.ships[d_idx])
            == Some(PilotAbility::DefenderDeniesFocusAndRerolls);
        let attacker_may_spend = attacker_may_modify && !dark_curse;
        if !attacker_may_modify {
            events.push(format!(
                "{}: ability — locked attacker cannot modify dice",
                self.label(content, d_idx)
            ));
        } else if dark_curse
            && (self.ships[a_idx].focus > 0 || self.ships[a_idx].lock == Some(defender))
        {
            events.push(format!(
                "{}: ability — attacker cannot spend focus or reroll",
                self.label(content, d_idx)
            ));
        }
        if !defender_may_modify {
            events.push(format!(
                "{}: ability — locked defender cannot modify dice",
                self.label(content, a_idx)
            ));
        }

        // Modify attack: spend the lock to reroll blanks (and eyes too if
        // no focus token is held), then free ability conversions, then
        // focus converts the remaining eyes to hits.
        let all_crits = attacker_may_spend
            && self.spend_for_all_crits(content, a_idx, defender, &mut attack_faces, events);
        lock_spent |= all_crits;
        if attacker_may_spend && !all_crits && self.ships[a_idx].lock == Some(defender) {
            let reroll_eyes = self.ships[a_idx].focus == 0;
            let mut any = false;
            for f in attack_faces.iter_mut() {
                if *f == AttackFace::Blank || (reroll_eyes && *f == AttackFace::Focus) {
                    *f = AttackFace::from_d8(roll());
                    any = true;
                }
            }
            if any {
                self.ships[a_idx].lock = None;
                lock_spent = true;
            }
        }
        if attacker_may_spend && !all_crits {
            let n = self.friendly_rerolls(content, a_idx, true);
            self.reroll_attack_dice(content, a_idx, &mut attack_faces, n, roll, events);
        }
        if attacker_may_modify {
            self.free_attack_mods(content, a_idx, range, &mut attack_faces, events);
            self.weapon_attack_mods(content, a_idx, weapon_effect, &mut attack_faces, events);
        }
        attacker_focus_spent |= all_crits;
        if attacker_may_spend
            && self.ships[a_idx].focus > 0
            && attack_faces.contains(&AttackFace::Focus)
        {
            self.ships[a_idx].focus -= 1;
            attacker_focus_spent = true;
            for f in attack_faces.iter_mut() {
                if *f == AttackFace::Focus {
                    *f = AttackFace::Hit;
                }
            }
        }
        let raw_hits = attack_faces.iter().filter(|f| **f == AttackFace::Hit).count() as u8;
        let raw_crits = attack_faces.iter().filter(|f| **f == AttackFace::Crit).count() as u8;

        // Roll defense dice (+1 at range 3 vs primary weapons).
        let n_def =
            self.agility(content, &self.ships[d_idx]) + u8::from(range == 3 && secondary.is_none());
        let mut defense_faces: Vec<DefenseFace> =
            (0..n_def).map(|_| DefenseFace::from_d8(roll())).collect();

        // A defender inside the attacker's bullseye lane cannot spend
        // focus or evade tokens to defend.
        let defender_in_bullseye = {
            let d_pose = self.ships[d_idx].pose.expect("candidates are placed");
            let d_fp = self.class_of(content, &self.ships[d_idx]).footprint;
            combat::in_bullseye(a_pose, &rules::footprint_corners(d_pose, d_fp))
        };

        // Modify defense: focus converts eyes when it helps, evade token
        // adds one evade result if damage would still land.
        let incoming = raw_hits + raw_crits;
        if defender_may_modify {
            let evading = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
            if evading < incoming {
                let n = self.friendly_rerolls(content, d_idx, false);
                self.reroll_defense_dice(content, d_idx, &mut defense_faces, n, roll, events);
            }
            self.free_defense_mods(content, d_idx, &mut defense_faces, incoming, events);
        }
        let mut evades = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
        let mut defender_focus_spent = false;
        let eyes = defense_faces.iter().filter(|f| **f == DefenseFace::Focus).count() as u8;
        let defender_may_spend = defender_may_modify && !defender_in_bullseye;
        if defender_may_spend && self.ships[d_idx].focus > 0 && eyes > 0 && evades < incoming {
            self.ships[d_idx].focus -= 1;
            defender_focus_spent = true;
            for f in defense_faces.iter_mut() {
                if *f == DefenseFace::Focus {
                    *f = DefenseFace::Evade;
                }
            }
            evades += eyes;
        }
        // Homing Missiles: the defender cannot spend evade tokens.
        let evade_allowed = weapon_effect != Some(UpgradeEffect::MissileDenyEvadeTokens);
        let mut evade_spent = false;
        if defender_may_spend && evade_allowed && self.ships[d_idx].evade > 0 && evades < incoming {
            self.ships[d_idx].evade -= 1;
            evade_spent = true;
            evades += 1;
        }

        // Compare results: evades cancel hits before crits. Autoblasters:
        // hits cannot be canceled, so evades only strike crits.
        let mut hits = raw_hits;
        let mut crits = raw_crits;
        let uncancelable = matches!(
            weapon_effect,
            Some(
                UpgradeEffect::TurretAutoblasterUncancelable
                    | UpgradeEffect::CannonUncancelableHits
            )
        );
        if uncancelable {
            crits -= evades.min(crits);
        } else {
            let canceled_hits = hits.min(evades);
            hits -= canceled_hits;
            crits -= (evades - canceled_hits).min(crits);
        }

        // Deal damage: hits before crits; shields absorb first. Only crits
        // reaching the hull are critical — each draws one effect from the
        // table (no card UI: immediates resolve now, the rest attach).
        let mut shields_lost = 0;
        let mut hull_lost = 0;
        let mut crits_to_hull = 0;
        for _ in 0..hits {
            if self.ships[d_idx].destroyed {
                break;
            }
            match self.damage_point(d_idx) {
                DamagePoint::Shield => shields_lost += 1,
                DamagePoint::Hull => hull_lost += 1,
                DamagePoint::None => {}
            }
        }
        for _ in 0..crits {
            if self.ships[d_idx].destroyed {
                break;
            }
            match self.damage_point(d_idx) {
                DamagePoint::Shield => shields_lost += 1,
                DamagePoint::Hull => {
                    hull_lost += 1;
                    crits_to_hull += 1;
                    if !self.ships[d_idx].destroyed {
                        let effect = crit::draw(roll());
                        events.push(format!(
                            "{}: critical — {}",
                            self.label(content, d_idx),
                            effect.name()
                        ));
                        let (s2, h2) = self.apply_crit_effect(content, d_idx, effect, roll, events);
                        shields_lost += s2;
                        hull_lost += h2;
                    }
                }
                DamagePoint::None => {}
            }
        }

        // "If you are hit by an attack": at least one uncanceled result.
        if hits + crits > 0 {
            self.discard_on_hit(content, d_idx, events);
        }
        // Ordnance is discarded once fired.
        if let Some((name, sw)) = &secondary
            && sw.discard_to_fire
        {
            self.ships[a_idx].upgrades.retain(|u| Some(*u) != weapon);
            events.push(format!("{}: {name} discarded (fired)", self.label(content, a_idx)));
        }
        AttackRecord {
            attacker,
            defender,
            range,
            weapon,
            attack_faces,
            defense_faces,
            lock_spent,
            attacker_focus_spent,
            defender_focus_spent,
            evade_spent,
            defender_in_bullseye,
            hits,
            crits,
            shields_lost,
            hull_lost,
            crits_to_hull,
            defender_destroyed: self.ships[d_idx].destroyed,
        }
    }

    /// Concede. Returns the winner.
    pub fn resign(&mut self, player: PlayerId) -> PlayerId {
        let other = PlayerId(1 - (player.0 & 1));
        self.phase = Phase::GameOver;
        self.winner = Some(other);
        other
    }

    /// What `viewer` is allowed to see right now.
    pub fn snapshot_for(&self, content: &Content, viewer: PlayerId) -> Vec<ShipView> {
        let pilot_name = |id| content.pilots.pilot(id).map(|p| p.name.clone()).unwrap_or_default();
        self.ships
            .iter()
            .map(|s| {
                let own = s.owner == viewer;
                ShipView {
                    id: s.id,
                    owner: s.owner,
                    class: s.class,
                    callsign: s.callsign.clone(),
                    pilot: pilot_name(s.pilot),
                    skill: self.effective_skill(content, s),
                    max_hull: self.max_hull(content, s),
                    max_shields: self.max_shields(content, s),
                    agility: self.agility(content, s),
                    actions: self.action_bar(content, s),
                    upgrades: s
                        .upgrades
                        .iter()
                        .filter_map(|u| content.upgrades.upgrade(*u))
                        .map(|u| u.name.clone())
                        .collect(),
                    pose: if own || self.phase != Phase::Placement { s.pose } else { None },
                    hull: s.hull,
                    shields: s.shields,
                    stress: s.stress,
                    focus: s.focus,
                    evade: s.evade,
                    lock: s.lock,
                    crits: s.crits.clone(),
                    destroyed: s.destroyed,
                    plan: if own { s.plan } else { None },
                    planned_action: if own { s.planned_action } else { None },
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ship::ShipClassId;
    use std::f64::consts::FRAC_PI_2;

    const TIE: ShipClassId = ShipClassId(1);
    const XWING: ShipClassId = ShipClassId(2);
    const P0: PlayerId = PlayerId(0);
    const P1: PlayerId = PlayerId(1);

    fn content() -> Content {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data");
        Content::load_dir(dir).unwrap()
    }

    fn board() -> Board {
        Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
    }

    /// Basic (cheapest generic) pilot of each class — the sandbox-era
    /// fixed fleets.
    fn fleet(c: &Content, classes: &[ShipClassId]) -> Vec<PilotId> {
        classes.iter().map(|k| c.pilots.basic_for(*k).unwrap().id).collect()
    }

    fn new_1v1(c: &Content) -> GameState {
        GameState::new(
            board(),
            c,
            [&fleet(c, &[TIE]), &fleet(c, &[XWING])],
            crate::dice::AttackFace::Hit,
        )
        .unwrap()
    }

    /// Index of a maneuver on a class's dial.
    fn dial_index(c: &Content, class: ShipClassId, m: fn(&Maneuver) -> bool) -> u8 {
        let set = c.ships.class(class).unwrap().maneuver_set;
        c.dials.set(set).unwrap().maneuvers.iter().position(m).unwrap() as u8
    }

    fn straight2(c: &Content, class: ShipClassId) -> u8 {
        dial_index(c, class, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 2)
    }

    fn place_both(c: &Content, gs: &mut GameState) {
        gs.place_ship(c, P0, ShipId(0), Pose::new(10.0, 2.0, FRAC_PI_2)).unwrap();
        gs.place_ship(c, P1, ShipId(1), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
    }

    #[test]
    fn callsigns_default_and_rename_during_placement() {
        let c = content();
        let mut gs = GameState::new(
            board(),
            &c,
            [&fleet(&c, &[TIE, TIE]), &fleet(&c, &[XWING, XWING])],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        let names: Vec<&str> = gs.ships.iter().map(|s| s.callsign.as_str()).collect();
        assert_eq!(names, ["Obsidian-leader", "Obsidian-2", "Red-leader", "Red-2"]);
        assert_eq!(gs.snapshot_for(&c, P1)[3].callsign, "Red-2");
        assert_eq!(gs.snapshot_for(&c, P1)[3].pilot, "Blue Squadron Novice");
        assert_eq!(gs.snapshot_for(&c, P1)[3].skill, 2);

        assert_eq!(gs.rename(P1, ShipId(3), "  Rogue-3 "), Ok(()));
        assert_eq!(gs.ships[3].callsign, "Rogue-3");
        assert_eq!(gs.rename(P0, ShipId(3), "Mine"), Err(Rejection::NotYourShip));
        assert!(matches!(gs.rename(P0, ShipId(0), "   "), Err(Rejection::BadCallsign(_))));
        assert!(matches!(gs.rename(P0, ShipId(0), "red-LEADER"), Err(Rejection::BadCallsign(_))));
        // Narration uses the callsign.
        assert_eq!(gs.label(&c, 3), "Rogue-3");

        for (p, id, y, h) in [(P0, 0, 2.0, FRAC_PI_2), (P0, 1, 2.0, FRAC_PI_2)] {
            gs.place_ship(&c, p, ShipId(id), Pose::new(6.0 + id as f64 * 4.0, y, h)).unwrap();
        }
        for (id, x) in [(2u32, 6.0), (3, 10.0)] {
            gs.place_ship(&c, P1, ShipId(id), Pose::new(x, 18.0, -FRAC_PI_2)).unwrap();
        }
        assert_eq!(gs.phase, Phase::Planning);
        assert_eq!(gs.rename(P0, ShipId(0), "Late"), Err(Rejection::WrongPhase));
    }

    #[test]
    fn initiative_breaks_equal_pilot_skill() {
        // Mirror match: all skill 1; P1 holds initiative.
        let ships =
            [(ShipId(0), 1, P0), (ShipId(1), 1, P0), (ShipId(2), 1, P1), (ShipId(3), 1, P1)];
        assert_eq!(movement_order(&ships, P1), vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]);
        assert_eq!(combat_order(&ships, P1), vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]);
    }

    #[test]
    fn initiative_setup_rules() {
        use crate::dice::AttackFace;
        // Lower squad total takes it outright — die irrelevant.
        assert_eq!(initiative_seat([12, 24], AttackFace::Blank), 0);
        assert_eq!(initiative_seat([48, 24], AttackFace::Hit), 1);
        // Tie: seat 0 rolls. Hit/Crit keeps, Focus/Blank hands over.
        assert_eq!(initiative_seat([24, 24], AttackFace::Hit), 0);
        assert_eq!(initiative_seat([24, 24], AttackFace::Crit), 0);
        assert_eq!(initiative_seat([24, 24], AttackFace::Focus), 1);
        assert_eq!(initiative_seat([24, 24], AttackFace::Blank), 1);
    }

    #[test]
    fn placement_flow_and_hidden_information() {
        let c = content();
        let mut gs = new_1v1(&c);
        assert_eq!(gs.phase, Phase::Placement);
        // Wrong zone rejected; opponent's ship rejected.
        assert_eq!(
            gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 10.0, 0.0)),
            Err(Rejection::OutOfZone)
        );
        assert_eq!(
            gs.place_ship(&c, P0, ShipId(1), Pose::new(10.0, 2.0, 0.0)),
            Err(Rejection::NotYourShip)
        );
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.0, FRAC_PI_2)).unwrap();
        // P1 cannot see P0's pose during placement; P0 can.
        assert!(gs.snapshot_for(&c, P1)[0].pose.is_none());
        assert!(gs.snapshot_for(&c, P0)[0].pose.is_some());
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
        assert_eq!(gs.phase, Phase::Planning);
        // Everything visible once placement ends.
        assert!(gs.snapshot_for(&c, P1)[0].pose.is_some());
    }

    #[test]
    fn full_turn_resolves_in_pilot_skill_order() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        // Opponent never sees the plan.
        assert!(gs.snapshot_for(&c, P1)[0].plan.is_none());
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        assert_eq!(gs.commit_plans(&c, P0, &mut || 7).unwrap(), None);
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        // TIE (skill 1) before X-Wing (skill 2).
        assert_eq!(moves[0].ship, ShipId(0));
        assert_eq!(moves[1].ship, ShipId(1));
        assert!((moves[0].end.anchor.y - 4.0).abs() < 1e-9);
        assert!((moves[1].end.anchor.y - 16.0).abs() < 1e-9);
        assert_eq!(gs.phase, Phase::Planning);
        assert_eq!(gs.turn, 2);
        assert_eq!(gs.committed, [false, false]);
    }

    #[test]
    fn red_maneuver_stresses_and_blue_sheds() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        let kturn3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(0), kturn3).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        assert_eq!(moves[0].stress, 1);
        // Stressed: red now forbidden, blue allowed…
        assert_eq!(
            gs.plan_maneuver(&c, P0, ShipId(0), kturn3),
            Err(Rejection::StressedRedForbidden)
        );
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        // …and the blue straight shed the token.
        assert_eq!(moves[0].stress, 0);
    }

    #[test]
    fn flying_off_the_board_destroys_the_ship() {
        let c = content();
        let mut gs = new_1v1(&c);
        // TIE faces south toward its own edge.
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
        let s3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(0), s3).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        assert!(moves[0].destroyed);
        assert_eq!(gs.phase, Phase::GameOver);
        assert_eq!(gs.winner, Some(P1));
    }

    #[test]
    fn bumping_stops_short_of_overlap() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        let s5_tie =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4_xw = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        // Turn 1: TIE 2→7, X-Wing 18→14.
        gs.plan_maneuver(&c, P0, ShipId(0), s5_tie).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4_xw).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap();
        // Turn 2: TIE tries 7→12; X-Wing hull occupies y 14..15, so a
        // straight-4 to 10 is clear, but TIE first: 7→12 is clear too.
        // Then X-Wing 14→10 must bump against the TIE hull at 11..12.
        gs.plan_maneuver(&c, P0, ShipId(0), s5_tie).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4_xw).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        let xw = moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert!(xw.bumped, "X-Wing should bump into the TIE");
        // Stopped just above the TIE's hull (anchor is its front/south end).
        assert!(xw.end.anchor.y > 12.0 && xw.end.anchor.y < 12.3, "{}", xw.end.anchor.y);
        let tie = moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert!(!tie.bumped);
    }

    #[test]
    fn action_bar_is_enforced_at_planning() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        // TIE has no TargetLock on its bar; X-Wing does.
        assert_eq!(
            gs.plan_action(&c, P0, ShipId(0), PlannedAction::TargetLock(ShipId(1))),
            Err(Rejection::ActionNotOnBar)
        );
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(0))).unwrap();
        // Locking your own ship is refused.
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(1))),
            Err(Rejection::BadLockTarget)
        );
        // Pass needs no bar entry.
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Pass).unwrap();
    }

    #[test]
    fn focus_performs_then_end_phase_clears_it() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        assert_eq!(moves[0].action, PlannedAction::Focus);
        assert_eq!(moves[0].action_result, ActionResult::Performed);
        // Unplanned action defaults to Pass.
        assert_eq!(moves[1].action, PlannedAction::Pass);
        // End phase already removed the unspent token (no combat phase yet).
        assert_eq!(gs.ships[0].focus, 0);
    }

    #[test]
    fn stress_forfeits_the_action() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        let kturn3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(0), kturn3).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        assert_eq!(moves[0].action_result, ActionResult::SkippedStressed);
    }

    #[test]
    fn barrel_roll_shifts_after_the_move() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRoll(action::Side::Left)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap().moves;
        assert_eq!(moves[0].action_result, ActionResult::Performed);
        // Straight-2 north from (10,2) → (10,4); left of north is -X,
        // shifted by template (1) + base width (1) = 2.
        let pose = gs.ships[0].pose.unwrap();
        assert!((pose.anchor.x - 8.0).abs() < 1e-9, "{}", pose.anchor.x);
        assert!((pose.anchor.y - 4.0).abs() < 1e-9);
    }

    #[test]
    fn target_lock_needs_range_and_persists() {
        let c = content();
        let mut gs = new_1v1(&c);
        // Far apart: lock fails at resolution.
        place_both(&c, &mut gs);
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(0))).unwrap();
        gs.commit_plans(&c, P0, &mut || 0).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 0).unwrap().unwrap().moves;
        // TIE at y=4, X-Wing at 16: gap 11 units — far beyond range 3.
        assert_eq!(moves[1].action_result, ActionResult::Failed);
        assert_eq!(gs.ships[1].lock, None);
        // Close the distance: TIE 4→9, X-Wing 16→12 (gap 9→wait: hulls
        // TIE [8,9], XW [12,13] → 3 units = range 2). Lock succeeds.
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4).unwrap();
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(0))).unwrap();
        gs.commit_plans(&c, P0, &mut || 0).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 0).unwrap().unwrap().moves;
        assert_eq!(moves[1].action_result, ActionResult::Performed);
        assert_eq!(gs.ships[1].lock, Some(ShipId(0)));
        // Locks persist through the End phase and the next turn.
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 0).unwrap();
        gs.commit_plans(&c, P1, &mut || 0).unwrap().unwrap();
        assert_eq!(gs.ships[1].lock, Some(ShipId(0)));
    }

    #[test]
    fn ships_move_through_others_when_final_position_is_clear() {
        let c = content();
        // Two TIEs south, one X-Wing north; TIE #0 flies straight through
        // the space occupied by TIE #1 and lands cleanly beyond it.
        let mut gs = GameState::new(
            board(),
            &c,
            [&fleet(&c, &[TIE, TIE]), &fleet(&c, &[XWING])],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 1.15, FRAC_PI_2)).unwrap();
        // Blocker faces east across #0's path (hull y 2.0-3.0).
        gs.place_ship(&c, P0, ShipId(1), Pose::new(10.0, 2.5, 0.0)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
        let s3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 3);
        let s1 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 1);
        gs.plan_maneuver(&c, P0, ShipId(0), s3).unwrap();
        gs.plan_maneuver(&c, P0, ShipId(1), s1).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(2), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 6).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 6).unwrap().unwrap().moves;
        let mover = moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert!(!mover.bumped, "final position is clear: passing through is legal");
        assert!((mover.end.anchor.y - 4.15).abs() < 1e-9, "{}", mover.end.anchor.y);
    }

    #[test]
    fn kturn_ending_in_overlap_becomes_a_straight_without_the_flip() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        let tie_s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let tie_s2 = straight2(&c, TIE);
        let xw_s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        let xw_k4 =
            dial_index(&c, XWING, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 4);
        // Turn 1: TIE 2→7, X-Wing 18→14.
        gs.plan_maneuver(&c, P0, ShipId(0), tie_s5).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), xw_s4).unwrap();
        gs.commit_plans(&c, P0, &mut || 6).unwrap();
        gs.commit_plans(&c, P1, &mut || 6).unwrap().unwrap();
        // Turn 2: TIE moves to 9 (hull 8-9); the X-Wing's K-turn to 10
        // would flip and overlap (flipped hull 9-10), so it executes as a
        // plain straight-4 instead: same spot, heading unchanged, and the
        // red maneuver still stresses.
        gs.plan_maneuver(&c, P0, ShipId(0), tie_s2).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), xw_k4).unwrap();
        gs.commit_plans(&c, P0, &mut || 6).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 6).unwrap().unwrap().moves;
        let xw = moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert!((xw.end.anchor.y - 10.0).abs() < 1e-9, "{}", xw.end.anchor.y);
        assert!(
            (xw.end.heading + FRAC_PI_2).abs() < 1e-9,
            "no 180° flip on a bumped K-turn: {}",
            xw.end.heading
        );
        assert!(!xw.bumped, "degraded straight lands clear");
        assert_eq!(xw.stress, 1, "the red maneuver still stresses");
    }

    #[test]
    fn boost_flies_a_one_template_after_the_move() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        // TIE has no Boost on its bar.
        assert_eq!(
            gs.plan_action(&c, P0, ShipId(0), PlannedAction::Boost(action::BoostDir::Straight)),
            Err(Rejection::ActionNotOnBar)
        );
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Boost(action::BoostDir::Straight))
            .unwrap();
        gs.commit_plans(&c, P0, &mut || 6).unwrap();
        let moves = gs.commit_plans(&c, P1, &mut || 6).unwrap().unwrap().moves;
        let xw = moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(xw.action_result, ActionResult::Performed);
        // Straight-2 (18→16) plus boost straight-1 → 15; no stress change.
        let pose = gs.ships[1].pose.unwrap();
        assert!((pose.anchor.y - 15.0).abs() < 1e-9, "{}", pose.anchor.y);
        assert_eq!(gs.ships[1].stress, 0);
    }

    /// Cycles a scripted d8 sequence.
    fn scripted(vals: Vec<u8>) -> impl FnMut() -> u8 {
        let mut i = 0;
        move || {
            let v = vals[i % vals.len()];
            i += 1;
            v
        }
    }

    /// TIE south at (10,2.5) flying straight-5 and X-Wing north at
    /// (10,17.5) flying straight-4 end nose-to-nose at range 3.
    fn fly_to_range3(c: &Content, gs: &mut GameState, rolls: &mut dyn FnMut() -> u8) {
        gs.place_ship(c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        let s5 =
            dial_index(c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4 = dial_index(c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(c, P1, ShipId(1), s4).unwrap();
        gs.commit_plans(c, P0, rolls).unwrap();
    }

    #[test]
    fn upgrades_modify_stats_action_bar_and_skill() {
        use crate::squad::{Squad, SquadShip};
        let c = content();
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let up = |x: &str| c.upgrades.upgrades.iter().find(|u| u.xws == x).unwrap().id;
        let imperial = Squad {
            name: "i".into(),
            faction: crate::ship::Faction::Empire,
            ships: vec![SquadShip {
                pilot: pilot("academypilot"),
                upgrades: vec![up("stealthdevice"), up("targetingcomputer")],
                callsign: String::new(),
            }],
        };
        let rebel = Squad {
            name: "r".into(),
            faction: crate::ship::Faction::RebelAlliance,
            ships: vec![SquadShip {
                pilot: pilot("redsquadronveteran"),
                upgrades: vec![up("veteraninstincts"), up("hullupgrade"), up("shieldupgrade")],
                callsign: String::new(),
            }],
        };
        let mut gs =
            GameState::from_squads(board(), &c, [&imperial, &rebel], crate::dice::AttackFace::Hit)
                .unwrap();
        // Starting values include Hull/Shield Upgrade; skill includes VI.
        assert_eq!((gs.ships[1].hull, gs.ships[1].shields), (4, 4));
        assert_eq!(gs.effective_skill(&c, &gs.ships[1]), 6);
        assert_eq!(gs.agility(&c, &gs.ships[0]), 4, "Stealth Device");
        let view = &gs.snapshot_for(&c, P1)[1];
        assert_eq!((view.max_hull, view.max_shields, view.skill), (4, 4, 6));
        // Same setup as fly_to_range3, with action checks in between.
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        // Targeting Computer puts target lock on the TIE's bar; the T-70
        // still has no evade.
        assert_eq!(gs.plan_action(&c, P0, ShipId(0), PlannedAction::TargetLock(ShipId(1))), Ok(()));
        assert_eq!(gs.plan_action(&c, P0, ShipId(0), PlannedAction::Pass), Ok(()));
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::Evade),
            Err(Rejection::ActionNotOnBar)
        );
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4).unwrap();
        // X-Wing (skill 6) fires first: 1 hit + 2 blanks; TIE defends with
        // 3 + 1 (Stealth) + 1 (R3) = 5 blank dice. Then the TIE fires 2
        // blanks; X-Wing defends 2 + 1 = 3 blanks.
        let mut rolls = scripted(vec![0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks[0].attacker, ShipId(1), "VI: 6 beats 1");
        assert_eq!(rec.attacks[0].hits, 1);
        assert_eq!(gs.ships[0].hull, 2);
        // Hit → Stealth Device discarded, agility back to 3.
        assert!(!gs.ships[0].upgrades.contains(&up("stealthdevice")));
        assert_eq!(gs.agility(&c, &gs.ships[0]), 3);
        assert!(
            rec.events.iter().any(|e| e.contains("Stealth Device discarded")),
            "{:?}",
            rec.events
        );
    }

    /// Two squads of one ship each, placed nose-to-nose 15 units apart
    /// (range 3 after the TIE flies 5 and the X-Wing 4), maneuvers
    /// planned, nothing committed yet — so actions can still be planned.
    fn duel(c: &Content, imperial: &str, rebel: &str) -> GameState {
        duel_at(c, imperial, rebel, Pose::new(10.0, 17.5, -FRAC_PI_2))
    }

    /// `duel` with the X-Wing's starting pose chosen by the test (the
    /// Imperial ship always starts at (10, 2.5) heading north). The
    /// X-Wing is placed legally, then moved outside the deployment zone
    /// by hand so tests can stage mid-board geometry.
    fn duel_at(c: &Content, imperial: &str, rebel: &str, xwing: Pose) -> GameState {
        use crate::squad::Squad;
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let a = Squad::basic(c, "i", &[pilot(imperial)]);
        let b = Squad::basic(c, "r", &[pilot(rebel)]);
        let mut gs =
            GameState::from_squads(board(), c, [&a, &b], crate::dice::AttackFace::Hit).unwrap();
        gs.place_ship(c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        gs.ships[1].pose = Some(xwing);
        let s5 = dial_index(c, gs.ships[0].class, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
        let s4 = dial_index(c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(c, P1, ShipId(1), s4).unwrap();
        gs
    }

    /// Several ships a side, each as (pilot xws, start pose, straight
    /// distance to fly). Imperial ships get ids 0.., Rebels follow.
    fn skirmish(
        c: &Content,
        imperial: &[(&str, Pose, u8)],
        rebel: &[(&str, Pose, u8)],
    ) -> GameState {
        use crate::squad::Squad;
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let ids =
            |side: &[(&str, Pose, u8)]| side.iter().map(|(x, _, _)| pilot(x)).collect::<Vec<_>>();
        let a = Squad::basic(c, "i", &ids(imperial));
        let b = Squad::basic(c, "r", &ids(rebel));
        let mut gs =
            GameState::from_squads(board(), c, [&a, &b], crate::dice::AttackFace::Hit).unwrap();
        let all: Vec<_> = imperial.iter().chain(rebel.iter()).collect();
        for (k, (_, pose, _)) in all.iter().enumerate() {
            let player = if k < imperial.len() { P0 } else { P1 };
            gs.place_ship(c, player, ShipId(k as u32), *pose).unwrap();
        }
        for (k, (_, _, dist)) in all.iter().enumerate() {
            let player = if k < imperial.len() { P0 } else { P1 };
            let id = ShipId(k as u32);
            let set = c.ships.class(gs.ships[k].class).unwrap().maneuver_set;
            let m =
                c.dials.set(set).unwrap().maneuvers.iter().position(|m| {
                    m.steer == crate::maneuver::Steer::Straight && m.distance == *dist
                });
            gs.plan_maneuver(c, player, id, m.unwrap() as u8).unwrap();
        }
        gs
    }

    #[test]
    fn howlrunner_lets_a_friend_at_range_1_reroll_one_attack_die() {
        let c = content();
        let north = FRAC_PI_2;
        let south = -FRAC_PI_2;
        // Two TIEs abreast, 1 unit apart (Range 1), vs one X-Wing. Fire
        // order: Howlrunner, X-Wing, Academy Pilot. Every die is blank
        // except the Academy Pilot's single reroll, which is a hit.
        let rolls = vec![7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 0, 7, 7, 7];
        let run = |leader: &str| {
            let mut gs = skirmish(
                &c,
                &[
                    (leader, Pose::new(9.0, 2.5, north), 5),
                    ("academypilot", Pose::new(11.0, 2.5, north), 5),
                ],
                &[("bluesquadronnovice", Pose::new(10.0, 17.5, south), 4)],
            );
            let mut rolls = scripted(rolls.clone());
            gs.commit_plans(&c, P0, &mut rolls).unwrap();
            gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap()
        };
        let rec = run("howlrunner");
        let wingman = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert_eq!(wingman.attack_faces, vec![AttackFace::Hit, AttackFace::Blank]);
        assert_eq!(wingman.hits, 1);
        assert!(rec.events.iter().any(|e| e.contains("rerolls 1 attack")), "{:?}", rec.events);
        // Howlrunner herself gets nothing ("another friendly ship").
        let leader = rec.attacks.iter().find(|a| a.attacker == ShipId(0)).unwrap();
        assert_eq!(leader.hits, 0);

        // A plain wingman instead: same dice, no reroll, no hit.
        let rec = run("obsidiansquadronpilot");
        let wingman = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert_eq!((wingman.hits, wingman.attack_faces.len()), (0, 2));
        assert!(!rec.events.iter().any(|e| e.contains("rerolls")));
    }

    #[test]
    fn jess_pava_rerolls_one_die_per_friend_at_range_1_attacking_and_defending() {
        let c = content();
        let north = FRAC_PI_2;
        let south = -FRAC_PI_2;
        // Jess flies 4 to Range 3 of the TIE; her wingman creeps 1 and
        // ends diagonally within Range 1 of her but beyond Range 3 of the
        // TIE (so it never fires or gets shot). Fire order: Jess (PS3),
        // TIE (PS1).
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 5)],
            &[
                ("jesspava", Pose::new(10.0, 17.5, south), 4),
                ("bluesquadronnovice", Pose::new(12.0, 17.5, south), 1),
            ],
        );
        // Jess: [Blank, Blank, Blank], reroll → Hit. TIE defends 4 blanks.
        // TIE attacks [Hit, Hit]; Jess defends [Blank, Blank, Blank],
        // reroll → Evade: one hit lands on her shields.
        let mut rolls = scripted(vec![7, 7, 7, 0, 7, 7, 7, 7, 0, 0, 7, 7, 7, 0, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks.len(), 2, "{:?}", rec.attacks);
        let hers = &rec.attacks[0];
        assert_eq!((hers.attacker, hers.hits), (ShipId(1), 1));
        let at_her = &rec.attacks[1];
        assert_eq!((at_her.defender, at_her.hits), (ShipId(1), 1));
        assert!(at_her.defense_faces.contains(&DefenseFace::Evade));
        assert_eq!(gs.ships[1].shields, 2);
        assert!(rec.events.iter().any(|e| e.contains("rerolls 1 attack")), "{:?}", rec.events);
        assert!(rec.events.iter().any(|e| e.contains("rerolls 1 defense")), "{:?}", rec.events);
    }

    #[test]
    fn yt1300_turret_primary_fires_at_ships_behind_it_and_pilot_stats_override() {
        let c = content();
        let north = FRAC_PI_2;
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 1)],
            &[("outerrimsmuggler", Pose::new(10.0, 17.5, -north), 1)],
        );
        // The Outer Rim Smuggler's card prints 2/1/6/4 on a 3/1/8/5 hull.
        assert_eq!((gs.ships[1].hull, gs.ships[1].shields), (6, 4));
        assert_eq!(gs.printed(&c, &gs.ships[1]).attack, 2);
        // Stage both heading north with the TIE 6 units behind the
        // freighter (range 3, outside the freighter's forward arc).
        gs.ships[0].pose = Some(Pose::new(10.0, 4.0, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 12.0, north));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shots: Vec<(ShipId, u8, usize)> =
            rec.attacks.iter().map(|a| (a.attacker, a.range, a.attack_faces.len())).collect();
        assert!(shots.contains(&(ShipId(1), 3, 2)), "turret shot missing: {shots:?}");
        assert!(shots.contains(&(ShipId(0), 3, 2)), "TIE shot missing: {shots:?}");
    }

    #[test]
    fn lambda_shuttle_stationary_maneuver_holds_position_and_stresses() {
        let c = content();
        let north = FRAC_PI_2;
        let mut gs = skirmish(
            &c,
            &[("omicrongrouppilot", Pose::new(10.0, 2.5, north), 0)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -north), 4)],
        );
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shuttle = &gs.ships[0];
        let pose = shuttle.pose.unwrap();
        assert!((pose.anchor.x - 10.0).abs() < 1e-9 && (pose.anchor.y - 2.5).abs() < 1e-9);
        assert_eq!(shuttle.stress, 1, "the stop is a red maneuver");
        assert!(rec.attacks.is_empty(), "13.5 units apart: out of range");
    }

    /// Resolve combat by hand, answering every prompt with `pick`.
    fn run_combat(
        c: &Content,
        gs: &mut GameState,
        rolls: &mut dyn FnMut() -> u8,
        mut pick: impl FnMut(&PendingAttack) -> (ShipId, Option<UpgradeId>),
    ) -> TurnRecords {
        loop {
            match gs.combat_step(c, rolls).unwrap() {
                CombatStep::NeedTarget(p) => {
                    let (t, w) = pick(&p);
                    gs.declare_target(c, p.owner, t, w, rolls).unwrap();
                }
                CombatStep::Attack(_) => {}
                CombatStep::Done(rec) => return rec,
            }
        }
    }

    #[test]
    fn proton_torpedoes_need_a_lock_are_offered_in_band_and_are_discarded_after_firing() {
        let c = content();
        let torps = UpgradeId(1); // Proton Torpedoes: 4 dice, R2-3, spend lock, discard
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.ships[1].upgrades.push(torps);
        let mut miss = || 7u8;
        assert_eq!(gs.commit_plans_begin(&c, P0, &mut miss).unwrap(), None);
        gs.commit_plans_begin(&c, P1, &mut miss).unwrap();
        // No lock: the X-Wing (fires first) sees one option, the primary,
        // and fires it automatically.
        let step = gs.combat_step(&c, &mut miss).unwrap();
        let CombatStep::Attack(rec) = step else { panic!("expected an automatic primary shot") };
        assert_eq!((rec.attacker, rec.weapon), (ShipId(1), None));
        assert!(gs.ships[1].upgrades.contains(&torps));

        // With a lock on the TIE the same range-3 shot offers two options;
        // the torpedo choice rolls 4 dice, the TIE defends with 3 (no
        // range-3 bonus against ordnance), the lock is spent up front and
        // the card is gone afterwards.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.ships[1].upgrades.push(torps);
        gs.ships[1].lock = Some(ShipId(0));
        let mut rolls = scripted(vec![0, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let CombatStep::NeedTarget(p) = gs.combat_step(&c, &mut rolls).unwrap() else {
            panic!("expected a weapon choice")
        };
        let mut weapons: Vec<Option<UpgradeId>> = p.options.iter().map(|o| o.weapon).collect();
        weapons.sort();
        assert_eq!(weapons, vec![None, Some(torps)]);
        assert_eq!(gs.auto_target(&p), Some((ShipId(0), None)), "auto never spends ordnance");
        let rec = gs.declare_target(&c, P1, ShipId(0), Some(torps), &mut rolls).unwrap();
        assert_eq!(rec.weapon, Some(torps));
        assert_eq!((rec.attack_faces.len(), rec.defense_faces.len()), (4, 3));
        assert!(rec.lock_spent);
        assert_eq!(rec.hits, 4);
        assert_eq!(gs.ships[1].lock, None);
        assert!(!gs.ships[1].upgrades.contains(&torps));
        let ev = gs.combat_events();
        assert!(ev.iter().any(|e| e.contains("fires Proton Torpedoes")), "{ev:?}");
        assert!(ev.iter().any(|e| e.contains("Proton Torpedoes discarded (fired)")), "{ev:?}");

        // At Range 1 the torpedo (R2-3) is not offered even with a lock.
        let mut gs =
            duel_at(&c, "academypilot", "bluesquadronnovice", Pose::new(10.0, 13.5, -FRAC_PI_2));
        gs.ships[1].upgrades.push(torps);
        gs.ships[1].lock = Some(ShipId(0));
        gs.commit_plans_begin(&c, P0, &mut miss).unwrap();
        gs.commit_plans_begin(&c, P1, &mut miss).unwrap();
        let CombatStep::Attack(rec) = gs.combat_step(&c, &mut miss).unwrap() else {
            panic!("one option only")
        };
        assert_eq!((rec.range, rec.weapon), (1, None));
    }

    #[test]
    fn turret_card_fires_outside_the_arc_without_being_discarded() {
        let c = content();
        let ion_turret = UpgradeId(10); // Ion Cannon Turret: 3 dice, R1-2, free
        let north = FRAC_PI_2;
        // Y-Wing ahead of a TIE, both heading north: the TIE is behind it
        // (out of the primary arc) 4 units back → Range 2.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 1)],
            &[("goldsquadronpilot", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[1].upgrades.push(ion_turret);
        gs.ships[0].pose = Some(Pose::new(10.0, 4.0, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 9.0, north));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, |p| panic!("no prompt expected: {p:?}"));
        let ywing_shot = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).expect("turret shot");
        assert_eq!((ywing_shot.weapon, ywing_shot.range), (Some(ion_turret), 2));
        assert_eq!(ywing_shot.attack_faces.len(), 3);
        assert!(gs.ships[1].upgrades.contains(&ion_turret), "turrets are not discarded");
    }

    /// Fire `weapon` from ship 1 (the Rebel) at ship 0 whatever the
    /// prompt offers; primary otherwise.
    fn prefer(weapon: UpgradeId) -> impl FnMut(&PendingAttack) -> (ShipId, Option<UpgradeId>) {
        move |p| {
            p.options
                .iter()
                .find(|o| o.weapon == Some(weapon))
                .map(|o| (o.target, o.weapon))
                .unwrap_or_else(|| {
                    let o = p.options.iter().find(|o| o.weapon.is_none()).unwrap();
                    (o.target, None)
                })
        }
    }

    #[test]
    fn proton_torpedoes_turn_a_focus_result_into_a_crit() {
        let c = content();
        let torps = UpgradeId(1);
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.ships[1].upgrades.push(torps);
        gs.ships[1].lock = Some(ShipId(0));
        // 4 dice [Eye, Eye, Blank, Blank], no focus token: one eye becomes
        // a crit, the other stays an eye. TIE defends 3 blanks.
        let mut rolls = scripted(vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(torps));
        let shot = &rec.attacks[0];
        assert_eq!((shot.weapon, shot.hits, shot.crits), (Some(torps), 0, 1));
        assert!(
            rec.events.iter().any(|e| e.contains("focus result to critical hit")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn advanced_proton_torpedoes_turn_blanks_into_eyes_for_the_focus_token() {
        let c = content();
        let adv = UpgradeId(2); // 5 dice, Range 1 only
        let mut gs =
            duel_at(&c, "academypilot", "bluesquadronnovice", Pose::new(10.0, 13.5, -FRAC_PI_2));
        gs.ships[1].upgrades.push(adv);
        gs.ships[1].lock = Some(ShipId(0));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // 5 blanks: three become eyes, the focus token turns them to hits.
        let mut rolls = scripted(vec![7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(adv));
        let shot = &rec.attacks[0];
        assert_eq!((shot.weapon, shot.range, shot.attack_faces.len()), (Some(adv), 1, 5));
        assert_eq!((shot.hits, shot.attacker_focus_spent), (3, true));
    }

    #[test]
    fn heavy_laser_cannon_downgrades_crits_and_autoblaster_hits_cannot_be_canceled() {
        let c = content();
        let hlc = UpgradeId(190);
        let north = FRAC_PI_2;
        // Lambda (PS2 Omicron) vs X-Wing novice (PS2): the shuttle owner
        // holds initiative (seat 0) and fires first.
        let mut gs = skirmish(
            &c,
            &[("omicrongrouppilot", Pose::new(10.0, 2.5, north), 1)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[0].upgrades.push(hlc);
        gs.ships[0].pose = Some(Pose::new(10.0, 4.0, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 12.0, -north)); // ends nose at 11: gap 6 → R3
        // HLC 4 dice [Crit, Crit, Crit, Hit] → all hits; X-Wing 2 defense
        // dice (no R3 bonus vs a cannon) [Evade, Blank] → 3 hits land.
        let mut rolls = scripted(vec![3, 3, 3, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, |p| {
            let o = p.options.iter().find(|o| o.weapon == Some(hlc)).unwrap();
            (o.target, o.weapon)
        });
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(0)).unwrap();
        assert_eq!((shot.weapon, shot.defense_faces.len()), (Some(hlc), 2));
        assert_eq!((shot.hits, shot.crits), (3, 0));

        // Autoblaster at Range 1: [Hit, Hit, Crit] vs [Evade, Evade]: the
        // evades may only cancel the crit.
        let auto = UpgradeId(192);
        let mut gs = skirmish(
            &c,
            &[("omicrongrouppilot", Pose::new(10.0, 2.5, north), 1)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[0].upgrades.push(auto);
        gs.ships[0].pose = Some(Pose::new(10.0, 4.0, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 8.0, -north)); // nose 7 vs nose 5: gap 2 → R1
        let mut rolls = scripted(vec![0, 0, 3, 0, 0, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, |p| {
            let o = p.options.iter().find(|o| o.weapon == Some(auto)).unwrap();
            (o.target, o.weapon)
        });
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(0)).unwrap();
        assert_eq!((shot.range, shot.hits, shot.crits), (1, 2, 0));
    }

    #[test]
    fn homing_missiles_keep_the_lock_and_deny_evade_tokens() {
        let c = content();
        let homing = UpgradeId(142);
        let north = FRAC_PI_2;
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 5)],
            &[("greensquadronpilot", Pose::new(10.0, 17.5, -north), 4)],
        );
        gs.ships[1].upgrades.push(homing);
        gs.ships[1].lock = Some(ShipId(0));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Evade).unwrap();
        // A-Wing (PS3) fires first: 4 hits; the TIE's 3 defense dice are
        // blank and its evade token may not be spent: destroyed.
        let mut rolls = scripted(vec![0, 0, 0, 0, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(homing));
        let shot = &rec.attacks[0];
        assert_eq!((shot.weapon, shot.evade_spent, shot.hits), (Some(homing), false, 4));
        assert!(shot.defender_destroyed);
        assert!(!shot.lock_spent, "Homing Missiles do not spend the lock");
        assert!(!gs.ships[1].upgrades.contains(&homing), "but the card is discarded");
    }

    #[test]
    fn dorsal_turret_and_proton_rockets_add_dice() {
        let c = content();
        let north = FRAC_PI_2;
        // Dorsal Turret (2 dice) at Range 1 behind the Y-Wing: 3 dice.
        let dorsal = UpgradeId(13);
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 1)],
            &[("goldsquadronpilot", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[1].upgrades.push(dorsal);
        gs.ships[0].pose = Some(Pose::new(10.0, 4.0, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 7.0, north));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(dorsal));
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert_eq!((shot.weapon, shot.range, shot.attack_faces.len()), (Some(dorsal), 1, 3));

        // Proton Rockets on an A-Wing (agility 3): 2 + 3 = 5 dice at
        // Range 1, focus token required but kept.
        let rockets = UpgradeId(145);
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 1)],
            &[("prototypepilot", Pose::new(10.0, 17.5, -north), 2)],
        );
        gs.ships[1].upgrades.push(rockets);
        gs.ships[0].pose = Some(Pose::new(10.0, 4.0, north)); // nose 5 after 1
        gs.ships[1].pose = Some(Pose::new(10.0, 9.0, -north)); // nose 7 after 2: gap 2 → R1
        gs.ships[1].focus = 1;
        let mut rolls = scripted(vec![7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(rockets));
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert_eq!((shot.weapon, shot.range, shot.attack_faces.len()), (Some(rockets), 1, 5));
        assert!(!shot.attacker_focus_spent);
    }

    /// Attack dice thrown by the Imperial ship (ShipId 0) in a duel.
    fn imperial_shot(rec: &TurnRecords) -> &AttackRecord {
        rec.attacks.iter().find(|a| a.attacker == ShipId(0)).expect("the TIE fired")
    }

    #[test]
    fn mauler_mithel_rolls_an_extra_die_only_at_range_1() {
        let c = content();
        // Nose to nose at range 3: the normal 2 dice.
        let mut gs = duel(&c, "maulermithel", "bluesquadronnovice");
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(imperial_shot(&rec).attack_faces.len(), 2);
        assert!(!rec.events.iter().any(|e| e.contains("ability")));

        // X-Wing starts 4 units closer: the bases end 2 apart (range 1),
        // 2 base + 1 range + 1 ability = 4 dice.
        let mut gs =
            duel_at(&c, "maulermithel", "bluesquadronnovice", Pose::new(10.0, 13.5, -FRAC_PI_2));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = imperial_shot(&rec);
        assert_eq!((shot.range, shot.attack_faces.len()), (1, 4));
        assert!(
            rec.events.iter().any(|e| e.contains("+1 attack die (point blank)")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn backstabber_rolls_an_extra_die_from_outside_the_defenders_arc() {
        let c = content();
        // Head-on the X-Wing sees him: 2 dice.
        let mut gs = duel(&c, "backstabber", "bluesquadronnovice");
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(imperial_shot(&rec).attack_faces.len(), 2);

        // Both heading north, the X-Wing ahead: it ends 6 units in front
        // of the TIE (range 3) with the TIE behind it, out of its arc.
        let mut gs =
            duel_at(&c, "backstabber", "bluesquadronnovice", Pose::new(10.0, 10.5, FRAC_PI_2));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks.len(), 1, "the X-Wing has no target: {:?}", rec.attacks);
        let shot = imperial_shot(&rec);
        assert_eq!((shot.range, shot.attack_faces.len()), (3, 3));
        assert!(
            rec.events.iter().any(|e| e.contains("outside the defender's arc")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn scourge_rolls_an_extra_die_against_a_damaged_defender() {
        let c = content();
        let mut gs = duel(&c, "scourge", "bluesquadronnovice");
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(imperial_shot(&rec).attack_faces.len(), 2, "undamaged: no bonus");

        // Shields lost do not count; a hull point lost (a Damage card) does.
        let mut gs = duel(&c, "scourge", "bluesquadronnovice");
        gs.ships[1].shields = 0;
        gs.ships[1].hull -= 1;
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(imperial_shot(&rec).attack_faces.len(), 3);
        assert!(
            rec.events.iter().any(|e| e.contains("defender already damaged")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn zeta_leader_takes_a_stress_for_an_extra_die_when_unstressed() {
        let c = content();
        let mut gs = duel(&c, "zetaleader", "bluesquadronnovice");
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(imperial_shot(&rec).attack_faces.len(), 3);
        assert_eq!(gs.ships[0].stress, 1);
        assert!(rec.events.iter().any(|e| e.contains("takes stress")), "{:?}", rec.events);

        // Already stressed (straight-5 is white on the TIE/fo dial, so
        // the token survives the move): no extra die, no second token.
        let mut gs = duel(&c, "zetaleader", "bluesquadronnovice");
        gs.ships[0].stress = 1;
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(imperial_shot(&rec).attack_faces.len(), 2);
        assert_eq!(gs.ships[0].stress, 1);
    }

    #[test]
    fn winged_gundark_turns_a_hit_into_a_crit_at_range_1() {
        let c = content();
        // Range 3, [Hit, Hit]: unchanged.
        let mut gs = duel(&c, "wingedgundark", "bluesquadronnovice");
        let mut rolls = scripted(vec![0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = imperial_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (2, 0));

        // Range 1, 3 dice [Hit, Hit, Blank]: one hit becomes a crit.
        let mut gs =
            duel_at(&c, "wingedgundark", "bluesquadronnovice", Pose::new(10.0, 13.5, -FRAC_PI_2));
        let mut rolls = scripted(vec![0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = imperial_shot(&rec);
        assert_eq!((shot.range, shot.hits, shot.crits), (1, 1, 1));
        assert!(
            rec.events.iter().any(|e| e.contains("hit result to critical")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn omega_ace_spends_lock_and_focus_for_all_crits() {
        let c = content();
        // With only a focus token the ability stays silent: [Blank, Eye]
        // → the token converts the eye, 1 hit.
        let mut gs = duel(&c, "omegaace", "bluesquadronnovice");
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        let mut rolls = scripted(vec![7, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = imperial_shot(&rec);
        assert_eq!((shot.hits, shot.crits, shot.lock_spent), (1, 0, false));

        // Lock on the X-Wing plus a focus token (Push the Limit is not
        // modelled: give the token by hand): [Blank, Blank] → 2 crits,
        // both tokens gone, no reroll consumed from the dice stream.
        let mut gs = duel(&c, "omegaace", "bluesquadronnovice");
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::TargetLock(ShipId(1))).unwrap();
        let mut rolls = scripted(vec![7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        gs.ships[0].focus = 1;
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = imperial_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (0, 2));
        assert!(shot.lock_spent && shot.attacker_focus_spent);
        assert_eq!((gs.ships[0].lock, gs.ships[0].focus), (None, 0));
        assert!(rec.events.iter().any(|e| e.contains("all dice critical")), "{:?}", rec.events);
    }

    #[test]
    fn dark_curse_denies_attackers_focus_spending_and_rerolls() {
        let c = content();
        // Dark Curse (PS6) fires first, all blanks. The X-Wing holds a
        // focus token and rolls [Eye, Eye, Hit]: the eyes stay eyes.
        let mut gs = duel(&c, "darkcurse", "bluesquadronnovice");
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let mut rolls = scripted(vec![7, 7, 7, 7, 7, 4, 4, 0, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = &rec.attacks[1];
        assert_eq!(shot.attacker, ShipId(1));
        assert_eq!((shot.hits, shot.attacker_focus_spent), (1, false));
        assert!(rec.events.iter().any(|e| e.contains("cannot spend focus")), "{:?}", rec.events);

        // A target lock on him cannot be spent to reroll blanks either
        // (the lock is handed out by hand: the X-Wing moves first and is
        // out of lock range when its action would resolve).
        let mut gs = duel(&c, "darkcurse", "bluesquadronnovice");
        gs.ships[1].lock = Some(ShipId(0));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = &rec.attacks[1];
        assert_eq!((shot.hits, shot.lock_spent), (0, false));
        assert_eq!(gs.ships[1].lock, Some(ShipId(0)));
    }

    #[test]
    fn omega_leader_freezes_dice_of_the_ship_he_has_locked() {
        let c = content();
        let mut gs = duel(&c, "omegaleader", "bluesquadronnovice");
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::TargetLock(ShipId(1))).unwrap();
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // Omega Leader (PS8) rolls [Hit, Hit] (no blanks, so his lock is
        // kept). The X-Wing defends [Eye, Eye, Blank] with a focus token
        // it may not spend: both hits land. Its own attack [Eye, Eye,
        // Eye] cannot be modified either: nothing lands.
        let mut rolls = scripted(vec![0, 0, 4, 4, 7, 4, 4, 4, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let (his, theirs) = (&rec.attacks[0], &rec.attacks[1]);
        assert_eq!(his.attacker, ShipId(0));
        assert_eq!((his.hits, his.defender_focus_spent, his.lock_spent), (2, false, false));
        assert_eq!(gs.ships[1].shields, 1);
        assert_eq!((theirs.hits, theirs.attacker_focus_spent), (0, false));
        assert_eq!(
            rec.events.iter().filter(|e| e.contains("cannot modify dice")).count(),
            2,
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn poe_turns_one_focus_result_without_spending_the_token() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "poedameron");
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // Poe (PS8) fires first: [Focus, Blank, Blank] → his ability turns
        // the eye into a hit for free; the TIE's 4 defense dice are
        // blanks. Then the TIE rolls [Hit, Hit]; Poe defends with 3 dice
        // [Focus, Blank, Blank]: the eye becomes an evade for free, one
        // hit lands on shields. His focus token is never spent.
        let mut rolls = scripted(vec![4, 7, 7, 7, 7, 7, 7, 0, 0, 3, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let poe_shot = &rec.attacks[0];
        assert_eq!(poe_shot.attacker, ShipId(1));
        assert_eq!((poe_shot.hits, poe_shot.attacker_focus_spent), (1, false));
        assert_eq!(gs.ships[0].hull, 2);
        let tie_shot = &rec.attacks[1];
        assert_eq!((tie_shot.hits, tie_shot.defender_focus_spent), (1, false));
        assert_eq!(gs.ships[1].shields, 2);
        assert_eq!(
            rec.events.iter().filter(|e| e.contains("ability")).count(),
            2,
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn poe_needs_a_focus_token_and_spends_it_only_for_extra_eyes() {
        let c = content();
        // Without a token the ability is silent: two eyes stay eyes (no
        // focus to spend either) → 0 hits.
        let mut gs = duel(&c, "academypilot", "poedameron");
        let mut rolls = scripted(vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks[0].hits, 0);
        assert!(!rec.events.iter().any(|e| e.contains("ability")));

        // With a token and two eyes: one converts free, the token is
        // spent on the other → 2 hits, focus 0.
        let mut gs = duel(&c, "academypilot", "poedameron");
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let mut rolls = scripted(vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!((rec.attacks[0].hits, rec.attacks[0].attacker_focus_spent), (2, true));
    }

    #[test]
    fn combat_fires_highest_skill_first_and_strips_shields_before_hull() {
        let c = content();
        let mut gs = new_1v1(&c);
        // X-Wing (skill 2) fires first: 3 dice at R3, all blanks (6).
        // TIE defense: 3 agility + 1 (R3) = 4 dice (blanks). Then TIE
        // fires 2 dice: Hit (0) + Crit (3); X-Wing defense 2+1=3 blanks.
        let mut rolls = scripted(vec![6, 6, 6, 7, 7, 7, 7, 0, 3, 7, 7, 7]);
        fly_to_range3(&c, &mut gs, &mut rolls);
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks.len(), 2);
        assert_eq!(rec.attacks[0].attacker, ShipId(1), "higher skill fires first");
        assert_eq!(rec.attacks[0].hits + rec.attacks[0].crits, 0);
        let tie_shot = &rec.attacks[1];
        assert_eq!(tie_shot.range, 3);
        assert_eq!((tie_shot.hits, tie_shot.crits), (1, 1));
        // Both absorbed by shields: no hull damage, no critical effect.
        assert_eq!(tie_shot.shields_lost, 2);
        assert_eq!(tie_shot.crits_to_hull, 0);
        assert_eq!(gs.ships[1].shields, 1);
        assert_eq!(gs.ships[1].hull, 3);
    }

    #[test]
    fn combat_tokens_spend_focus_and_evade() {
        let c = content();
        let mut gs = new_1v1(&c);
        // Laterally offset: each ship is in the other's 90° arc but OUT of
        // the narrow bullseye lane, so defense tokens stay spendable.
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(12.0, 17.5, -FRAC_PI_2)).unwrap();
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Evade).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4).unwrap();
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // X-Wing attack: eye, eye, blank → focus turns 2 eyes into hits.
        // TIE defense: 4 blanks, then spends its evade token → 1 evade,
        // so 1 hit lands on the shieldless TIE's hull.
        // TIE attack: 2 blanks; X-Wing defense: 3 blanks.
        let mut rolls = scripted(vec![4, 4, 6, 7, 7, 7, 7, 6, 6, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let xw_shot = &rec.attacks[0];
        assert!(!xw_shot.defender_in_bullseye);
        assert!(xw_shot.attacker_focus_spent);
        assert!(xw_shot.evade_spent);
        assert_eq!((xw_shot.hits, xw_shot.crits), (1, 0));
        assert_eq!(xw_shot.hull_lost, 1);
        assert_eq!(gs.ships[0].hull, 2);
    }

    #[test]
    fn stressed_red_reveal_substitutes_slowest_white_straight() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        let kturn3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 3);
        // Plan the red K-turn while unstressed (legal), then simulate an
        // external stress source (future crit/ability) before it resolves.
        gs.plan_maneuver(&c, P0, ShipId(0), kturn3).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        gs.ships[0].stress = 1;
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap();
        let mv = &rec.moves[0];
        // TIE's slowest white straight is speed 4: flown instead, no flip.
        assert_eq!(mv.maneuver.steer, crate::maneuver::Steer::Straight);
        assert_eq!(mv.maneuver.distance, 4);
        assert!((mv.end.anchor.y - 6.0).abs() < 1e-9);
        assert!((mv.end.heading - FRAC_PI_2).abs() < 1e-9, "no 180 flip");
        // White maneuver: the stress neither grows nor sheds…
        assert_eq!(mv.stress, 1);
        // …and the still-stressed ship forfeits its action.
        assert_eq!(mv.action_result, ActionResult::SkippedStressed);
    }

    #[test]
    fn crit_direct_hit_deals_an_extra_hull_point() {
        let c = content();
        let mut gs = new_1v1(&c);
        // X-Wing lands 1 crit on the shieldless TIE (hull 3→2), the draw
        // (raw 5) is Direct Hit! → 1 more hull. TIE's return shot misses.
        let mut rolls = scripted(vec![3, 6, 6, 7, 7, 7, 7, 5, 6, 6, 7, 7, 7]);
        fly_to_range3(&c, &mut gs, &mut rolls);
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let shot = &rec.attacks[0];
        assert_eq!(shot.crits_to_hull, 1);
        assert_eq!(shot.hull_lost, 2, "crit + Direct Hit extra");
        assert_eq!(gs.ships[0].hull, 1);
        assert!(rec.events.iter().any(|e| e.contains("Direct Hit")));
        assert!(gs.ships[0].crits.is_empty(), "Direct Hit is immediate, not persistent");
    }

    #[test]
    fn crit_damaged_engine_makes_turns_red() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.ships[0].crits.push(crate::crit::CritEffect::DamagedEngine);
        let turn2 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::TurnLeft && m.distance == 2);
        // Stressed: the now-effectively-red turn cannot be planned.
        gs.ships[0].stress = 1;
        assert_eq!(
            gs.plan_maneuver(&c, P0, ShipId(0), turn2),
            Err(Rejection::StressedRedForbidden)
        );
        // Unstressed it flies — and stresses the pilot like any red.
        gs.ships[0].stress = 0;
        gs.plan_maneuver(&c, P0, ShipId(0), turn2).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap();
        assert_eq!(rec.moves[0].stress, 1, "white turn flown as red gains stress");
    }

    #[test]
    fn crit_weapons_failure_blocks_attack_and_ticks_down() {
        let c = content();
        let mut gs = new_1v1(&c);
        gs.ships[1].crits.push(crate::crit::CritEffect::WeaponsFailure { rounds: 2 });
        let mut rolls = scripted(vec![7]);
        fly_to_range3(&c, &mut gs, &mut rolls);
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        // Only the TIE fired; the X-Wing's weapons are down.
        assert_eq!(rec.attacks.len(), 1);
        assert_eq!(rec.attacks[0].attacker, ShipId(0));
        // End phase ticked the effect down but it survives one more round.
        assert!(gs.ships[1].crits.contains(&crate::crit::CritEffect::WeaponsFailure { rounds: 1 }));
    }

    #[test]
    fn crit_structural_damage_cuts_defense_dice() {
        let c = content();
        let mut gs = new_1v1(&c);
        gs.ships[0].crits.push(crate::crit::CritEffect::StructuralDamage);
        let mut rolls = scripted(vec![6, 7]);
        fly_to_range3(&c, &mut gs, &mut rolls);
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        // TIE agility 3 − 1 structural + 1 range-3 bonus = 3 defense dice.
        assert_eq!(rec.attacks[0].defender, ShipId(0));
        assert_eq!(rec.attacks[0].defense_faces.len(), 3);
    }

    #[test]
    fn crit_sensor_array_forfeits_actions() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.ships[0].crits.push(crate::crit::CritEffect::DamagedSensorArray);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap();
        assert_eq!(rec.moves[0].action_result, ActionResult::SkippedDamaged);
        assert_eq!(gs.ships[0].focus, 0);
    }

    #[test]
    fn crit_blinded_pilot_fires_zero_dice_once() {
        let c = content();
        let mut gs = new_1v1(&c);
        gs.ships[1].crits.push(crate::crit::CritEffect::BlindedPilot);
        let mut rolls = scripted(vec![7]);
        fly_to_range3(&c, &mut gs, &mut rolls);
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let xw_shot = &rec.attacks[0];
        assert_eq!(xw_shot.attacker, ShipId(1));
        assert!(xw_shot.attack_faces.is_empty(), "blinded: zero attack dice");
        assert!(gs.ships[1].crits.is_empty(), "vision clears after the wild shot");
    }

    #[test]
    fn crit_stunned_pilot_takes_damage_on_bump() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.ships[1].crits.push(crate::crit::CritEffect::StunnedPilot);
        let s5_tie =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4_xw = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        // Same two-turn approach as the bump test: turn 2 the X-Wing
        // rams the TIE, and the stunned pilot takes 1 (to shields).
        for _ in 0..2 {
            gs.plan_maneuver(&c, P0, ShipId(0), s5_tie).unwrap();
            gs.plan_maneuver(&c, P1, ShipId(1), s4_xw).unwrap();
            gs.commit_plans(&c, P0, &mut || 7).unwrap();
            gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap();
        }
        assert_eq!(gs.ships[1].shields, 2, "bump damage absorbed by shields");
    }

    #[test]
    fn crit_console_fire_burns_at_combat_start() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.ships[0].crits.push(crate::crit::CritEffect::ConsoleFire);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        // Ships are far apart (no attacks), so the only roll consumed is
        // the Console Fire die: 0 = Hit → 1 hull on the shieldless TIE.
        gs.commit_plans(&c, P0, &mut || 0).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut || 0).unwrap().unwrap();
        assert!(rec.events.iter().any(|e| e.contains("Console Fire")));
        assert_eq!(gs.ships[0].hull, 2);
    }

    #[test]
    fn crit_damaged_cockpit_zeroes_pilot_skill() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        // X-Wing (skill 2) with a damaged cockpit drops to skill 0: it now
        // moves BEFORE the TIE (skill 1) instead of after.
        gs.ships[1].crits.push(crate::crit::CritEffect::DamagedCockpit);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut || 7).unwrap().unwrap();
        assert_eq!(rec.moves[0].ship, ShipId(1), "skill 0 moves first");
        assert_eq!(rec.moves[1].ship, ShipId(0));
    }

    #[test]
    fn bullseye_denies_defender_tokens() {
        let c = content();
        let mut gs = new_1v1(&c);
        // Dead-center alignment: the TIE ends squarely in the X-Wing's
        // bullseye lane, so its evade token cannot be spent.
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Evade).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4).unwrap();
        // X-Wing attack: 2 hits + blank; TIE defense: 4 blanks. Without
        // its evade token the shieldless TIE takes both hits.
        // TIE attack: 2 blanks; X-Wing defense: 3 blanks.
        let mut rolls = scripted(vec![0, 0, 6, 7, 7, 7, 7, 6, 6, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let xw_shot = &rec.attacks[0];
        assert!(xw_shot.defender_in_bullseye);
        assert!(!xw_shot.evade_spent, "bullseye denies the evade token");
        assert_eq!((xw_shot.hits, xw_shot.crits), (2, 0));
        assert_eq!(xw_shot.hull_lost, 2);
        assert_eq!(gs.ships[0].hull, 1);
    }

    #[test]
    fn equal_skill_fires_simultaneously_and_initiative_wins_mutual_kill() {
        let c = content();
        // TIE mirror match: equal squads, tie roll Hit → P0 has initiative.
        let mut gs = GameState::new(
            board(),
            &c,
            [&fleet(&c, &[TIE]), &fleet(&c, &[TIE])],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        assert_eq!(gs.initiative, P0);
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s1 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 1);
        // Every attack: 2 hits; every defense: 3 blanks.
        let mut rolls = scripted(vec![0, 0, 7, 7, 7]);
        // Turn 1: close to range 2 (hull gap 5); both take 2 hull damage.
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s5).unwrap();
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks.len(), 2);
        assert_eq!(gs.ships[0].hull, 1);
        assert_eq!(gs.ships[1].hull, 1);
        // Turn 2: both die — but both still fire (simultaneous rule),
        // and the initiative holder wins the mutual kill.
        gs.plan_maneuver(&c, P0, ShipId(0), s1).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s1).unwrap();
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks.len(), 2, "destroyed ship of equal skill still fires");
        assert!(rec.attacks.iter().all(|a| a.defender_destroyed));
        assert_eq!(gs.phase, Phase::GameOver);
        assert_eq!(gs.winner, Some(P0), "initiative wins the mutual kill");
    }

    #[test]
    fn declare_target_prompt_when_several_enemies_in_arc() {
        let c = content();
        let mut gs = GameState::new(
            board(),
            &c,
            [&fleet(&c, &[TIE]), &fleet(&c, &[XWING, XWING])],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        // TIE south; two X-Wings north, both ending inside its arc at R3
        // (#1 a hair nearer than #2).
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(9.0, 17.5, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(11.5, 17.5, -FRAC_PI_2)).unwrap();
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        let s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(2), s4).unwrap();
        let mut miss = || 7u8;
        assert_eq!(gs.commit_plans_begin(&c, P0, &mut miss).unwrap(), None);
        let act = gs.commit_plans_begin(&c, P1, &mut miss).unwrap().unwrap();
        assert_eq!(act.moves.len(), 3);
        assert_eq!(gs.phase, Phase::Combat);
        // X-Wings (skill 2) fire first; each sees only the TIE → automatic.
        assert!(matches!(gs.combat_step(&c, &mut miss).unwrap(), CombatStep::Attack(_)));
        assert!(matches!(gs.combat_step(&c, &mut miss).unwrap(), CombatStep::Attack(_)));
        // The TIE sees both X-Wings: the game must ask its owner.
        let CombatStep::NeedTarget(p) = gs.combat_step(&c, &mut miss).unwrap() else {
            panic!("expected a Declare Target prompt")
        };
        assert_eq!((p.attacker, p.owner), (ShipId(0), P0));
        let mut ids: Vec<u32> = p.options.iter().map(|o| o.target.0).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
        // Stepping again re-issues the prompt; only the owner may answer,
        // and only with an eligible enemy.
        assert!(matches!(gs.combat_step(&c, &mut miss).unwrap(), CombatStep::NeedTarget(_)));
        assert_eq!(
            gs.declare_target(&c, P1, ShipId(2), None, &mut miss),
            Err(Rejection::NotYourShip)
        );
        assert_eq!(
            gs.declare_target(&c, P0, ShipId(0), None, &mut miss),
            Err(Rejection::BadTarget)
        );
        // Pick the farther X-Wing — overriding the auto policy's nearest.
        let rec = gs.declare_target(&c, P0, ShipId(2), None, &mut miss).unwrap();
        assert_eq!(rec.defender, ShipId(2));
        let CombatStep::Done(all) = gs.combat_step(&c, &mut miss).unwrap() else {
            panic!("expected the turn to finish")
        };
        assert_eq!(all.attacks.len(), 3);
        assert_eq!(gs.phase, Phase::Planning);
        assert_eq!(gs.turn, 2);
    }

    #[test]
    fn commit_requires_all_plans_and_resign_ends_game() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        assert_eq!(gs.commit_plans(&c, P0, &mut || 7), Err(Rejection::PlansIncomplete));
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.commit_plans(&c, P0, &mut || 7).unwrap();
        assert_eq!(gs.commit_plans(&c, P0, &mut || 7), Err(Rejection::AlreadyCommitted));
        assert_eq!(gs.resign(P1), P0);
        assert_eq!(gs.phase, Phase::GameOver);
        assert_eq!(gs.winner, Some(P0));
    }
}
