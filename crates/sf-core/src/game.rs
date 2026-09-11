//! The authoritative game state and turn phase machine:
//!
//! `Placement -> [Planning -> Resolution]* -> GameOver`
//!
//! Every mutation goes through a validated command method returning
//! `Result<_, Rejection>`; the server applies these verbatim, the client
//! may use the same methods for optimistic UI checks.

use serde::{Deserialize, Serialize};

use crate::action::{
    self, ActionExtras, ActionKind, ActionResult, PlannedAction, SecondActionKind,
};
use crate::board::{Board, Seat};
use crate::bombs::{self, BombHit, BombKind, BombToken, Detonation};
use crate::combat;
use crate::crit::{self, CritEffect};
use crate::data::Content;
use crate::dice::{AttackFace, DefenseFace};
use crate::geometry::{Footprint, Pose, Vec2};
use crate::maneuver::{self, Difficulty, Maneuver};
use crate::mission::{self, MissionKind, MissionState, MissionView};
use crate::obstacle::{self, Obstacle, ObstacleKind, Pull};
use crate::pilot::{PilotAbility, PilotId};
use crate::rules;
use crate::ship::{Faction, PlayerId, ShipClass, ShipClassId, ShipId, ShipState, StatBlock};
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

/// Who holds initiative at setup: the side with the lowest squad-point
/// total. On a tie the first tied side (the game creator's, when it is
/// among them) rolls one red die — Hit/Crit keeps it, Focus/Blank hands
/// it to the next tied side. ("Choosing" is automated as choosing
/// yourself.) Returns the index into `totals`.
pub fn initiative_seat(totals: &[u32], tie_roll: crate::dice::AttackFace) -> usize {
    use crate::dice::AttackFace;
    let low = totals.iter().copied().min().unwrap_or(0);
    let tied: Vec<usize> = (0..totals.len()).filter(|&i| totals[i] == low).collect();
    match (tied.len(), tie_roll) {
        (0, _) => 0,
        (1, _) => tied[0],
        (_, AttackFace::Hit | AttackFace::Crit) => tied[0],
        (_, AttackFace::Focus | AttackFace::Blank) => tied[1],
    }
}

/// Reroll up to `max` dice showing one of `wants` (earlier entries
/// first); returns how many were rerolled.
fn reroll_matching<F: PartialEq + Copy>(
    faces: &mut [F],
    wants: &[F],
    max: u8,
    fresh: &mut dyn FnMut() -> F,
) -> u8 {
    let mut done = 0;
    for want in wants {
        for f in faces.iter_mut() {
            if done < max && *f == *want {
                *f = fresh();
                done += 1;
            }
        }
    }
    done
}

/// Outcome of one point of normal damage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
pub fn movement_order(ships: &[(ShipId, u8, PlayerId)], ranks: &[u8]) -> Vec<ShipId> {
    let rank = |p: PlayerId| ranks.get(p.0 as usize).copied().unwrap_or(u8::MAX);
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill, owner)| (skill, rank(owner), id.0));
    v.into_iter().map(|(id, _, _)| id).collect()
}

/// Combat phase order: HIGHEST pilot skill fires first. At equal skill
/// the initiative player's ships fire first; then ship id.
pub fn combat_order(ships: &[(ShipId, u8, PlayerId)], ranks: &[u8]) -> Vec<ShipId> {
    let rank = |p: PlayerId| ranks.get(p.0 as usize).copied().unwrap_or(u8::MAX);
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill, owner)| (std::cmp::Reverse(skill), rank(owner), id.0));
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
    /// The card is not equipped on that ship (or is not that kind of card).
    NoSuchUpgrade,
    /// A boost or barrel-roll template only some pilots may use.
    TemplateNotAllowed,
    /// Nothing grants this ship a second action, or not that one.
    SecondActionNotAllowed,
    /// The base would sit on an asteroid or debris token.
    OverlapsObstacle,
    /// Mission 2: the Rebels must deploy beyond Range 1 of every asteroid.
    TooCloseToObstacle,
    /// Mission 2: the disabled ship flies only speed 1-2 until Round 5.
    ShipDisabled,
    /// The action exists only in a mission (Protect, mission 1).
    NotInThisMission,
    /// Han Solo (HotR) is placed after every other ship.
    PlaceLast,
    /// Han Solo (HotR) must be placed beyond Range 3 of every enemy ship.
    TooCloseToEnemy,
    /// No obstacle with that id is on the board.
    NoSuchObstacle,
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
            Rejection::NoSuchUpgrade => "that ship does not carry that card",
            Rejection::TemplateNotAllowed => "that template is not available to this pilot",
            Rejection::SecondActionNotAllowed => "this ship cannot take that second action",
            Rejection::OverlapsObstacle => "the ship would sit on an obstacle",
            Rejection::TooCloseToObstacle => "must deploy beyond Range 1 of every asteroid",
            Rejection::ShipDisabled => "the disabled ship flies only speed 1-2 until Round 5",
            Rejection::NotInThisMission => "that action is not available in this game",
            Rejection::PlaceLast => "this ship is placed after every other ship",
            Rejection::TooCloseToEnemy => "must be placed beyond Range 3 of every enemy ship",
            Rejection::NoSuchObstacle => "there is no such obstacle on the board",
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
    /// Left the board alive under a mission rule (see `destroyed`).
    #[serde(default)]
    pub escaped: bool,
    /// Snap Shot attacks enemies fired at this ship right after its
    /// maneuver, before its action.
    #[serde(default)]
    pub snap_shots: Vec<AttackRecord>,
    /// Flew off the board and is destroyed.
    pub destroyed: bool,
    /// Stress tokens after the maneuver.
    pub stress: u8,
    /// The action that was planned (Pass if none was).
    pub action: PlannedAction,
    pub action_result: ActionResult,
    /// Bomb tokens dropped on dial reveal, before the move.
    #[serde(default)]
    pub dropped_before: Vec<BombToken>,
    /// Mine tokens dropped by the action, after the move.
    #[serde(default)]
    pub dropped_after: Vec<BombToken>,
    /// Mines this ship set off by crossing them (resolved after the move,
    /// before the action).
    #[serde(default)]
    pub mines_hit: Vec<Detonation>,
    /// A free barrel roll taken before the move (BB-8 on a green reveal).
    #[serde(default)]
    pub pre: Option<(PlannedAction, ActionResult)>,
    /// A second action (Push the Limit, Darth Vader, "Snap" Wexley's
    /// boost, Jake Farrell's reposition).
    #[serde(default)]
    pub second: Option<(PlannedAction, ActionResult)>,
    /// Obstacles the base or template overlapped during the move.
    #[serde(default)]
    pub obstacles_hit: Vec<u32>,
    /// A Seismic Torpedo fired as the action: the obstacle it removed and
    /// the blast on every ship at Range 1 of it.
    #[serde(default)]
    pub seismic: Option<SeismicBlast>,
}

/// A Seismic Torpedo blast: the obstacle (removed afterwards) and the
/// detonation resolved on the ships around it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeismicBlast {
    pub obstacle: u32,
    pub detonation: Detonation,
}

/// What an attack did, for the after-attack card effects.
#[derive(Debug, Clone, Copy)]
struct AttackOutcome {
    landed: bool,
    lock_spent: bool,
}

/// One resolved attack in the Combat phase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttackRecord {
    pub attacker: ShipId,
    pub defender: ShipId,
    pub range: u8,
    /// The secondary weapon card fired, or None for the primary weapon.
    pub weapon: Option<UpgradeId>,
    /// The line of sight crossed an obstacle (+1 defense die).
    #[serde(default)]
    pub obstructed: bool,
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
    /// Turr Phennir: the free boost or barrel roll taken after the attack.
    #[serde(default)]
    pub reposition: Option<Reposition>,
}

/// A free reposition taken during the Combat phase, for the animation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Reposition {
    pub action: PlannedAction,
    pub result: ActionResult,
    /// The ship's pose afterwards (unchanged if the action failed).
    pub to: Pose,
}

/// Everything that happened when a turn resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecords {
    pub moves: Vec<MoveRecord>,
    /// Ships dragged by black holes after all moves.
    #[serde(default)]
    pub pulls: Vec<Pull>,
    /// Bombs that went off at the end of the Activation phase.
    #[serde(default)]
    pub detonations: Vec<Detonation>,
    pub attacks: Vec<AttackRecord>,
    /// Narrated side effects: crit draws, Console Fire burns, Stunned
    /// Pilot bumps, destructions from effects.
    pub events: Vec<String>,
}

/// What the Activation phase produced (returned by `commit_plans_begin`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivationRecords {
    pub moves: Vec<MoveRecord>,
    /// Ships dragged by black holes after all moves.
    #[serde(default)]
    pub pulls: Vec<Pull>,
    /// Bombs that went off at the end of the Activation phase.
    #[serde(default)]
    pub detonations: Vec<Detonation>,
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
    /// The range line crosses an obstacle (+1 defense die).
    #[serde(default)]
    pub obstructed: bool,
}

/// A declared shot: defender index, range band and weapon. `second` marks
/// the repeat of an "attack twice" weapon (no token cost, card discarded
/// only after it).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct Shot {
    d_idx: usize,
    range: u8,
    weapon: Option<UpgradeId>,
    second: bool,
    /// Luke Skywalker (crew): one focus result becomes a hit for free.
    focus_hit: bool,
    /// Snap Shot: the attacker may not modify the dice.
    no_mods: bool,
}

/// An attack whose owner must Declare Target (core rules p.10): more than
/// one (weapon, enemy) combination is eligible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAttack {
    pub attacker: ShipId,
    pub owner: PlayerId,
    pub options: Vec<AttackOption>,
    /// Equipped weapons that cannot fire, as (name, reason) — shown
    /// greyed out in the prompt.
    #[serde(default)]
    pub unavailable: Vec<(String, String)>,
}

/// Step-by-step Combat phase bookkeeping (lives in `GameState.combat`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CombatState {
    /// Remaining pilot-skill groups, highest first.
    groups: Vec<Vec<ShipId>>,
    /// Attackers still to act in the current group (alive at its start).
    current: Vec<ShipId>,
    pub pending: Option<PendingAttack>,
    /// Second shot of an "attack twice" weapon, fired on the next step.
    #[serde(default)]
    followup: Option<(usize, Shot)>,
    attacks: Vec<AttackRecord>,
    events: Vec<String>,
    moves: Vec<MoveRecord>,
    #[serde(default)]
    pulls: Vec<Pull>,
    #[serde(default)]
    detonations: Vec<Detonation>,
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
    /// The owner's side: ships on the same team are friendly.
    #[serde(default)]
    pub team: u8,
    pub class: ShipClassId,
    pub callsign: String,
    /// Pilot card name and printed skill (crits may lower the effective
    /// skill; see `pilot_skill`).
    pub pilot: String,
    pub skill: u8,
    /// Equipped upgrade card names.
    pub upgrades: Vec<String>,
    /// The same cards by id (for client-side weapon status).
    #[serde(default)]
    pub upgrade_ids: Vec<crate::upgrade::UpgradeId>,
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
    /// Ion tokens: the next maneuver is a forced white straight 1.
    #[serde(default)]
    pub ion: u8,
    pub lock: Option<ShipId>,
    /// Weapons Engineer: the second target lock.
    #[serde(default)]
    pub lock2: Option<ShipId>,
    /// Active critical effects — public, like faceup cards.
    pub crits: Vec<CritEffect>,
    pub destroyed: bool,
    /// Left the board alive under a mission rule.
    #[serde(default)]
    pub escaped: bool,
    /// Satellite tokens carried (mission 3).
    #[serde(default)]
    pub satellites: u8,
    /// Cards switched on for this round (own ships only).
    #[serde(default)]
    pub card_uses: Vec<UpgradeId>,
    /// Ended its move on an asteroid: no attack this round.
    #[serde(default)]
    pub on_asteroid: bool,
    pub plan: Option<u8>,
    /// Own ships only; None on opponent ships.
    pub planned_action: Option<PlannedAction>,
    /// Own ships only: the bomb card chosen to drop on dial reveal.
    #[serde(default)]
    pub bomb: Option<UpgradeId>,
    /// Own ships only: the planned second action.
    #[serde(default)]
    pub planned_action2: Option<PlannedAction>,
    /// Own ships only: what beyond the action bar may be planned.
    #[serde(default)]
    pub extras: ActionExtras,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub board: Board,
    pub phase: Phase,
    pub turn: u32,
    pub ships: Vec<ShipState>,
    /// One entry per seat.
    pub committed: Vec<bool>,
    /// The winning side (team index) once the game is over.
    pub winner: Option<u8>,
    /// Holder of the initiative token (see `initiative_seat`): the first
    /// seat of the side that has it.
    pub initiative: PlayerId,
    /// Squad-point totals per seat, for display.
    pub squad_totals: Vec<u32>,
    /// Side (team) of each seat; seats on one side are friendly and win
    /// or lose together. A duel is `[0, 1]`.
    #[serde(default)]
    pub teams: Vec<u8>,
    /// Present while the Combat phase is being stepped through.
    pub combat: Option<CombatState>,
    /// Bomb and mine tokens currently on the board.
    #[serde(default)]
    pub bombs: Vec<BombToken>,
    #[serde(default)]
    next_bomb_id: u32,
    /// Asteroid and debris tokens (placed before setup, never move).
    #[serde(default)]
    pub obstacles: Vec<Obstacle>,
    /// Leia Organa (crew) was discarded this round: that seat's red
    /// maneuvers are flown as white (one entry per seat).
    #[serde(default)]
    pub white_reds: Vec<bool>,
    /// Rulebook mission in play (setup, special rules, objectives).
    #[serde(default)]
    pub mission: Option<MissionState>,
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
        Self::from_squads(board, content, &[&a, &b], &[0, 1], tie_roll)
    }

    /// Build a game from two (already validated) squads. `tie_roll` is
    /// one red die drawn by the server, used only when the squad totals
    /// are equal.
    pub fn from_squads(
        board: Board,
        content: &Content,
        squads: &[&Squad],
        teams: &[u8],
        tie_roll: crate::dice::AttackFace,
    ) -> Result<Self, String> {
        let n = squads.len();
        if !(2..=4).contains(&n) {
            return Err(format!("{n} squads: a game needs 2-4"));
        }
        let teams: Vec<u8> = if teams.len() == n { teams.to_vec() } else { (0..n as u8).collect() };
        let pilot_of =
            |id: PilotId| content.pilots.pilot(id).ok_or_else(|| format!("unknown pilot {id:?}"));
        let factions: Vec<_> = squads.iter().map(|s| s.faction).collect();
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
                let mut ship = ShipState::new(
                    ShipId(ships.len() as u32),
                    PlayerId(seat as u32),
                    class_id,
                    pilot_id,
                    callsigns[n].clone(),
                    class.hull,
                    class.shields,
                );
                let ability = content.pilots.pilot(pilot_id).and_then(|p| p.ability);
                ship.lingers = ability == Some(PilotAbility::SurviveUntilEndOfCombat);
                ship.late_setup = ability == Some(PilotAbility::SetupAnywhereBeyondRange3);
                ship.upgrades = entry.upgrades.clone();
                if entry.upgrades.iter().any(|u| {
                    content.upgrades.upgrade(*u).and_then(|c| c.effect)
                        == Some(UpgradeEffect::OrdnanceTokens)
                }) {
                    ship.ordnance = entry
                        .upgrades
                        .iter()
                        .copied()
                        .filter(|u| {
                            content.upgrades.upgrade(*u).is_some_and(|c| {
                                matches!(c.slot, Slot::Torpedo | Slot::Missile | Slot::Bomb)
                            })
                        })
                        .collect();
                }
                ships.push(ship);
            }
        }
        // Hull Upgrade / Shield Upgrade raise the starting values.
        let mut gs_probe = Self {
            board,
            phase: Phase::Placement,
            turn: 1,
            ships,
            committed: vec![false; n],
            winner: None,
            initiative: PlayerId(0),
            squad_totals: vec![0; n],
            teams: teams.clone(),
            combat: None,
            bombs: Vec::new(),
            next_bomb_id: 0,
            obstacles: Vec::new(),
            white_reds: vec![false; n],
            mission: None,
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
        let squad_totals: Vec<u32> = squads.iter().map(|s| s.cost(content)).collect();
        // Initiative goes to the side with the lowest total; the token
        // sits with that side's first seat.
        let sides = teams.iter().copied().max().unwrap_or(0) as usize + 1;
        let side_totals: Vec<u32> = (0..sides)
            .map(|t| {
                squad_totals
                    .iter()
                    .zip(&teams)
                    .filter(|(_, x)| **x as usize == t)
                    .map(|(c, _)| c)
                    .sum()
            })
            .collect();
        let side = initiative_seat(&side_totals, tie_roll) as u8;
        let initiative = PlayerId(teams.iter().position(|t| *t == side).unwrap_or(0) as u32);
        Ok(Self {
            board,
            phase: Phase::Placement,
            turn: 1,
            ships,
            committed: vec![false; n],
            winner: None,
            initiative,
            squad_totals,
            teams,
            combat: None,
            bombs: Vec::new(),
            next_bomb_id: 0,
            obstacles: Vec::new(),
            white_reds: vec![false; n],
            mission: None,
        })
    }

    /// The side (team) of a seat.
    pub fn team(&self, player: PlayerId) -> u8 {
        self.teams.get(player.0 as usize).copied().unwrap_or(player.0 as u8)
    }

    /// Number of sides in the game.
    pub fn sides(&self) -> u8 {
        self.teams.iter().copied().max().map_or(2, |m| m + 1)
    }

    /// Are two seats on the same side (a seat is allied with itself)?
    pub fn allied(&self, a: PlayerId, b: PlayerId) -> bool {
        self.team(a) == self.team(b)
    }

    /// The board edge a seat deploys from: its side's edge.
    pub fn seat_of(&self, player: PlayerId) -> Seat {
        Seat::for_side(self.team(player), self.sides())
    }

    /// Tie-break rank per seat for equal pilot skill: the initiative
    /// side first (its initiative seat, then its other seats in seat
    /// order), then the other sides in seat order.
    fn seat_ranks(&self) -> Vec<u8> {
        let n = self.committed.len().max(2);
        let init = self.initiative.0 as usize;
        let init_team = self.team(self.initiative);
        let mut order: Vec<usize> = vec![init];
        order.extend((0..n).filter(|&s| s != init && self.team(PlayerId(s as u32)) == init_team));
        order.extend((0..n).filter(|&s| self.team(PlayerId(s as u32)) != init_team));
        let mut ranks = vec![u8::MAX; n];
        for (rank, seat) in order.into_iter().enumerate() {
            ranks[seat] = rank as u8;
        }
        ranks
    }

    /// Sides with at least one ship still in play.
    fn alive_teams(&self) -> Vec<u8> {
        let mut v: Vec<u8> =
            self.ships.iter().filter(|s| !s.destroyed).map(|s| self.team(s.owner)).collect();
        v.sort_unstable();
        v.dedup();
        v
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
        // "Epsilon Ace": skill 12 while no Damage card is assigned.
        if self.ability(content, s) == Some(PilotAbility::SkillTwelveWhileUndamaged)
            && s.hull == self.max_hull(content, s)
        {
            return 12;
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
        // Gemmer Sojan: +1 agility while an enemy ship is at Range 1.
        let gemmer = self.ability(content, s) == Some(PilotAbility::AgilityPlus1IfEnemyAtRange1)
            && self.ships.iter().position(|x| x.id == s.id).is_some_and(|i| {
                (0..self.ships.len()).any(|e| {
                    !self.allied(self.ships[e].owner, s.owner)
                        && !self.ships[e].destroyed
                        && self.range_between(content, i, e) == Some(1)
                })
            });
        let r2f2 = u8::from(self.card_action_active(content, s, UpgradeEffect::AgilityPlus1Action));
        let expose = u8::from(self.card_action_active(content, s, UpgradeEffect::ExposeAction));
        (self.printed(content, s).agility
            + self.count_effect(content, s, UpgradeEffect::AgilityPlus1DiscardWhenHit)
            + u8::from(gemmer)
            + r2f2)
            .saturating_sub(structural + expose + s.tractor)
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
        self.friends_within(content, s, 1)
    }

    /// Other living friendly ships within Range 1-`band` of ship `s`.
    fn friends_within(&self, content: &Content, s: usize, band: u8) -> Vec<usize> {
        (0..self.ships.len())
            .filter(|&o| {
                o != s
                    && self.allied(self.ships[o].owner, self.ships[s].owner)
                    && !self.ships[o].destroyed
                    && self.range_between(content, s, o).is_some_and(|r| r <= band)
            })
            .collect()
    }

    fn has_effect(&self, content: &Content, i: usize, e: UpgradeEffect) -> bool {
        self.count_effect(content, &self.ships[i], e) > 0
    }

    /// The first equipped card carrying effect `e` (to discard it).
    fn card_with_effect(&self, content: &Content, i: usize, e: UpgradeEffect) -> Option<UpgradeId> {
        self.ships[i]
            .upgrades
            .iter()
            .copied()
            .find(|u| content.upgrades.upgrade(*u).and_then(|c| c.effect) == Some(e))
    }

    /// Use a once-per-round card effect on ship `i`: the card if it is
    /// carried and unused this round (now marked used), else None.
    fn use_once(&mut self, content: &Content, i: usize, e: UpgradeEffect) -> Option<UpgradeId> {
        let card = self.ships[i].upgrades.iter().copied().find(|u| {
            content.upgrades.upgrade(*u).and_then(|c| c.effect) == Some(e)
                && !self.ships[i].used_round.contains(u)
        })?;
        self.ships[i].used_round.push(card);
        Some(card)
    }

    /// Agent Kallus / A Score to Settle: the enemy chosen at setup — the
    /// most expensive one (lowest id on a tie), chosen once and for all.
    fn marked_enemy(&self, content: &Content, i: usize, e: UpgradeEffect) -> Option<ShipId> {
        if !self.has_effect(content, i, e) {
            return None;
        }
        self.ships
            .iter()
            .filter(|s| !self.allied(s.owner, self.ships[i].owner))
            .max_by_key(|s| {
                (content.pilots.pilot(s.pilot).map(|p| p.cost).unwrap_or(0), u32::MAX - s.id.0)
            })
            .map(|s| s.id)
    }

    /// Is ship `i` touching (bases in contact with) an enemy ship that
    /// carries Intimidation?
    fn touching_intimidator(&self, content: &Content, i: usize) -> bool {
        let Some(p) = self.ships[i].pose else { return false };
        let mine = rules::footprint_corners(p, self.class_of(content, &self.ships[i]).footprint);
        (0..self.ships.len()).any(|k| {
            let s = &self.ships[k];
            !self.allied(s.owner, self.ships[i].owner)
                && !s.destroyed
                && self.has_effect(content, k, UpgradeEffect::ReduceAgilityWhileTouching)
                && s.pose.is_some_and(|q| {
                    let theirs = rules::footprint_corners(q, self.class_of(content, s).footprint);
                    combat::base_distance(&mine, &theirs) <= 0.0
                })
        })
    }

    /// Swarm Leader: up to two other friends with the defender in arc at
    /// Range 1-3 each give up an evade token for one more attack die.
    fn swarm_leader_dice(
        &mut self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        events: &mut Vec<String>,
    ) -> u8 {
        let friends: Vec<usize> = (0..self.ships.len())
            .filter(|&f| {
                f != a_idx
                    && self.allied(self.ships[f].owner, self.ships[a_idx].owner)
                    && !self.ships[f].destroyed
                    && self.ships[f].evade > 0
                    && self.range_between(content, f, d_idx).is_some()
                    && self.ship_in_front_arc(content, f, d_idx)
            })
            .take(2)
            .collect();
        for &f in &friends {
            self.ships[f].evade -= 1;
            events.push(format!(
                "{}: Swarm Leader — {} gives up an evade token, +1 attack die",
                self.label(content, a_idx),
                self.label(content, f)
            ));
        }
        friends.len() as u8
    }

    /// Targeting Synchronizer: a friend within Range 1-2 of the attacker
    /// holds a lock on `target` and shares it.
    fn synced_lock(&self, content: &Content, a_idx: usize, target: ShipId) -> bool {
        self.friends_within(content, a_idx, 2).into_iter().any(|f| {
            self.ships[f].locks_on(target)
                && self.has_effect(content, f, UpgradeEffect::ShareLockWithFriendly)
        })
    }

    /// Is a living enemy of ship `i` with `ability` at Range 1 of it?
    fn enemy_ability_at_range1(&self, content: &Content, i: usize, ability: PilotAbility) -> bool {
        (0..self.ships.len()).any(|e| {
            !self.allied(self.ships[e].owner, self.ships[i].owner)
                && !self.ships[e].destroyed
                && self.ability(content, &self.ships[e]) == Some(ability)
                && self.range_between(content, i, e) == Some(1)
        })
    }

    /// Carnor Jax at Range 1: no focus or evade actions, no spending
    /// focus or evade tokens.
    fn carnor_near(&self, content: &Content, i: usize) -> bool {
        self.enemy_ability_at_range1(content, i, PilotAbility::DenyFocusEvadeAtRange1)
    }

    /// The faceup card an attack deals: Maarek Stele draws three and
    /// keeps the worst for the defender.
    fn draw_crit_for(
        &self,
        content: &Content,
        a_idx: usize,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> CritEffect {
        if self.ability(content, &self.ships[a_idx]) != Some(PilotAbility::ChooseCritFromThree) {
            return crit::draw(roll());
        }
        let three = [crit::draw(roll()), crit::draw(roll()), crit::draw(roll())];
        let pick = three.iter().max_by_key(|e| e.severity()).copied().expect("three cards");
        events.push(format!(
            "{}: Maarek Stele — draws {}, {} and {}; chooses {}",
            self.label(content, a_idx),
            three[0].name(),
            three[1].name(),
            three[2].name(),
            pick.name()
        ));
        pick
    }

    /// Extra Munitions: spend an ordnance token on `card` instead of
    /// discarding it. True when a token was spent (the card stays).
    fn spend_ordnance(
        &mut self,
        content: &Content,
        i: usize,
        card: UpgradeId,
        events: &mut Vec<String>,
    ) -> bool {
        let Some(k) = self.ships[i].ordnance.iter().position(|u| *u == card) else {
            return false;
        };
        self.ships[i].ordnance.remove(k);
        let name = content.upgrades.upgrade(card).map(|c| c.name.clone()).unwrap_or_default();
        events.push(format!(
            "{}: Extra Munitions — ordnance token spent, {name} kept",
            self.label(content, i)
        ));
        true
    }

    /// Acquire a lock for ship `i` on the nearest enemy within Range 1-3
    /// when it holds none. True when a lock was taken.
    fn auto_lock(
        &mut self,
        content: &Content,
        i: usize,
        why: &str,
        events: &mut Vec<String>,
    ) -> bool {
        self.auto_lock_within(content, i, 3, why, events)
    }

    /// `auto_lock` limited to enemies within `bands` range bands. Captain
    /// Kagi, when lockable, takes the lock whoever is nearer.
    fn auto_lock_within(
        &mut self,
        content: &Content,
        i: usize,
        bands: u8,
        why: &str,
        events: &mut Vec<String>,
    ) -> bool {
        if !self.ships[i].lock_free(self.two_locks(content, i)) || self.ships[i].destroyed {
            return false;
        }
        let Some(p) = self.ships[i].pose else { return false };
        let mine = rules::footprint_corners(p, self.class_of(content, &self.ships[i]).footprint);
        let candidates: Vec<(usize, f64)> = (0..self.ships.len())
            .filter(|&e| {
                !self.allied(self.ships[e].owner, self.ships[i].owner)
                    && !self.ships[e].destroyed
                    && !self.ships[i].locks_on(self.ships[e].id)
            })
            .filter_map(|e| {
                let q = self.ships[e].pose?;
                let theirs =
                    rules::footprint_corners(q, self.class_of(content, &self.ships[e]).footprint);
                let d = combat::base_distance(&mine, &theirs);
                (d <= f64::from(bands) * combat::RANGE_BAND_UNITS).then_some((e, d))
            })
            .collect();
        let kagi = candidates.iter().find(|(e, _)| {
            self.ability(content, &self.ships[*e]) == Some(PilotAbility::EnemyLocksMustTargetMe)
        });
        let target =
            kagi.or_else(|| candidates.iter().min_by(|a, b| a.1.total_cmp(&b.1))).map(|(e, _)| *e);
        let Some(e) = target else { return false };
        let two = self.two_locks(content, i);
        let eid = self.ships[e].id;
        self.ships[i].take_lock(eid, two);
        events.push(format!(
            "{}: {why} — locks {}",
            self.label(content, i),
            self.label(content, e)
        ));
        true
    }

    /// Black One: after a boost or barrel roll, one enemy lock on a
    /// friendly ship at Range 1 (this one first) is removed.
    fn after_reposition_cards(&mut self, content: &Content, i: usize, events: &mut Vec<String>) {
        if !self.has_effect(content, i, UpgradeEffect::RemoveEnemyLockAfterReposition) {
            return;
        }
        let owner = self.ships[i].owner;
        let mut friends = vec![i];
        friends.extend(self.friends_at_range1(content, i));
        for f in friends {
            let fid = self.ships[f].id;
            if let Some(e) = (0..self.ships.len())
                .find(|&e| !self.allied(self.ships[e].owner, owner) && self.ships[e].locks_on(fid))
            {
                self.ships[e].drop_lock(fid);
                events.push(format!(
                    "{}: Black One — {}'s lock on {} removed",
                    self.label(content, i),
                    self.label(content, e),
                    self.label(content, f)
                ));
                return;
            }
        }
    }

    fn discard_card(&mut self, content: &Content, i: usize, card: UpgradeId, why: &str) -> String {
        let name = content.upgrades.upgrade(card).map(|c| c.name.clone()).unwrap_or_default();
        if self.tomax_keeps(content, i, card) {
            return format!(
                "{}: {name} — {why}, card discarded; Tomax Bren flips it back faceup",
                self.label(content, i)
            );
        }
        self.ships[i].upgrades.retain(|u| *u != card);
        format!("{}: {name} — {why}, card discarded", self.label(content, i))
    }

    /// Tomax Bren: once per round a discarded Elite Pilot Talent is
    /// flipped faceup again (the card stays equipped).
    fn tomax_keeps(&mut self, content: &Content, i: usize, card: UpgradeId) -> bool {
        let talent = content.upgrades.upgrade(card).is_some_and(|c| c.slot == Slot::Talent);
        if !talent
            || self.ability(content, &self.ships[i])
                != Some(PilotAbility::FlipTalentFaceupAfterDiscard)
            || self.ships[i].used_round.contains(&card)
        {
            return false;
        }
        self.ships[i].used_round.push(card);
        true
    }

    /// Any living enemy inside ship `i`'s firing arc at Range 1?
    fn enemy_in_arc_at_range1(&self, content: &Content, i: usize) -> bool {
        (0..self.ships.len()).any(|e| {
            !self.allied(self.ships[e].owner, self.ships[i].owner)
                && !self.ships[e].destroyed
                && self.range_between(content, i, e) == Some(1)
                && self.ship_in_front_arc(content, i, e)
        })
    }

    /// The color a maneuver is flown at by ship `i`: crits first (Damaged
    /// Engine / Thrust Control Fire redden), then cards that green it (R2
    /// Astromech speed 1-2 straights, Nien Nunb crew straights, Twin Ion
    /// Engine Mk. II banks), Ello Asty's white Tallon Rolls while
    /// unstressed, and Adrenaline Rush turning a red maneuver white — the
    /// second value is that card, to discard when the maneuver is flown.
    fn maneuver_difficulty(
        &self,
        content: &Content,
        i: usize,
        man: &Maneuver,
    ) -> (Difficulty, Option<UpgradeId>) {
        use maneuver::Steer;
        let s = &self.ships[i];
        let mut d = crit::effective_difficulty(&s.crits, man);
        let straight = man.steer == Steer::Straight;
        let bank = matches!(man.steer, Steer::BankLeft | Steer::BankRight);
        let tallon = matches!(man.steer, Steer::TallonLeft | Steer::TallonRight);
        if d == Difficulty::Hard
            && tallon
            && s.stress == 0
            && self.ability(content, s) == Some(PilotAbility::TallonWhiteWhileUnstressed)
        {
            d = Difficulty::Normal;
        }
        if (straight
            && man.distance <= 2
            && self.has_effect(content, i, UpgradeEffect::Speed1And2AreGreen))
            || (straight && self.has_effect(content, i, UpgradeEffect::CrewStraightsAreGreen))
            || (bank && self.has_effect(content, i, UpgradeEffect::BanksAreGreen))
        {
            d = Difficulty::Easy;
        }
        // Leia Organa (crew), discarded this round: reds are white.
        if d == Difficulty::Hard && self.white_reds[s.owner.0 as usize] {
            d = Difficulty::Normal;
        }
        let mut rush = None;
        if d == Difficulty::Hard
            && let Some(card) =
                self.card_with_effect(content, i, UpgradeEffect::TreatRedAsWhiteDiscard)
        {
            d = Difficulty::Normal;
            rush = Some(card);
        }
        (d, rush)
    }

    /// Ship `i` receives a stress token. Nien Nunb (T-70) discards it when
    /// an enemy is in arc at Range 1; Soontir Fel gains a focus token;
    /// Cool Hand is discarded for a focus token.
    fn gain_stress(&mut self, content: &Content, i: usize, events: &mut Vec<String>) {
        // Captain Yorr: a friend at Range 1-2 with two or fewer stress
        // tokens takes the token instead (never from another Yorr).
        let yorr = PilotAbility::AbsorbFriendlyStressAtRange1To2;
        if self.ability(content, &self.ships[i]) != Some(yorr)
            && let Some(y) = self.friends_within(content, i, 2).into_iter().find(|&y| {
                self.ability(content, &self.ships[y]) == Some(yorr) && self.ships[y].stress <= 2
            })
        {
            events.push(format!(
                "{}: Captain Yorr — takes the stress token meant for {}",
                self.label(content, y),
                self.label(content, i)
            ));
            self.gain_stress(content, y, events);
            return;
        }
        self.ships[i].stress += 1;
        let who = self.label(content, i);
        // Electronic Baffle (switched on): one damage sheds the token —
        // never when that damage would destroy the ship.
        if self.using(content, i, UpgradeEffect::SystemDamageToDiscardToken).is_some()
            && (self.ships[i].shields > 0 || self.ships[i].hull > 1)
        {
            self.damage_point(i);
            self.ships[i].stress -= 1;
            events.push(format!("{who}: Electronic Baffle — 1 damage, stress token discarded"));
            return;
        }
        match self.ability(content, &self.ships[i]) {
            Some(PilotAbility::DiscardStressIfEnemyInArcRange1)
                if self.enemy_in_arc_at_range1(content, i) =>
            {
                self.ships[i].stress -= 1;
                events.push(format!("{who}: ability — stress discarded (enemy in arc at Range 1)"));
            }
            Some(PilotAbility::FocusOnStress) => {
                self.ships[i].focus += 1;
                events.push(format!("{who}: ability — focus token for the stress"));
            }
            _ => {}
        }
        if let Some(card) = self.card_with_effect(content, i, UpgradeEffect::FocusOrEvadeOnStress) {
            self.ships[i].focus += 1;
            let e = self.discard_card(content, i, card, "focus token for the stress");
            events.push(e);
        }
    }

    /// Ship `i` removes a stress token if it has one (Kyle Katarn crew
    /// then grants a focus token). Returns whether one was removed.
    fn lose_stress(&mut self, content: &Content, i: usize, events: &mut Vec<String>) -> bool {
        if self.ships[i].stress == 0 {
            return false;
        }
        self.ships[i].stress -= 1;
        if self.has_effect(content, i, UpgradeEffect::CrewFocusAfterStressRemoved) {
            self.ships[i].focus += 1;
            events.push(format!("{}: Kyle Katarn — focus token", self.label(content, i)));
        }
        true
    }

    /// "Red Ace": the first shield lost each round grants an evade token.
    fn after_shield_loss(&mut self, content: &Content, i: usize, events: &mut Vec<String>) {
        if !self.ships[i].shield_lost_round
            && self.ability(content, &self.ships[i]) == Some(PilotAbility::EvadeOnFirstShieldLoss)
        {
            self.ships[i].evade += 1;
            events.push(format!(
                "{}: ability — evade token (first shield lost)",
                self.label(content, i)
            ));
        }
        self.ships[i].shield_lost_round = true;
    }

    /// "Chaser": a focus token whenever another friendly ship at Range 1
    /// spends one.
    fn friend_spent_focus(&mut self, content: &Content, spender: usize, events: &mut Vec<String>) {
        // Garven Dreis: the spent token goes to another friendly ship at
        // Range 1-2 (policy: the one holding the fewest focus tokens).
        if self.ability(content, &self.ships[spender])
            == Some(PilotAbility::PassSpentFocusRange1To2)
            && let Some(f) = self
                .friends_within(content, spender, 2)
                .into_iter()
                .min_by_key(|&f| (self.ships[f].focus, f))
        {
            self.ships[f].focus += 1;
            events.push(format!(
                "{}: Garven Dreis — spent focus token passed to {}",
                self.label(content, spender),
                self.label(content, f)
            ));
        }
        for f in self.friends_at_range1(content, spender) {
            if self.ability(content, &self.ships[f])
                == Some(PilotAbility::FocusWhenFriendlySpendsFocusRange1)
            {
                self.ships[f].focus += 1;
                events.push(format!(
                    "{}: ability — focus token (friend spent one)",
                    self.label(content, f)
                ));
            }
        }
    }

    /// Talent rerolls on attack, after the lock and friendly rerolls:
    /// Predator (1 die, 2 against pilot skill 2 or lower), Lone Wolf (1
    /// blank when no friend is within Range 2), Wired (every focus
    /// result while stressed, when no focus token could convert them).
    fn talent_attack_rerolls(
        &self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        faces: &mut [AttackFace],
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        if self.has_effect(content, a_idx, UpgradeEffect::RerollOneAttackDie) {
            let n = if self.effective_skill(content, &self.ships[d_idx]) <= 2 { 2 } else { 1 };
            let done = self.reroll_attack_dice(a_idx, faces, n, roll);
            if done > 0 {
                events.push(format!(
                    "{}: Predator — rerolls {done} attack dice",
                    self.label(content, a_idx)
                ));
            }
        }
        if self.card_action_active(
            content,
            &self.ships[a_idx],
            UpgradeEffect::RerollUpTo3ForFocusAnd2Stress,
        ) {
            let done = self.reroll_attack_dice(a_idx, faces, 3, roll);
            if done > 0 {
                events.push(format!(
                    "{}: Rage — rerolls {done} attack dice",
                    self.label(content, a_idx)
                ));
            }
        }
        if self.has_effect(content, a_idx, UpgradeEffect::RerollBlankIfAlone)
            && self.friends_within(content, a_idx, 2).is_empty()
        {
            let n =
                reroll_matching(
                    faces,
                    &[AttackFace::Blank],
                    1,
                    &mut || AttackFace::from_d8(roll()),
                );
            if n > 0 {
                events.push(format!("{}: Lone Wolf — rerolls a blank", self.label(content, a_idx)));
            }
        }
        if self.has_effect(content, a_idx, UpgradeEffect::RerollFocusWhenStressed)
            && self.ships[a_idx].stress > 0
            && self.ships[a_idx].focus == 0
        {
            let n = reroll_matching(faces, &[AttackFace::Focus], u8::MAX, &mut || {
                AttackFace::from_d8(roll())
            });
            if n > 0 {
                events.push(format!(
                    "{}: Wired — rerolls {n} focus results while stressed",
                    self.label(content, a_idx)
                ));
            }
        }
        // Horton Salm: every blank rerolled at Range 2-3. Rey: up to two
        // blanks rerolled with the enemy in arc.
        let range = self.range_between(content, a_idx, d_idx).unwrap_or(0);
        let ability = self.ability(content, &self.ships[a_idx]);
        if ability == Some(PilotAbility::RerollBlanksAtRange2To3) && (2..=3).contains(&range) {
            let n = reroll_matching(faces, &[AttackFace::Blank], u8::MAX, &mut || {
                AttackFace::from_d8(roll())
            });
            if n > 0 {
                events.push(format!(
                    "{}: ability — rerolls {n} blanks at Range {range}",
                    self.label(content, a_idx)
                ));
            }
        }
        if ability == Some(PilotAbility::RerollTwoBlanksIfEnemyInArc)
            && self.ship_in_front_arc(content, a_idx, d_idx)
        {
            let n =
                reroll_matching(
                    faces,
                    &[AttackFace::Blank],
                    2,
                    &mut || AttackFace::from_d8(roll()),
                );
            if n > 0 {
                events.push(format!(
                    "{}: ability — rerolls {n} blanks (enemy in arc)",
                    self.label(content, a_idx)
                ));
            }
        }
    }

    /// The same talents on defense (Lone Wolf, Wired, Rey); only called
    /// when damage would otherwise still land.
    fn talent_defense_rerolls(
        &self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        faces: &mut [DefenseFace],
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        // Rey: up to two blanks rerolled with the attacker in her arc.
        if self.ability(content, &self.ships[d_idx])
            == Some(PilotAbility::RerollTwoBlanksIfEnemyInArc)
            && self.ship_in_front_arc(content, d_idx, a_idx)
        {
            let n = reroll_matching(faces, &[DefenseFace::Blank], 2, &mut || {
                DefenseFace::from_d8(roll())
            });
            if n > 0 {
                events.push(format!(
                    "{}: ability — rerolls {n} blanks (enemy in arc)",
                    self.label(content, d_idx)
                ));
            }
        }
        if self.has_effect(content, d_idx, UpgradeEffect::RerollBlankIfAlone)
            && self.friends_within(content, d_idx, 2).is_empty()
        {
            let n = reroll_matching(faces, &[DefenseFace::Blank], 1, &mut || {
                DefenseFace::from_d8(roll())
            });
            if n > 0 {
                events.push(format!("{}: Lone Wolf — rerolls a blank", self.label(content, d_idx)));
            }
        }
        if self.has_effect(content, d_idx, UpgradeEffect::RerollFocusWhenStressed)
            && self.ships[d_idx].stress > 0
            && self.ships[d_idx].focus == 0
        {
            let n = reroll_matching(faces, &[DefenseFace::Focus], u8::MAX, &mut || {
                DefenseFace::from_d8(roll())
            });
            if n > 0 {
                events.push(format!(
                    "{}: Wired — rerolls {n} focus results while stressed",
                    self.label(content, d_idx)
                ));
            }
        }
    }

    /// After the attacker has modified its dice, the defender may force
    /// rerolls: Elusiveness (take a stress while unstressed to reroll the
    /// attacker's best die) and R7 Astromech (spend a target lock on the
    /// attacker to reroll every hit and critical). The rerolled results
    /// stand. Used whenever at least one hit or critical is showing.
    fn defender_forces_rerolls(
        &mut self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        faces: &mut [AttackFace],
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        let landing = |faces: &[AttackFace]| {
            faces.iter().any(|f| matches!(f, AttackFace::Hit | AttackFace::Crit))
        };
        if landing(faces)
            && self.ships[d_idx].stress == 0
            && self.has_effect(content, d_idx, UpgradeEffect::ForceRerollForStress)
        {
            let best = faces
                .iter()
                .position(|f| *f == AttackFace::Crit)
                .or_else(|| faces.iter().position(|f| *f == AttackFace::Hit))
                .expect("a hit or crit is showing");
            faces[best] = AttackFace::from_d8(roll());
            events.push(format!(
                "{}: Elusiveness — takes a stress, attacker rerolls a die",
                self.label(content, d_idx)
            ));
            self.gain_stress(content, d_idx, events);
        }
        if landing(faces)
            && self.ships[d_idx].locks_on(self.ships[a_idx].id)
            && self.has_effect(content, d_idx, UpgradeEffect::ForceRerollWithLock)
        {
            let aid = self.ships[a_idx].id;
            self.ships[d_idx].drop_lock(aid);
            let n =
                reroll_matching(faces, &[AttackFace::Crit, AttackFace::Hit], u8::MAX, &mut || {
                    AttackFace::from_d8(roll())
                });
            events.push(format!(
                "{}: R7 Astromech — lock spent, attacker rerolls {n} dice",
                self.label(content, d_idx)
            ));
        }
        // M9-G8: every ship holding a lock on the attacker with the
        // astromech forces one reroll — an enemy picks the best die, a
        // friend (locked on purpose) a blank.
        let attacker = self.ships[a_idx].id;
        let holders: Vec<usize> = (0..self.ships.len())
            .filter(|&k| {
                !self.ships[k].destroyed
                    && self.ships[k].locks_on(attacker)
                    && self.has_effect(content, k, UpgradeEffect::ForceRerollLockedAttacker)
            })
            .collect();
        for k in holders {
            let enemy = !self.allied(self.ships[k].owner, self.ships[a_idx].owner);
            let pick = if enemy {
                faces
                    .iter()
                    .position(|f| *f == AttackFace::Crit)
                    .or_else(|| faces.iter().position(|f| *f == AttackFace::Hit))
            } else {
                faces.iter().position(|f| *f == AttackFace::Blank)
            };
            if let Some(p) = pick {
                faces[p] = AttackFace::from_d8(roll());
                events.push(format!(
                    "{}: M9-G8 — the locked attacker rerolls a die",
                    self.label(content, k)
                ));
            }
        }
    }

    /// Opportunist: +1 attack die for a stress token when the defender
    /// holds no focus or evade token and the attacker is unstressed.
    /// Always taken.
    fn opportunist_die(
        &mut self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        events: &mut Vec<String>,
    ) -> u8 {
        if self.has_effect(content, a_idx, UpgradeEffect::ExtraAttackDieForStress)
            && self.ships[a_idx].stress == 0
            && self.ships[d_idx].focus == 0
            && self.ships[d_idx].evade == 0
        {
            events.push(format!(
                "{}: Opportunist — takes a stress token for +1 attack die",
                self.label(content, a_idx)
            ));
            self.gain_stress(content, a_idx, events);
            1
        } else {
            0
        }
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
        a_idx: usize,
        faces: &mut [AttackFace],
        n: u8,
        roll: &mut dyn FnMut() -> u8,
    ) -> u8 {
        let wants: &[AttackFace] = if self.ships[a_idx].focus == 0 {
            &[AttackFace::Blank, AttackFace::Focus]
        } else {
            &[AttackFace::Blank]
        };
        reroll_matching(faces, wants, n, &mut || AttackFace::from_d8(roll()))
    }

    /// Reroll up to `n` defense dice on the same policy.
    fn reroll_defense_dice(
        &self,
        d_idx: usize,
        faces: &mut [DefenseFace],
        n: u8,
        roll: &mut dyn FnMut() -> u8,
    ) -> u8 {
        let wants: &[DefenseFace] = if self.ships[d_idx].focus == 0 {
            &[DefenseFace::Blank, DefenseFace::Focus]
        } else {
            &[DefenseFace::Blank]
        };
        reroll_matching(faces, wants, n, &mut || DefenseFace::from_d8(roll()))
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
                self.gain_stress(content, a_idx, events);
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
            || !ship.locks_on(defender)
            || ship.focus == 0
            || faces.is_empty()
        {
            return false;
        }
        self.ships[a_idx].drop_lock(defender);
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
    /// One point of damage straight to the hull (a faceup card dealt
    /// past the shields).
    fn hull_point(&mut self, i: usize) -> DamagePoint {
        let s = &mut self.ships[i];
        if s.hull > 0 {
            s.hull -= 1;
            if s.hull == 0 && !s.lingers {
                s.destroyed = true;
            }
            DamagePoint::Hull
        } else {
            DamagePoint::None
        }
    }

    /// "After you perform this attack" / "if this attack hits" riders on
    /// weapon cards, resolved after damage is dealt.
    fn after_attack_effects(
        &mut self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        effect: UpgradeEffect,
        landed: bool,
        events: &mut Vec<String>,
    ) {
        let who = self.label(content, d_idx);
        match effect {
            // Flechette Torpedoes: stress if the defender's hull value is 4 or less.
            UpgradeEffect::TorpedoStressIfHullLow => {
                if self.printed(content, &self.ships[d_idx]).hull <= 4
                    && !self.ships[d_idx].destroyed
                {
                    self.ships[d_idx].stress += 1;
                    events.push(format!("{who}: flechette torpedoes — stressed"));
                }
            }
            // Plasma Torpedoes: after dealing damage, remove 1 shield token.
            UpgradeEffect::TorpedoStripShield if landed => {
                if self.ships[d_idx].shields > 0 {
                    self.ships[d_idx].shields -= 1;
                    events.push(format!("{who}: plasma torpedoes — 1 shield stripped"));
                }
            }
            // Ion Torpedoes: the defender and every ship at Range 1 of it are ionized.
            UpgradeEffect::TorpedoIonSplash if landed => {
                let near: Vec<usize> = (0..self.ships.len())
                    .filter(|&j| {
                        j != d_idx
                            && !self.ships[j].destroyed
                            && self.range_between(content, d_idx, j) == Some(1)
                    })
                    .collect();
                self.ships[d_idx].ion += 1;
                let mut names = vec![who.clone()];
                for j in near {
                    self.ships[j].ion += 1;
                    names.push(self.label(content, j));
                }
                events.push(format!("ion torpedoes — ionized: {}", names.join(", ")));
            }
            // Assault Missiles: each other ship at Range 1 of the defender suffers 1 damage.
            UpgradeEffect::MissileSplashRange1 if landed => {
                let near: Vec<usize> = (0..self.ships.len())
                    .filter(|&j| {
                        j != d_idx
                            && !self.ships[j].destroyed
                            && self.range_between(content, d_idx, j) == Some(1)
                    })
                    .collect();
                for j in near {
                    self.damage_point(j);
                    let died = if self.ships[j].destroyed { " — DESTROYED" } else { "" };
                    events.push(format!(
                        "{}: assault missiles splash — 1 damage{died}",
                        self.label(content, j)
                    ));
                }
            }
            // XX-23 S-Thread Tracers: friends at Range 1-2 of the attacker
            // lock the defender (those without a lock take it).
            UpgradeEffect::MissileFriendsLockOnHit if landed => {
                let defender = self.ships[d_idx].id;
                let friends: Vec<usize> = (0..self.ships.len())
                    .filter(|&j| {
                        j != a_idx
                            && self.allied(self.ships[j].owner, self.ships[a_idx].owner)
                            && !self.ships[j].destroyed
                            && self.ships[j].lock_free(self.two_locks(content, j))
                            && matches!(self.range_between(content, a_idx, j), Some(1 | 2))
                    })
                    .collect();
                for j in friends {
                    let two = self.two_locks(content, j);
                    self.ships[j].take_lock(defender, two);
                    events
                        .push(format!("{}: locks {who} (thread tracers)", self.label(content, j)));
                }
            }
            _ => {}
        }
    }

    /// Crew, talent and system effects that trigger after an attack:
    /// Tactician, Ruthlessness, Darth Vader (crew), Fire-Control System,
    /// R5-K6 and Operations Specialist.
    fn after_attack_cards(
        &mut self,
        content: &Content,
        a_idx: usize,
        shot: Shot,
        outcome: AttackOutcome,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        let Shot { d_idx, range, .. } = shot;
        let defender = self.ships[d_idx].id;
        let who = self.label(content, a_idx);
        // Tactician: a target at Range 2 inside the arc is stressed.
        if range == 2
            && !self.ships[d_idx].destroyed
            && self.has_effect(content, a_idx, UpgradeEffect::CrewStressTargetAtRange2InArc)
            && self.ship_in_front_arc(content, a_idx, d_idx)
        {
            events.push(format!("{who}: Tactician — {} is stressed", self.label(content, d_idx)));
            self.gain_stress(content, d_idx, events);
        }
        // Ruthlessness: after a hit, another ship at Range 1 of the
        // defender suffers 1 damage — an enemy if there is one, else a
        // friend (the card says must).
        if outcome.landed && self.has_effect(content, a_idx, UpgradeEffect::SplashDamageAfterHit) {
            let near: Vec<usize> = (0..self.ships.len())
                .filter(|&j| {
                    j != d_idx
                        && j != a_idx
                        && !self.ships[j].destroyed
                        && self.range_between(content, d_idx, j) == Some(1)
                })
                .collect();
            let pick = near
                .iter()
                .copied()
                .find(|&j| !self.allied(self.ships[j].owner, self.ships[a_idx].owner))
                .or_else(|| near.first().copied());
            if let Some(j) = pick {
                self.damage_point(j);
                let died = if self.ships[j].destroyed { " — DESTROYED" } else { "" };
                events.push(format!(
                    "{who}: Ruthlessness — {} suffers 1 damage{died}",
                    self.label(content, j)
                ));
            }
        }
        // Darth Vader (crew): two damage to the attacker for a critical
        // hit on the defender — taken when it finishes the defender off
        // and the attacker keeps at least one hull.
        if !self.ships[d_idx].destroyed
            && !self.allied(self.ships[d_idx].owner, self.ships[a_idx].owner)
            && self.ships[d_idx].shields == 0
            && self.ships[d_idx].hull == 1
            && self.ships[a_idx].shields + self.ships[a_idx].hull >= 3
            && self.has_effect(content, a_idx, UpgradeEffect::CrewSufferTwoForCrit)
        {
            self.damage_point(a_idx);
            self.damage_point(a_idx);
            self.damage_point(d_idx);
            events.push(format!(
                "{who}: Darth Vader — suffers 2 damage; {} suffers a critical hit — DESTROYED",
                self.label(content, d_idx)
            ));
        }
        if self.ships[a_idx].destroyed {
            return;
        }
        // Fire-Control System: a lock on the defender afterwards.
        if !self.ships[d_idx].destroyed
            && !self.ships[a_idx].locks_on(defender)
            && self.has_effect(content, a_idx, UpgradeEffect::SystemLockAfterAttack)
        {
            let two = self.two_locks(content, a_idx);
            self.ships[a_idx].take_lock(defender, two);
            events
                .push(format!("{who}: Fire-Control System — locks {}", self.label(content, d_idx)));
        }
        // R5-K6: after spending the lock, a defense die may bring it back.
        if outcome.lock_spent
            && !self.ships[d_idx].destroyed
            && !self.ships[a_idx].locks_on(defender)
            && self.has_effect(content, a_idx, UpgradeEffect::ReLockOnEvadeDie)
        {
            if DefenseFace::from_d8(roll()) == DefenseFace::Evade {
                let two = self.two_locks(content, a_idx);
                self.ships[a_idx].take_lock(defender, two);
                events.push(format!(
                    "{who}: R5-K6 — evade rolled, lock on {} re-acquired",
                    self.label(content, d_idx)
                ));
            } else {
                events.push(format!("{who}: R5-K6 — no evade, lock not re-acquired"));
            }
        }
        // Operations Specialist: a friend's miss at Range 1-2 hands a
        // focus token to a friendly ship at Range 1-3 of the attacker
        // (one without tokens first).
        if !outcome.landed {
            let owner = self.ships[a_idx].owner;
            let specialists: Vec<usize> = (0..self.ships.len())
                .filter(|&k| {
                    self.allied(self.ships[k].owner, owner)
                        && !self.ships[k].destroyed
                        && self.has_effect(content, k, UpgradeEffect::CrewFocusAfterFriendlyMiss)
                        && (k == a_idx
                            || matches!(self.range_between(content, k, a_idx), Some(1 | 2)))
                })
                .collect();
            for k in specialists {
                let friends: Vec<usize> = (0..self.ships.len())
                    .filter(|&j| {
                        j != a_idx
                            && self.allied(self.ships[j].owner, owner)
                            && !self.ships[j].destroyed
                            && self.range_between(content, a_idx, j).is_some()
                    })
                    .collect();
                let pick = friends
                    .iter()
                    .copied()
                    .find(|&j| self.ships[j].focus == 0)
                    .or_else(|| friends.first().copied());
                if let Some(j) = pick {
                    self.ships[j].focus += 1;
                    events.push(format!(
                        "{}: Operations Specialist — focus token to {}",
                        self.label(content, k),
                        self.label(content, j)
                    ));
                }
            }
        }
    }

    fn damage_point(&mut self, i: usize) -> DamagePoint {
        let s = &mut self.ships[i];
        if s.shields > 0 {
            s.shields -= 1;
            DamagePoint::Shield
        } else if s.hull > 0 {
            s.hull -= 1;
            if s.hull == 0 && !s.lingers {
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
        // Chewbacca flips every faceup card facedown unresolved;
        // Determination discards Pilot-trait cards the same way.
        if self.ability(content, &self.ships[i]) == Some(PilotAbility::FlipCritFacedownImmediately)
        {
            events.push(format!(
                "{}: ability — card flipped facedown, no effect",
                self.label(content, i)
            ));
            return (0, 0);
        }
        if effect.is_pilot_trait()
            && self.has_effect(content, i, UpgradeEffect::DiscardPilotCritImmediately)
        {
            events.push(format!(
                "{}: Determination — Pilot card discarded, no effect",
                self.label(content, i)
            ));
            return (0, 0);
        }
        // Chewbacca (crew): the card is discarded outright and a shield
        // comes back; then the crew card goes. Moff Jerjerrod: discarded
        // to flip the card facedown. Integrated Astromech: the astromech
        // is discarded to discard the card.
        if let Some(card) =
            self.card_with_effect(content, i, UpgradeEffect::CrewDiscardDamageRecoverShield)
        {
            self.ships[i].hull += 1;
            if self.ships[i].shields < self.max_shields(content, &self.ships[i]) {
                self.ships[i].shields += 1;
            }
            let e = self.discard_card(content, i, card, "damage card discarded, shield recovered");
            events.push(e);
            return (0, 0);
        }
        if let Some(card) =
            self.card_with_effect(content, i, UpgradeEffect::CrewDiscardToFlipCritFacedown)
        {
            let e = self.discard_card(content, i, card, "damage card flipped facedown");
            events.push(e);
            return (0, 0);
        }
        if self.has_effect(content, i, UpgradeEffect::DiscardAstromechToCancelDamage)
            && let Some(astro) =
                self.ships[i].upgrades.iter().copied().find(|u| {
                    content.upgrades.upgrade(*u).is_some_and(|c| c.slot == Slot::Astromech)
                })
        {
            self.ships[i].hull += 1;
            let e =
                self.discard_card(content, i, astro, "Integrated Astromech: damage card discarded");
            events.push(e);
            return (0, 0);
        }
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
        // Han Solo (HotR): after everyone else, in the open, beyond Range 3
        // of every enemy.
        if self.ships[i].late_setup && self.turn == 1 && !self.late_setup_pending() {
            return Err(Rejection::PlaceLast);
        }
        let fp = self.class_of(content, &self.ships[i]).footprint;
        if self.ships[i].late_setup && self.turn == 1 {
            let corners = rules::footprint_corners(pose, fp);
            let near = self.ships.iter().any(|s| {
                !s.destroyed
                    && !self.allied(s.owner, player)
                    && s.pose.is_some_and(|p| {
                        let theirs =
                            rules::footprint_corners(p, self.class_of(content, s).footprint);
                        combat::range_band_between(&corners, &theirs).is_some()
                    })
            });
            if near {
                return Err(Rejection::TooCloseToEnemy);
            }
        }
        // Legality vs the player's OWN placed ships only — zones are
        // disjoint, and checking the opponent's would leak hidden info.
        let own_placed: Vec<(ShipId, Pose, Footprint)> = self
            .ships
            .iter()
            .filter(|s| self.allied(s.owner, player) && s.id != ship_id && !s.destroyed)
            .filter_map(|s| s.pose.map(|p| (s.id, p, self.class_of(content, s).footprint)))
            .collect();
        rules::placement_legal_in(&self.deploy_zones(player), pose, fp, &own_placed).map_err(
            |e| match e {
                rules::PlacementError::OutOfZone => Rejection::OutOfZone,
                rules::PlacementError::OverlapsShip(_) => Rejection::OverlapsShip,
            },
        )?;
        let corners = rules::footprint_corners(pose, fp);
        if self.on_obstacle(&corners).is_some() {
            return Err(Rejection::OverlapsObstacle);
        }
        // Mission 2: Rebel ships deploy beyond Range 1 of every asteroid
        // (the rulebook places the rocks around the ships; the rocks are
        // scattered first here, so the constraint is turned around).
        if self.turn == 1
            && self.mission.as_ref().is_some_and(|m| {
                m.kind == MissionKind::AsteroidRun && self.team(player) == m.rebel_side
            })
            && self
                .obstacles
                .iter()
                .any(|o| mission::outline_distance(&corners, &o.polygon()) <= mission::R1)
        {
            return Err(Rejection::TooCloseToObstacle);
        }
        self.ships[i].pose = Some(pose);
        if self.ships.iter().all(|s| s.pose.is_some()) {
            // Hyperwave Comm Scanner: at setup, every other friendly ship
            // placed at Range 1-2 gets a token (policy: focus). The skill
            // override for placement order has no meaning with hidden,
            // simultaneous placement.
            if self.turn == 1 {
                for k in 0..self.ships.len() {
                    if !self.has_effect(content, k, UpgradeEffect::SetupSkillOverride) {
                        continue;
                    }
                    for f in self.friends_within(content, k, 2) {
                        self.ships[f].focus += 1;
                    }
                }
            }
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
        if self.turn < mission::REPAIR_ROUND
            && man.distance > 2
            && self.mission.as_ref().is_some_and(|m| m.disabled == Some(ship_id))
        {
            return Err(Rejection::ShipDisabled);
        }
        // Crits can make normally-white maneuvers red (Damaged Engine /
        // Thrust Control Fire) — the stress rule uses the effective color.
        let (difficulty, _) = self.maneuver_difficulty(content, i, &man);
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
        let extras = self.action_extras(content, &self.ships[i]);
        let daredevil_turn = extras.daredevil
            && matches!(
                planned,
                PlannedAction::Boost(action::BoostDir::TurnLeft | action::BoostDir::TurnRight)
            );
        if let Some(kind) = planned.kind()
            && !self.action_bar(content, &self.ships[i]).contains(&kind)
            && !(kind == ActionKind::BarrelRoll && extras.expert_roll)
            && !daredevil_turn
        {
            return Err(Rejection::ActionNotOnBar);
        }
        if let PlannedAction::TargetLock(target) = planned {
            let t = self.ship_index(target)?;
            if self.allied(self.ships[t].owner, player) || self.ships[t].destroyed {
                return Err(Rejection::BadLockTarget);
            }
        }
        if planned == PlannedAction::Protect
            && !self.mission.as_ref().is_some_and(|m| {
                m.kind == MissionKind::PoliticalEscort
                    && self.team(player) == m.rebel_side
                    && m.shuttle != Some(ship_id)
            })
        {
            return Err(Rejection::NotInThisMission);
        }
        self.check_action_extras(content, i, planned)?;
        self.ships[i].planned_action = Some(planned);
        Ok(())
    }

    /// Card-bound checks shared by both action slots: mine cards, card
    /// actions, and the "Blue Ace" / "Zeta Ace" templates.
    fn check_action_extras(
        &self,
        content: &Content,
        i: usize,
        planned: PlannedAction,
    ) -> Result<(), Rejection> {
        let extras = self.action_extras(content, &self.ships[i]);
        match planned {
            PlannedAction::DropMine(card)
                if !self.bomb_kind(content, i, card).is_some_and(BombKind::is_mine) =>
            {
                Err(Rejection::NoSuchUpgrade)
            }
            PlannedAction::CardAction(card) if !extras.card_actions.contains(&card) => {
                Err(Rejection::NoSuchUpgrade)
            }
            PlannedAction::CardActionAt(card, obstacle) => {
                if !extras.card_actions.contains(&card) {
                    Err(Rejection::NoSuchUpgrade)
                } else if !self.obstacles.iter().any(|o| o.id == obstacle) {
                    Err(Rejection::NoSuchObstacle)
                } else {
                    Ok(())
                }
            }
            PlannedAction::Boost(action::BoostDir::TurnLeft | action::BoostDir::TurnRight)
                if !extras.turn_boost && !extras.daredevil =>
            {
                Err(Rejection::TemplateNotAllowed)
            }
            PlannedAction::BarrelRollFar(_) if !extras.far_roll => {
                Err(Rejection::TemplateNotAllowed)
            }
            PlannedAction::BarrelRollBank(..) if !extras.bank_roll => {
                Err(Rejection::TemplateNotAllowed)
            }
            _ => Ok(()),
        }
    }

    /// Secretly plan the ship's second action (None clears it). What is
    /// allowed depends on what grants it — see `SecondActionKind`.
    pub fn plan_second_action(
        &mut self,
        content: &Content,
        player: PlayerId,
        ship_id: ShipId,
        planned: Option<PlannedAction>,
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
        let Some(planned) = planned else {
            self.ships[i].planned_action2 = None;
            return Ok(());
        };
        let extras = self.action_extras(content, &self.ships[i]);
        let Some(kind) = extras.second else { return Err(Rejection::SecondActionNotAllowed) };
        let reposition = matches!(
            planned,
            PlannedAction::Boost(_)
                | PlannedAction::BarrelRoll(_)
                | PlannedAction::BarrelRollFar(_)
                | PlannedAction::BarrelRollBank(..)
        );
        let allowed = match kind {
            SecondActionKind::FreeBarAction => {
                planned != PlannedAction::Pass
                    && planned
                        .kind()
                        .is_some_and(|k| self.action_bar(content, &self.ships[i]).contains(&k))
            }
            SecondActionKind::TwoActions => planned != PlannedAction::Pass,
            SecondActionKind::CardActionThenStress => {
                matches!(planned, PlannedAction::CardAction(_) | PlannedAction::CardActionAt(..))
            }
            SecondActionKind::BoostAfterMove => matches!(planned, PlannedAction::Boost(_)),
            SecondActionKind::RepositionAfterFocus | SecondActionKind::RepositionAfterAttack => {
                reposition
            }
            SecondActionKind::RollOnGreenReveal => matches!(
                planned,
                PlannedAction::BarrelRoll(_)
                    | PlannedAction::BarrelRollFar(_)
                    | PlannedAction::BarrelRollBank(..)
            ),
        };
        if !allowed {
            return Err(Rejection::SecondActionNotAllowed);
        }
        // Granted repositions (BB-8, Snap, Jake Farrell) need no icon on
        // the bar; the other kinds are ordinary actions.
        let needs_icon =
            matches!(kind, SecondActionKind::FreeBarAction | SecondActionKind::TwoActions);
        if needs_icon
            && let Some(k) = planned.kind()
            && !self.action_bar(content, &self.ships[i]).contains(&k)
        {
            return Err(Rejection::ActionNotOnBar);
        }
        if let PlannedAction::TargetLock(target) = planned {
            let t = self.ship_index(target)?;
            if self.allied(self.ships[t].owner, player) || self.ships[t].destroyed {
                return Err(Rejection::BadLockTarget);
            }
        }
        self.check_action_extras(content, i, planned)?;
        self.ships[i].planned_action2 = Some(planned);
        Ok(())
    }

    /// What this ship may plan beyond its action bar.
    pub fn action_extras(&self, content: &Content, s: &ShipState) -> ActionExtras {
        let ability = self.ability(content, s);
        let second = if ability == Some(PilotAbility::TwoActions) {
            Some(SecondActionKind::TwoActions)
        } else if self.count_effect(content, s, UpgradeEffect::FreeActionThenStress) > 0 {
            Some(SecondActionKind::FreeBarAction)
        } else if ability == Some(PilotAbility::FreeBoostAfterSpeed2To4) {
            Some(SecondActionKind::BoostAfterMove)
        } else if ability == Some(PilotAbility::FreeRepositionAfterFocus) {
            Some(SecondActionKind::RepositionAfterFocus)
        } else if self.count_effect(content, s, UpgradeEffect::FreeBarrelRollOnGreen) > 0 {
            Some(SecondActionKind::RollOnGreenReveal)
        } else if ability == Some(PilotAbility::FreeRepositionAfterAttack) {
            Some(SecondActionKind::RepositionAfterAttack)
        } else if s.upgrades.iter().any(|u| {
            content.upgrades.upgrade(*u).and_then(|c| c.effect)
                == Some(UpgradeEffect::ExtraActionThenStress)
                && !s.used_round.contains(u)
        }) {
            Some(SecondActionKind::CardActionThenStress)
        } else {
            None
        };
        // Youngster: friendly ships of his class at Range 1-3 may use
        // his talent's action.
        let me = self.ships.iter().position(|x| x.id == s.id);
        let shared: Vec<UpgradeId> = me
            .map(|me| {
                (0..self.ships.len())
                    .filter(|&y| {
                        y != me
                            && self.allied(self.ships[y].owner, s.owner)
                            && !self.ships[y].destroyed
                            && self.ships[y].class == s.class
                            && self.ability(content, &self.ships[y])
                                == Some(PilotAbility::ShareTalentAction)
                            && self.range_between(content, me, y).is_some()
                    })
                    .flat_map(|y| self.ships[y].upgrades.clone())
                    .collect()
            })
            .unwrap_or_default();
        let card_actions = s
            .upgrades
            .iter()
            .copied()
            .chain(shared)
            .filter(|u| {
                matches!(
                    content.upgrades.upgrade(*u).and_then(|c| c.effect),
                    Some(
                        UpgradeEffect::FocusToCritOthersToHitAction
                            | UpgradeEffect::RerollUpTo3ForFocusAnd2Stress
                            | UpgradeEffect::ExposeAction
                            | UpgradeEffect::AgilityPlus1Action
                            | UpgradeEffect::SeismicTorpedoAction
                            | UpgradeEffect::CrewFleetOfficerAction
                            | UpgradeEffect::FreeActionForLowerSkillShip
                            | UpgradeEffect::LockAndBoostAction
                            | UpgradeEffect::CrewRollDefenseForTokensAction
                            | UpgradeEffect::CrewSaboteurAction
                            | UpgradeEffect::DiscardFacedownOnDefenseDie
                    )
                )
            })
            .collect();
        ActionExtras {
            second,
            turn_boost: ability == Some(PilotAbility::BoostWithTurnTemplate),
            far_roll: ability == Some(PilotAbility::BarrelRollWithStraight2),
            card_actions,
            bank_roll: ability == Some(PilotAbility::BarrelRollWithBank1ForStress),
            expert_roll: self.count_effect(content, s, UpgradeEffect::BarrelRollActionDiscardLock)
                > 0,
            daredevil: self.count_effect(content, s, UpgradeEffect::RedTurn1Action) > 0,
            toggles: self.toggle_cards(content, s),
        }
    }

    /// Was a card action with effect `e` performed this round?
    fn card_action_active(&self, content: &Content, s: &ShipState, e: UpgradeEffect) -> bool {
        s.card_actions
            .iter()
            .any(|u| content.upgrades.upgrade(*u).and_then(|c| c.effect) == Some(e))
    }

    /// Execute one action for ship `i` at its current pose. Repositions
    /// are blocked by other ships and the board edge.
    fn perform_action(
        &mut self,
        content: &Content,
        i: usize,
        planned: PlannedAction,
        fp: Footprint,
        obstacles: &[[Vec2; 4]],
        events: &mut Vec<String>,
    ) -> (ActionResult, Vec<BombToken>) {
        let pose = self.ships[i].pose.expect("acting ships are on the board");
        // Collision Detector: repositions may end on obstacles.
        let tokens: Vec<Vec<Vec2>> =
            if self.has_effect(content, i, UpgradeEffect::SystemOverlapObstaclesOnReposition) {
                Vec::new()
            } else {
                self.obstacles.iter().map(|o| o.polygon()).collect()
            };
        let clear = |board: &Board, candidate: Pose| {
            let corners = rules::footprint_corners(candidate, fp);
            rules::within_board(board, &corners)
                && obstacles.iter().all(|oc| !rules::obbs_overlap(&corners, oc))
                && tokens.iter().all(|t| !obstacle::convex_overlap(&corners, t))
        };
        let result = match planned {
            PlannedAction::Pass => ActionResult::Performed,
            // Mission 1: an evade token on the senator's shuttle, if it is
            // within Range 1 (no limit on how many it holds).
            PlannedAction::Protect => {
                let shuttle = self
                    .mission
                    .as_ref()
                    .and_then(|m| m.shuttle)
                    .and_then(|id| self.ships.iter().position(|s| s.id == id))
                    .filter(|&k| !self.ships[k].destroyed);
                match shuttle.filter(|&k| self.range_between(content, i, k) == Some(1)) {
                    Some(k) => {
                        self.ships[k].evade += 1;
                        events.push(format!(
                            "{}: protect — evade token on the senator's shuttle ({} held)",
                            self.label(content, i),
                            self.ships[k].evade
                        ));
                        ActionResult::Performed
                    }
                    None => {
                        events.push(format!(
                            "{}: action FAILED — the senator's shuttle is not within Range 1",
                            self.label(content, i)
                        ));
                        ActionResult::Failed
                    }
                }
            }
            // Carnor Jax at Range 1: no focus or evade actions.
            PlannedAction::Focus | PlannedAction::Evade if self.carnor_near(content, i) => {
                events.push(format!(
                    "{}: action FAILED — Carnor Jax at Range 1 forbids focus and evade",
                    self.label(content, i)
                ));
                ActionResult::Failed
            }
            PlannedAction::Focus => {
                self.ships[i].focus += 1;
                // Jan Ors (crew, switched on, once per round) on a friend
                // at Range 1-3: an evade token instead.
                let jan = (0..self.ships.len()).find(|&j| {
                    j != i
                        && !self.ships[j].destroyed
                        && self.allied(self.ships[j].owner, self.ships[i].owner)
                        && self.range_between(content, i, j).is_some()
                        && self
                            .using(content, j, UpgradeEffect::CrewEvadeInsteadOfFocusForFriendly)
                            .is_some_and(|c| !self.ships[j].used_round.contains(&c))
                });
                if let Some(j) = jan {
                    let card = self
                        .using(content, j, UpgradeEffect::CrewEvadeInsteadOfFocusForFriendly)
                        .expect("found above");
                    self.ships[j].used_round.push(card);
                    self.ships[i].focus -= 1;
                    self.ships[i].evade += 1;
                    events.push(format!(
                        "{}: Jan Ors — evade token instead of focus for {}",
                        self.label(content, j),
                        self.label(content, i)
                    ));
                }
                // Recon Specialist: a second focus token.
                if self.has_effect(content, i, UpgradeEffect::CrewExtraFocusOnFocusAction) {
                    self.ships[i].focus += 1;
                    events.push(format!(
                        "{}: Recon Specialist — extra focus token",
                        self.label(content, i)
                    ));
                }
                ActionResult::Performed
            }
            PlannedAction::Evade => {
                // Comm Relay caps the ship at one evade token.
                if self.ships[i].evade == 0
                    || !self.has_effect(content, i, UpgradeEffect::KeepOneEvade)
                {
                    self.ships[i].evade += 1;
                }
                ActionResult::Performed
            }
            PlannedAction::BarrelRoll(side)
            | PlannedAction::BarrelRollFar(side)
            | PlannedAction::BarrelRollBank(side, _) => {
                let candidate = match planned {
                    PlannedAction::BarrelRollFar(_) => {
                        action::barrel_roll_pose_with(pose, fp, side, 2.0)
                    }
                    PlannedAction::BarrelRollBank(_, forward) => {
                        action::barrel_roll_bank_pose(pose, fp, side, forward)
                    }
                    _ => action::barrel_roll_pose_with(pose, fp, side, 1.0),
                };
                if clear(&self.board, candidate) {
                    self.ships[i].pose = Some(candidate);
                    if matches!(planned, PlannedAction::BarrelRollBank(..)) {
                        events.push(format!(
                            "{}: Lieutenant Lorrir — bank template roll, 1 stress",
                            self.label(content, i)
                        ));
                        self.gain_stress(content, i, events);
                    }
                    self.after_expert_roll(content, i, events);
                    self.after_reposition_cards(content, i, events);
                    ActionResult::Performed
                } else {
                    ActionResult::Failed
                }
            }
            PlannedAction::Boost(dir) => {
                // Not a maneuver: no stress interaction. Blocked if it
                // would overlap a ship or leave the board. Daredevil (a
                // turn without Blue Ace's free template): the turn is red
                // — a stress token — and without the boost icon two
                // attack dice of damage are rolled against the ship.
                let daredevil =
                    matches!(dir, action::BoostDir::TurnLeft | action::BoostDir::TurnRight)
                        && self.ability(content, &self.ships[i])
                            != Some(PilotAbility::BoostWithTurnTemplate)
                        && self.has_effect(content, i, UpgradeEffect::RedTurn1Action);
                match maneuver::apply(pose, action::boost_maneuver(dir)) {
                    Ok(candidate) if clear(&self.board, candidate) => {
                        self.ships[i].pose = Some(candidate);
                        self.after_reposition_cards(content, i, events);
                        if daredevil {
                            events.push(format!(
                                "{}: Daredevil — red turn 1, stress",
                                self.label(content, i)
                            ));
                            self.gain_stress(content, i, events);
                        }
                        ActionResult::Performed
                    }
                    _ => ActionResult::Failed,
                }
            }
            PlannedAction::TargetLock(target) => {
                // Captain Kagi: an enemy in lock range must be locked
                // instead of anyone else.
                let my_corners = rules::footprint_corners(pose, fp);
                let kagi = (0..self.ships.len()).find(|&k| {
                    let s = &self.ships[k];
                    !self.allied(s.owner, self.ships[i].owner)
                        && !s.destroyed
                        && self.ability(content, s) == Some(PilotAbility::EnemyLocksMustTargetMe)
                        && s.pose.is_some_and(|q| {
                            let theirs =
                                rules::footprint_corners(q, self.class_of(content, s).footprint);
                            combat::base_distance(&my_corners, &theirs)
                                <= 3.0 * combat::RANGE_BAND_UNITS
                        })
                });
                let target = match kagi {
                    Some(k) if self.ships[k].id != target => {
                        events.push(format!(
                            "{}: Captain Kagi draws the target lock",
                            self.label(content, i)
                        ));
                        self.ships[k].id
                    }
                    _ => target,
                };
                // ST-321 locks anywhere; Long-Range Scanners only beyond
                // Range 2 (and at any distance past it).
                let anywhere = self.has_effect(content, i, UpgradeEffect::TitleLockAnywhere);
                let scanners = self.has_effect(content, i, UpgradeEffect::LocksOnlyAtRange3);
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
                        let my = rules::footprint_corners(pose, fp);
                        let d = combat::base_distance(&my, &rules::footprint_corners(tp, tfp));
                        let ok = anywhere
                            || if scanners {
                                d > 2.0 * combat::RANGE_BAND_UNITS
                            } else {
                                d <= 3.0 * combat::RANGE_BAND_UNITS
                            };
                        ok.then_some(())
                    })
                    .is_some();
                if in_range {
                    let two = self.two_locks(content, i);
                    self.ships[i].take_lock(target, two);
                    if two {
                        self.weapons_engineer_second_lock(content, i, target, events);
                    }
                    // Dutch Vander: a friend at Range 1-2 locks too.
                    if self.ability(content, &self.ships[i])
                        == Some(PilotAbility::FriendlyLockAfterLock)
                    {
                        for f in self.friends_within(content, i, 2) {
                            if self.auto_lock(content, f, "Dutch Vander", events) {
                                break;
                            }
                        }
                    }
                    ActionResult::Performed
                } else {
                    let who = self.label(content, i);
                    let whom = self
                        .ship_index(target)
                        .map(|t| self.label(content, t))
                        .unwrap_or_else(|_| "target".into());
                    events.push(format!(
                        "{who}: target lock on {whom} FAILED — not within Range 1-3 after moving"
                    ));
                    ActionResult::Failed
                }
            }
            PlannedAction::DropMine(card) => match self.bomb_kind(content, i, card) {
                Some(kind) if kind.is_mine() => {
                    let tokens = self.drop_bomb(content, i, card, kind, events);
                    return (ActionResult::Performed, tokens);
                }
                _ => ActionResult::Failed,
            },
            PlannedAction::CardAction(card) => {
                if !self.action_extras(content, &self.ships[i]).card_actions.contains(&card) {
                    return (ActionResult::Failed, Vec::new());
                }
                let name =
                    content.upgrades.upgrade(card).map(|c| c.name.clone()).unwrap_or_default();
                self.ships[i].card_actions.push(card);
                events.push(format!("{}: {name} action", self.label(content, i)));
                match content.upgrades.upgrade(card).and_then(|c| c.effect) {
                    // Rage: a focus token now and two stress tokens.
                    Some(UpgradeEffect::RerollUpTo3ForFocusAnd2Stress) => {
                        self.ships[i].focus += 1;
                        self.gain_stress(content, i, events);
                        self.gain_stress(content, i, events);
                    }
                    // Fleet Officer: up to two friendly ships within
                    // Range 1-2 (this one first) gain a focus; then stress.
                    Some(UpgradeEffect::CrewFleetOfficerAction) => {
                        let mut picks = vec![i];
                        picks.extend(self.friends_within(content, i, 2));
                        for &f in picks.iter().take(2) {
                            self.ships[f].focus += 1;
                            events.push(format!(
                                "{}: Fleet Officer — focus token to {}",
                                self.label(content, i),
                                self.label(content, f)
                            ));
                        }
                        self.gain_stress(content, i, events);
                    }
                    // Squad Leader: a lower-skill friend at Range 1-2 takes
                    // a free focus action.
                    Some(UpgradeEffect::FreeActionForLowerSkillShip) => {
                        let mine = self.effective_skill(content, &self.ships[i]);
                        let pick = self
                            .friends_within(content, i, 2)
                            .into_iter()
                            .find(|&f| self.effective_skill(content, &self.ships[f]) < mine);
                        match pick {
                            Some(f) if self.may_act_freely(content, f) => {
                                self.ships[f].focus += 1;
                                events.push(format!(
                                    "{}: Squad Leader — {} takes a free focus action",
                                    self.label(content, i),
                                    self.label(content, f)
                                ));
                            }
                            _ => {
                                events.push(format!(
                                    "{}: Squad Leader — no eligible friend",
                                    self.label(content, i)
                                ));
                                return (ActionResult::Failed, Vec::new());
                            }
                        }
                    }
                    // R7-T1: a lock on an enemy at Range 1-2 whose arc this
                    // ship sits in, then a free straight boost.
                    Some(UpgradeEffect::LockAndBoostAction) => {
                        let enemy = (0..self.ships.len()).find(|&e| {
                            !self.allied(self.ships[e].owner, self.ships[i].owner)
                                && !self.ships[e].destroyed
                                && matches!(self.range_between(content, i, e), Some(1 | 2))
                        });
                        let Some(e) = enemy else {
                            events.push(format!(
                                "{}: R7-T1 — no enemy at Range 1-2",
                                self.label(content, i)
                            ));
                            return (ActionResult::Failed, Vec::new());
                        };
                        if self.ship_in_front_arc(content, e, i) {
                            let two = self.two_locks(content, i);
                            let eid = self.ships[e].id;
                            self.ships[i].take_lock(eid, two);
                            events.push(format!(
                                "{}: R7-T1 — locks {}",
                                self.label(content, i),
                                self.label(content, e)
                            ));
                        }
                        if let Ok(candidate) = maneuver::apply(
                            pose,
                            action::boost_maneuver(action::BoostDir::Straight),
                        ) && clear(&self.board, candidate)
                        {
                            self.ships[i].pose = Some(candidate);
                            events.push(format!("{}: R7-T1 — free boost", self.label(content, i)));
                        }
                    }
                    _ => {}
                }
                ActionResult::Performed
            }
            // Resolved by `fire_seismic_torpedo` from the main action step
            // (it needs dice); as a second action it simply fails.
            PlannedAction::CardActionAt(..) => ActionResult::Failed,
        };
        (result, Vec::new())
    }

    /// Navigator (same bearing, any speed; no red while stressed) and Stay
    /// on Target (same speed, any bearing, flown as red): the replacement
    /// maneuver when `man` would leave the board or end on another ship.
    fn dial_rotation(
        &self,
        content: &Content,
        i: usize,
        dial: &[Maneuver],
        man: &Maneuver,
        others: &[[Vec2; 4]],
    ) -> Option<(Maneuver, &'static str)> {
        let navigator = self.has_effect(content, i, UpgradeEffect::CrewRotateDialSameBearing);
        let stay = self.has_effect(content, i, UpgradeEffect::RotateDialSameSpeedRed);
        let ability = self.ability(content, &self.ships[i]);
        let juno = ability == Some(PilotAbility::AdjustManeuverSpeedBy1);
        let tetran =
            ability == Some(PilotAbility::KTurnSpeed1Or3Or5) && man.steer == maneuver::Steer::KTurn;
        if !navigator && !stay && !juno && !tetran {
            return None;
        }
        let start = self.ships[i].pose?;
        let fp = self.class_of(content, &self.ships[i]).footprint;
        let bad = |m: &Maneuver| {
            maneuver::sample_path(start, *m)
                .ok()
                .map(|p| {
                    let c = rules::footprint_corners(*p.last().expect("non-empty"), fp);
                    !rules::within_board(&self.board, &c)
                        || others.iter().any(|o| rules::obbs_overlap(&c, o))
                })
                .unwrap_or(true)
        };
        if !bad(man) {
            return None;
        }
        let stressed = self.ships[i].stress > 0;
        // Juno Eclipse: the same bearing one speed slower or faster (the
        // dial's entry when it has one). Tetran Cowall: a Koiogran turn
        // at speed 1, 3 or 5, nearest to the planned speed first.
        let alternatives: Vec<(u8, &'static str)> = if juno {
            vec![
                (man.distance.saturating_sub(1).max(1), "Juno Eclipse"),
                (man.distance + 1, "Juno Eclipse"),
            ]
        } else if tetran {
            let mut v: Vec<(u8, &'static str)> =
                [1u8, 3, 5].into_iter().map(|d| (d, "Tetran Cowall")).collect();
            v.sort_by_key(|(d, _)| d.abs_diff(man.distance));
            v
        } else {
            Vec::new()
        };
        for (d, who) in alternatives {
            if d == man.distance {
                continue;
            }
            let m =
                dial.iter().find(|m| m.steer == man.steer && m.distance == d).copied().unwrap_or(
                    Maneuver { steer: man.steer, distance: d, difficulty: man.difficulty },
                );
            if maneuver::segments(m).is_ok() && !bad(&m) {
                return Some((m, who));
            }
        }
        if navigator {
            let mut same: Vec<&Maneuver> = dial
                .iter()
                .filter(|m| m.steer == man.steer && m.distance != man.distance)
                .filter(|m| {
                    !(stressed && self.maneuver_difficulty(content, i, m).0 == Difficulty::Hard)
                })
                .collect();
            same.sort_by_key(|m| m.distance.abs_diff(man.distance));
            if let Some(m) = same.into_iter().find(|m| !bad(m)) {
                return Some((*m, "Navigator"));
            }
        }
        if stay && !stressed {
            let pick = dial
                .iter()
                .filter(|m| m.distance == man.distance && m.steer != man.steer)
                .find(|m| !bad(m));
            if let Some(m) = pick {
                return Some((Maneuver { difficulty: Difficulty::Hard, ..*m }, "Stay on Target"));
            }
        }
        None
    }

    /// Card actions that roll dice: Lando Calrissian, Saboteur, R5-D8.
    fn needs_dice(content: &Content, card: UpgradeId) -> bool {
        matches!(
            content.upgrades.upgrade(card).and_then(|c| c.effect),
            Some(
                UpgradeEffect::CrewRollDefenseForTokensAction
                    | UpgradeEffect::CrewSaboteurAction
                    | UpgradeEffect::DiscardFacedownOnDefenseDie
            )
        )
    }

    /// Facedown Damage cards on ship `i`: hull lost beyond the faceup ones.
    fn facedown_cards(&self, content: &Content, i: usize) -> u8 {
        let s = &self.ships[i];
        (self.max_hull(content, s) - s.hull).saturating_sub(s.crits.len() as u8)
    }

    fn dice_card_action(
        &mut self,
        content: &Content,
        i: usize,
        card: UpgradeId,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> ActionResult {
        if !self.action_extras(content, &self.ships[i]).card_actions.contains(&card) {
            return ActionResult::Failed;
        }
        let who = self.label(content, i);
        match content.upgrades.upgrade(card).and_then(|c| c.effect) {
            // Lando: two defense dice, a token per focus or evade result.
            Some(UpgradeEffect::CrewRollDefenseForTokensAction) => {
                let (mut f, mut e) = (0, 0);
                for _ in 0..2 {
                    match DefenseFace::from_d8(roll()) {
                        DefenseFace::Focus => f += 1,
                        DefenseFace::Evade => e += 1,
                        DefenseFace::Blank => {}
                    }
                }
                self.ships[i].focus += f;
                self.ships[i].evade += e;
                events.push(format!("{who}: Lando Calrissian — {f} focus, {e} evade tokens"));
                ActionResult::Performed
            }
            // Saboteur: an enemy at Range 1 with facedown cards; a hit or
            // critical turns one faceup.
            Some(UpgradeEffect::CrewSaboteurAction) => {
                let target = (0..self.ships.len()).find(|&e| {
                    !self.allied(self.ships[e].owner, self.ships[i].owner)
                        && !self.ships[e].destroyed
                        && self.facedown_cards(content, e) > 0
                        && self.range_between(content, i, e) == Some(1)
                });
                let Some(e) = target else {
                    events.push(format!("{who}: Saboteur — no damaged enemy at Range 1"));
                    return ActionResult::Failed;
                };
                match AttackFace::from_d8(roll()) {
                    AttackFace::Hit | AttackFace::Crit => {
                        let effect = crit::draw(roll());
                        events.push(format!(
                            "{who}: Saboteur — {}'s facedown card turns faceup: {}",
                            self.label(content, e),
                            effect.name()
                        ));
                        self.apply_crit_effect(content, e, effect, roll, events);
                    }
                    _ => events.push(format!("{who}: Saboteur — the sabotage fails")),
                }
                ActionResult::Performed
            }
            // R5-D8: a defense die; evade or focus repairs a facedown card.
            Some(UpgradeEffect::DiscardFacedownOnDefenseDie) => {
                if self.facedown_cards(content, i) == 0 {
                    events.push(format!("{who}: R5-D8 — nothing to repair"));
                    return ActionResult::Failed;
                }
                match DefenseFace::from_d8(roll()) {
                    DefenseFace::Blank => events.push(format!("{who}: R5-D8 — repair fails")),
                    _ => {
                        self.ships[i].hull += 1;
                        events.push(format!("{who}: R5-D8 — a facedown damage card discarded"));
                    }
                }
                ActionResult::Performed
            }
            _ => ActionResult::Failed,
        }
    }

    /// Seismic Torpedo: discard the card to blast obstacle `obstacle`,
    /// which must be at Range 1-2 and inside the primary firing arc after
    /// moving. Each ship at Range 1 of it (this one included) rolls one
    /// attack die and suffers the damage; then the obstacle is removed.
    fn fire_seismic_torpedo(
        &mut self,
        content: &Content,
        i: usize,
        card: UpgradeId,
        obstacle: u32,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> (ActionResult, Option<SeismicBlast>) {
        let label = self.label(content, i);
        if !self.action_extras(content, &self.ships[i]).card_actions.contains(&card) {
            return (ActionResult::Failed, None);
        }
        let Some(target) = self.obstacles.iter().find(|o| o.id == obstacle).cloned() else {
            events.push(format!("{label}: Seismic Torpedo FAILED — the obstacle is gone"));
            return (ActionResult::Failed, None);
        };
        let pose = self.ships[i].pose.expect("acting ships are on the board");
        let fp = self.class_of(content, &self.ships[i]).footprint;
        let poly = target.polygon();
        let base = rules::footprint_corners(pose, fp);
        let in_range = obstacle::polygon_distance(&base, &poly) <= 2.0 * combat::RANGE_BAND_UNITS;
        let in_arc = poly
            .iter()
            .chain(std::iter::once(&target.center))
            .any(|p| combat::in_front_arc(pose, fp, *p));
        if !in_range || !in_arc {
            events.push(format!(
                "{label}: Seismic Torpedo FAILED — the {} is not at Range 1-2 inside the firing arc",
                target.kind.name()
            ));
            return (ActionResult::Failed, None);
        }
        if !self.spend_ordnance(content, i, card, events) {
            let why = format!("fired at the {}", target.kind.name());
            let line = self.discard_card(content, i, card, &why);
            events.push(line);
        }
        let victims: Vec<usize> = (0..self.ships.len())
            .filter(|&v| {
                let s = &self.ships[v];
                !s.destroyed
                    && s.pose.is_some_and(|p| {
                        let corners =
                            rules::footprint_corners(p, self.class_of(content, s).footprint);
                        obstacle::polygon_distance(&corners, &poly) <= combat::RANGE_BAND_UNITS
                    })
            })
            .collect();
        // The blast is recorded as a detonation centred on the obstacle;
        // the token id doubles as the obstacle id for the client.
        let token = BombToken {
            id: obstacle,
            kind: BombKind::SeismicTorpedo,
            card,
            pose: Pose { anchor: target.center + Vec2::new(0.5, 0.0), heading: 0.0 },
            owner: self.ships[i].owner,
        };
        let detonation = self.detonate(content, token, &victims, roll, events);
        self.obstacles.retain(|o| o.id != obstacle);
        events.push(format!("The {} breaks up and is removed", target.kind.name()));
        (ActionResult::Performed, Some(SeismicBlast { obstacle, detonation }))
    }

    /// Expert Handling after a barrel roll: a stress token when the bar
    /// has no barrel roll icon, then one enemy target lock on this ship
    /// is removed.
    fn after_expert_roll(&mut self, content: &Content, i: usize, events: &mut Vec<String>) {
        if self.count_effect(content, &self.ships[i], UpgradeEffect::BarrelRollActionDiscardLock)
            == 0
        {
            return;
        }
        let label = self.label(content, i);
        if !self.action_bar(content, &self.ships[i]).contains(&ActionKind::BarrelRoll) {
            events.push(format!("{label}: Expert Handling — rolled without the icon, 1 stress"));
            self.gain_stress(content, i, events);
        }
        let me = self.ships[i].id;
        let owner = self.ships[i].owner;
        if let Some(e) = (0..self.ships.len())
            .find(|&e| !self.allied(self.ships[e].owner, owner) && self.ships[e].locks_on(me))
        {
            self.ships[e].drop_lock(me);
            events.push(format!(
                "{label}: Expert Handling — {}'s target lock removed",
                self.label(content, e)
            ));
        }
    }

    /// May ship `i` perform a free action right now (not stressed, no
    /// Damaged Sensor Array, alive)?
    fn may_act_freely(&self, content: &Content, i: usize) -> bool {
        let s = &self.ships[i];
        !s.destroyed
            && (s.stress == 0
                || self.ability(content, s) == Some(PilotAbility::ActionsWhileStressed))
            && !s.crits.contains(&CritEffect::DamagedSensorArray)
    }

    /// Secretly choose a bomb card to drop when the dial is revealed
    /// (None = keep it). Only dial-reveal bombs qualify; mines are
    /// dropped through `PlannedAction::DropMine`.
    /// Cards whose effect is a per-round choice: switched on with the dial.
    fn toggle_cards(&self, content: &Content, s: &ShipState) -> Vec<UpgradeId> {
        s.upgrades
            .iter()
            .copied()
            .filter(|u| {
                matches!(
                    content.upgrades.upgrade(*u).and_then(|c| c.effect),
                    Some(
                        UpgradeEffect::RotateShip180Discard
                            | UpgradeEffect::SystemDamageToDiscardToken
                            | UpgradeEffect::CrewEvadeInsteadOfFocusForFriendly
                            | UpgradeEffect::SwapSkillWithFriendly
                    )
                )
            })
            .collect()
    }

    /// Secretly switch one of the ship's toggle cards on or off for this
    /// round (see `toggle_cards`).
    pub fn plan_card_use(
        &mut self,
        content: &Content,
        player: PlayerId,
        ship_id: ShipId,
        card: UpgradeId,
        on: bool,
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
        if !self.toggle_cards(content, &self.ships[i]).contains(&card) {
            return Err(Rejection::NoSuchUpgrade);
        }
        self.ships[i].card_uses.retain(|c| *c != card);
        if on {
            self.ships[i].card_uses.push(card);
        }
        Ok(())
    }

    /// Is `card`'s effect switched on for this round?
    fn using(&self, content: &Content, i: usize, effect: UpgradeEffect) -> Option<UpgradeId> {
        self.ships[i]
            .card_uses
            .iter()
            .copied()
            .find(|u| content.upgrades.upgrade(*u).and_then(|c| c.effect) == Some(effect))
    }

    pub fn plan_bomb(
        &mut self,
        content: &Content,
        player: PlayerId,
        ship_id: ShipId,
        bomb: Option<UpgradeId>,
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
        if let Some(card) = bomb
            && !self.bomb_kind(content, i, card).is_some_and(|k| {
                // Deathfire drops mines on the reveal too.
                !k.is_mine()
                    || self.ability(content, &self.ships[i])
                        == Some(PilotAbility::FreeBombActionOnRevealOrAction)
            })
        {
            return Err(Rejection::NoSuchUpgrade);
        }
        self.ships[i].bomb = bomb;
        Ok(())
    }

    /// The bomb kind of `card` if ship `i` carries it and it is a bomb.
    fn bomb_kind(&self, content: &Content, i: usize, card: UpgradeId) -> Option<BombKind> {
        if !self.ships[i].upgrades.contains(&card) {
            return None;
        }
        content.upgrades.upgrade(card).and_then(|u| u.effect).and_then(BombKind::from_effect)
    }

    /// Discard `card` from ship `i` and place its token(s) one template
    /// behind the ship's current pose.
    fn drop_bomb(
        &mut self,
        content: &Content,
        i: usize,
        card: UpgradeId,
        kind: BombKind,
        events: &mut Vec<String>,
    ) -> Vec<BombToken> {
        let pose = self.ships[i].pose.expect("dropping ships are on the board");
        let fp = self.class_of(content, &self.ships[i]).footprint;
        let owner = self.ships[i].owner;
        if !self.spend_ordnance(content, i, card, events) {
            self.ships[i].upgrades.retain(|u| *u != card);
        }
        let name = content.upgrades.upgrade(card).map(|u| u.name.clone()).unwrap_or_default();
        events.push(format!("{}: drops {name}", self.label(content, i)));
        let tokens: Vec<BombToken> = bombs::drop_poses(kind, pose, fp)
            .into_iter()
            .map(|pose| {
                let id = self.next_bomb_id;
                self.next_bomb_id += 1;
                BombToken { id, kind, card, pose, owner }
            })
            .collect();
        self.bombs.extend(tokens.iter().copied());
        tokens
    }

    /// Deal one faceup Damage card: a hull point plus a drawn critical.
    fn faceup_card(
        &mut self,
        content: &Content,
        i: usize,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        if self.hull_point(i) == DamagePoint::Hull && !self.ships[i].destroyed {
            let effect = crit::draw(roll());
            events.push(format!("{}: critical — {}", self.label(content, i), effect.name()));
            self.apply_crit_effect(content, i, effect, roll, events);
        }
    }

    /// A token goes off against the ships at indices `victims`.
    fn detonate(
        &mut self,
        content: &Content,
        token: BombToken,
        victims: &[usize],
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> Detonation {
        events.push(format!("{} detonates", token.kind.name()));
        let mut hits = Vec::new();
        for &i in victims {
            let mut hit = BombHit {
                ship: self.ships[i].id,
                damage: 0,
                crits: 0,
                ion: 0,
                stress: 0,
                destroyed: false,
            };
            let mut dice = 0;
            let shields_before = self.ships[i].shields;
            match token.kind {
                BombKind::Proton => {
                    self.faceup_card(content, i, roll, events);
                    hit.crits = 1;
                }
                BombKind::Seismic => {
                    self.damage_point(i);
                    hit.damage = 1;
                }
                BombKind::Ion => {
                    self.ships[i].ion += 2;
                    hit.ion = 2;
                }
                BombKind::Thermal => {
                    self.damage_point(i);
                    hit.damage = 1;
                    if !self.ships[i].destroyed {
                        self.gain_stress(content, i, events);
                        hit.stress = 1;
                    }
                }
                BombKind::ProximityMine => dice = 3,
                BombKind::ClusterMine => dice = 2,
                BombKind::SeismicTorpedo => dice = 1,
                BombKind::ConnerNet => {
                    self.damage_point(i);
                    self.ships[i].ion += 2;
                    hit.damage = 1;
                    hit.ion = 2;
                }
            }
            // Mines: suffer every hit and critical hit rolled (shields
            // absorb criticals like any other damage).
            for _ in 0..dice {
                if self.ships[i].destroyed {
                    break;
                }
                match AttackFace::from_d8(roll()) {
                    AttackFace::Hit => {
                        self.damage_point(i);
                        hit.damage += 1;
                    }
                    AttackFace::Crit => {
                        hit.damage += 1;
                        if self.damage_point(i) == DamagePoint::Hull && !self.ships[i].destroyed {
                            hit.crits += 1;
                            let effect = crit::draw(roll());
                            events.push(format!(
                                "{}: critical — {}",
                                self.label(content, i),
                                effect.name()
                            ));
                            self.apply_crit_effect(content, i, effect, roll, events);
                        }
                    }
                    _ => {}
                }
            }
            hit.destroyed = self.ships[i].destroyed;
            if self.ships[i].shields < shields_before && !hit.destroyed {
                self.after_shield_loss(content, i, events);
            }
            let mut what = Vec::new();
            if hit.damage > 0 {
                what.push(format!("{} damage", hit.damage));
            }
            if token.kind == BombKind::Proton {
                what.push("faceup damage card".to_string());
            }
            if hit.ion > 0 {
                what.push(format!("{} ion", hit.ion));
            }
            if hit.stress > 0 {
                what.push("stressed".to_string());
            }
            if what.is_empty() {
                what.push("no damage".to_string());
            }
            let died = if hit.destroyed { " — DESTROYED" } else { "" };
            events.push(format!(
                "{}: caught by the {} — {}{died}",
                self.label(content, i),
                token.kind.name(),
                what.join(", ")
            ));
            hits.push(hit);
        }
        Detonation { token, hits }
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
        if self.committed.iter().any(|c| !c) {
            return Ok(None);
        }
        let (moves, pulls, detonations, mut events) = self.resolve_movement(content, roll);
        self.combat_start_stress_relief(content, &mut events);

        // Combat order: highest pilot skill first (initiative breaks
        // ties), grouped by skill; each group's survivors are fixed when
        // the group starts (the simultaneous-attack rule).
        let mut skills: Vec<u8> =
            self.ships.iter().map(|s| self.effective_skill(content, s)).collect();
        // Swarm Tactics: the lowest-skill friend at Range 1 fires at the
        // leader's skill this phase.
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed
                || self.ships[i].pose.is_none()
                || !self.has_effect(content, i, UpgradeEffect::ShareSkillWithFriendly)
            {
                continue;
            }
            let mine = self.effective_skill(content, &self.ships[i]);
            if let Some(f) = self
                .friends_at_range1(content, i)
                .into_iter()
                .filter(|&f| skills[f] < mine)
                .min_by_key(|&f| skills[f])
            {
                skills[f] = mine;
                events.push(format!(
                    "{}: Swarm Tactics — {} fires at pilot skill {mine}",
                    self.label(content, i),
                    self.label(content, f)
                ));
            }
        }
        // Decoy (switched on): swap pilot skill with a friend at Range 1-2
        // for the phase — policy: the friend with the highest skill.
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed
                || self.ships[i].pose.is_none()
                || self.using(content, i, UpgradeEffect::SwapSkillWithFriendly).is_none()
            {
                continue;
            }
            if let Some(f) =
                self.friends_within(content, i, 2).into_iter().max_by_key(|&f| skills[f])
            {
                skills.swap(i, f);
                events.push(format!(
                    "{}: Decoy — swaps pilot skill with {} ({} / {})",
                    self.label(content, i),
                    self.label(content, f),
                    skills[i],
                    skills[f]
                ));
            }
        }
        let combatants: Vec<(ShipId, u8, PlayerId)> = self
            .ships
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.destroyed && s.pose.is_some())
            .map(|(k, s)| (s.id, skills[k], s.owner))
            .collect();
        let skill_of = |id: ShipId| {
            combatants.iter().find(|(s, _, _)| *s == id).map(|(_, k, _)| *k).unwrap_or(0)
        };
        let mut groups: Vec<Vec<ShipId>> = Vec::new();
        let ranks = self.seat_ranks();
        for id in combat_order(&combatants, &ranks) {
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
            followup: None,
            attacks: Vec::new(),
            events: events.clone(),
            moves: moves.clone(),
            pulls: pulls.clone(),
            detonations: detonations.clone(),
        });
        Ok(Some(ActivationRecords { moves, pulls, detonations, events }))
    }

    /// Start of the Combat phase: "Epsilon Leader" removes a stress token
    /// from every friendly ship at Range 1; a Wingman card from one.
    fn combat_start_stress_relief(&mut self, content: &Content, events: &mut Vec<String>) {
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed || self.ships[i].pose.is_none() {
                continue;
            }
            // Commander Alozen: a lock on an enemy at Range 1.
            if self.ability(content, &self.ships[i])
                == Some(PilotAbility::LockAtRange1AtCombatStart)
            {
                self.auto_lock_within(content, i, 1, "Commander Alozen", events);
            }
            // Colonel Jendon: his lock goes to a friend at Range 1 without one.
            if self.ability(content, &self.ships[i])
                == Some(PilotAbility::GiveLockToFriendlyAtCombatStart)
                && let Some(l) = self.ships[i].lock
                && let Some(f) = self
                    .friends_at_range1(content, i)
                    .into_iter()
                    .find(|&f| self.ships[f].lock_free(self.two_locks(content, f)))
            {
                let two = self.two_locks(content, f);
                self.ships[f].take_lock(l, two);
                self.ships[i].drop_lock(l);
                events.push(format!(
                    "{}: Colonel Jendon — hands his target lock to {}",
                    self.label(content, i),
                    self.label(content, f)
                ));
            }
            // Rey: a stored focus token comes back for the Combat phase.
            if self.ships[i].stored_focus > 0
                && self.has_effect(content, i, UpgradeEffect::CrewStoreFocusTokens)
            {
                self.ships[i].stored_focus -= 1;
                self.ships[i].focus += 1;
                events
                    .push(format!("{}: Rey — stored focus token assigned", self.label(content, i)));
            }
            // Ysanne Isard: shieldless and damaged, a free evade action.
            if self.ships[i].shields == 0
                && self.ships[i].hull < self.max_hull(content, &self.ships[i])
                && self.has_effect(content, i, UpgradeEffect::CrewFreeEvadeIfNoShieldsDamaged)
                && self.may_act_freely(content, i)
            {
                self.ships[i].evade += 1;
                events
                    .push(format!("{}: Ysanne Isard — free evade action", self.label(content, i)));
            }
            let friends = self.friends_at_range1(content, i);
            if self.ability(content, &self.ships[i])
                == Some(PilotAbility::RemoveStressFriendlyRange1AtCombatStart)
            {
                for &f in &friends {
                    if self.lose_stress(content, f, events) {
                        events.push(format!(
                            "{}: ability — stress removed from {}",
                            self.label(content, i),
                            self.label(content, f)
                        ));
                    }
                }
            }
            if self.has_effect(content, i, UpgradeEffect::RemoveStressFriendlyAtCombatStart)
                && let Some(&f) = friends.iter().find(|&&f| self.ships[f].stress > 0)
                && self.lose_stress(content, f, events)
            {
                events.push(format!(
                    "{}: Wingman — stress removed from {}",
                    self.label(content, i),
                    self.label(content, f)
                ));
            }
        }
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
            if let Some((a_idx, shot)) = cs.followup.take() {
                if self.ships[a_idx].destroyed || self.ships[shot.d_idx].destroyed {
                    continue;
                }
                return Ok(CombatStep::Attack(self.fire(content, a_idx, shot, roll)));
            }
            if cs.current.is_empty() {
                if cs.groups.is_empty() {
                    let mut cs = self.combat.take().expect("checked above");
                    self.finish_turn(content, roll, &mut cs.events);
                    return Ok(CombatStep::Done(TurnRecords {
                        moves: cs.moves,
                        pulls: cs.pulls,
                        detonations: cs.detonations,
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
            // Mission 3: scanning a satellite replaces the attack.
            if let Some(msg) = self.mission_scan(content, a_idx) {
                self.combat.as_mut().expect("in combat").events.push(msg);
                continue;
            }
            let options = self.attack_options(content, a_idx);
            match options.len() {
                0 => continue,
                1 => {
                    let o = options[0].clone();
                    let d_idx = self.ships.iter().position(|s| s.id == o.target).expect("option");
                    let shot = Shot {
                        d_idx,
                        range: o.range,
                        weapon: o.weapon,
                        second: false,
                        focus_hit: false,
                        no_mods: false,
                    };
                    return Ok(CombatStep::Attack(self.fire(content, a_idx, shot, roll)));
                }
                _ => {
                    let views = self.snapshot_for(content, owner);
                    let unavailable = views
                        .iter()
                        .find(|v| v.id == attacker)
                        .map(|me| {
                            crate::weapons::unavailable_reasons(
                                content,
                                &views,
                                &self.obstacles,
                                me,
                            )
                        })
                        .unwrap_or_default();
                    let p = PendingAttack { attacker, owner, options, unavailable };
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
        self.combat.as_mut().expect("checked above").pending = None;
        let shot = Shot { d_idx, range, weapon, second: false, focus_hit: false, no_mods: false };
        Ok(self.fire(content, a_idx, shot, roll))
    }

    /// Resolve a shot inside the Combat phase: record it, and queue the
    /// repeat for "attack twice" weapons (Cluster Missiles, Twin Laser
    /// Turret).
    fn fire(
        &mut self,
        content: &Content,
        a_idx: usize,
        shot: Shot,
        roll: &mut dyn FnMut() -> u8,
    ) -> AttackRecord {
        let mut ev = Vec::new();
        let rec = self.perform_attack_on(content, a_idx, shot, roll, &mut ev);
        let twice = matches!(
            shot.weapon.and_then(|u| content.upgrades.upgrade(u)).and_then(|c| c.effect),
            Some(UpgradeEffect::MissileAttackTwice | UpgradeEffect::TurretTwinLaserTwiceOneDamage)
        );
        let missed = rec.hits + rec.crits == 0;
        {
            let cs = self.combat.as_mut().expect("in combat");
            cs.events.extend(ev);
            cs.attacks.push(rec.clone());
            if twice && !shot.second {
                cs.followup = Some((a_idx, Shot { second: true, ..shot }));
                return rec;
            }
        }
        // Gunner / Luke Skywalker (crew): a miss is followed by a primary
        // weapon attack — at the same ship when it is still a legal
        // target, else the nearest — once per round, and never after a
        // second attack already taken.
        if missed && !self.ships[a_idx].destroyed && !shot.focus_hit {
            let cards =
                [UpgradeEffect::CrewSecondAttackOnMiss, UpgradeEffect::CrewSecondAttackFocusToHit];
            let already = cards.iter().any(|e| {
                self.ships[a_idx]
                    .used_round
                    .iter()
                    .any(|u| content.upgrades.upgrade(*u).and_then(|c| c.effect) == Some(*e))
            });
            let luke = if already {
                None
            } else if self.use_once(content, a_idx, cards[0]).is_some() {
                Some(false)
            } else {
                self.use_once(content, a_idx, cards[1]).map(|_| true)
            };
            if let Some(luke) = luke {
                let defender = self.ships[shot.d_idx].id;
                let options = self.attack_options(content, a_idx);
                let primary: Vec<&AttackOption> =
                    options.iter().filter(|o| o.weapon.is_none()).collect();
                let pick =
                    primary.iter().find(|o| o.target == defender).copied().or_else(|| {
                        primary.iter().copied().min_by(|a, b| a.dist.total_cmp(&b.dist))
                    });
                if let Some(o) = pick {
                    let d_idx = self.ships.iter().position(|s| s.id == o.target).expect("option");
                    let name = if luke { "Luke Skywalker" } else { "Gunner" };
                    let cs = self.combat.as_mut().expect("in combat");
                    cs.events.push(format!(
                        "{}: {name} — a second attack after the miss",
                        self.ships[a_idx].callsign
                    ));
                    cs.followup = Some((
                        a_idx,
                        Shot {
                            d_idx,
                            range: o.range,
                            weapon: None,
                            second: false,
                            focus_hit: luke,
                            no_mods: false,
                        },
                    ));
                }
            }
        }
        // BTL-A4 Y-Wing: a primary attack is followed by a turret attack
        // (once per round) — at the same ship when legal, else the
        // nearest turret target.
        if shot.weapon.is_none()
            && !self.ships[a_idx].destroyed
            && self.combat.as_ref().is_some_and(|cs| cs.followup.is_none())
            && self.use_once(content, a_idx, UpgradeEffect::TitleArcOnlyThenTurretAttack).is_some()
        {
            let defender = self.ships[shot.d_idx].id;
            let options = self.attack_options(content, a_idx);
            let turrets: Vec<&AttackOption> = options
                .iter()
                .filter(|o| {
                    o.weapon.is_some_and(|w| {
                        content.upgrades.upgrade(w).is_some_and(|c| c.slot == Slot::Turret)
                    })
                })
                .collect();
            let pick = turrets
                .iter()
                .find(|o| o.target == defender)
                .copied()
                .or_else(|| turrets.iter().copied().min_by(|a, b| a.dist.total_cmp(&b.dist)));
            if let Some(o) = pick {
                let d_idx = self.ships.iter().position(|s| s.id == o.target).expect("option");
                let cs = self.combat.as_mut().expect("in combat");
                cs.events.push(format!(
                    "{}: BTL-A4 Y-Wing — turret attack after the primary weapon",
                    self.ships[a_idx].callsign
                ));
                cs.followup = Some((
                    a_idx,
                    Shot {
                        d_idx,
                        range: o.range,
                        weapon: o.weapon,
                        second: false,
                        focus_hit: false,
                        no_mods: false,
                    },
                ));
            }
        }
        // Chewbacca (Heroes of the Resistance): a friend destroyed at
        // Range 1-3 of him lets him attack at once.
        if rec.defender_destroyed && self.combat.as_ref().is_some_and(|cs| cs.followup.is_none()) {
            let d_idx = shot.d_idx;
            let d_owner = self.ships[d_idx].owner;
            let chewie = (0..self.ships.len()).find(|&k| {
                k != d_idx
                    && self.allied(self.ships[k].owner, d_owner)
                    && !self.ships[k].destroyed
                    && self.ability(content, &self.ships[k])
                        == Some(PilotAbility::AttackWhenFriendlyDestroyed)
                    && self.range_between(content, k, d_idx).is_some()
            });
            if let Some(k) = chewie {
                let options = self.attack_options(content, k);
                let lock = self.ships[k].lock;
                let primary: Vec<&AttackOption> =
                    options.iter().filter(|o| o.weapon.is_none()).collect();
                let pick =
                    primary.iter().find(|o| Some(o.target) == lock).copied().or_else(|| {
                        primary.iter().copied().min_by(|a, b| a.dist.total_cmp(&b.dist))
                    });
                if let Some(o) = pick {
                    let t = self.ships.iter().position(|s| s.id == o.target).expect("option");
                    let cs = self.combat.as_mut().expect("in combat");
                    cs.events.push(format!(
                        "{}: Chewbacca — attacks after a friend's destruction",
                        self.ships[k].callsign
                    ));
                    cs.followup = Some((
                        k,
                        Shot {
                            d_idx: t,
                            range: o.range,
                            weapon: None,
                            second: false,
                            focus_hit: false,
                            no_mods: false,
                        },
                    ));
                }
            }
        }
        rec
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
    ) -> (Vec<MoveRecord>, Vec<Pull>, Vec<Detonation>, Vec<String>) {
        // Enhanced Scopes: pilot skill 0 for the Activation phase.
        let order = movement_order(
            &self
                .ships
                .iter()
                .filter(|s| !s.destroyed && s.pose.is_some())
                .map(|s| {
                    let scopes =
                        self.count_effect(content, s, UpgradeEffect::SystemSkillZeroInActivation)
                            > 0;
                    (s.id, if scopes { 0 } else { self.effective_skill(content, s) }, s.owner)
                })
                .collect::<Vec<_>>(),
            &self.seat_ranks(),
        );
        let mut records = Vec::new();
        let mut events: Vec<String> = Vec::new();
        for s in self.ships.iter_mut() {
            s.snap_shot_fired = false;
        }
        // Intelligence Agent (crew): at the start of the Activation phase
        // the nearest enemy dial at Range 1-2 is read out (flavour only —
        // plans are already fixed).
        for k in 0..self.ships.len() {
            if self.ships[k].destroyed
                || !self.has_effect(content, k, UpgradeEffect::CrewPeekEnemyDial)
            {
                continue;
            }
            let target = (0..self.ships.len())
                .filter(|&e| {
                    !self.ships[e].destroyed
                        && !self.allied(self.ships[e].owner, self.ships[k].owner)
                        && self.ships[e].plan.is_some()
                })
                .filter_map(|e| {
                    self.range_between(content, k, e).filter(|r| *r <= 2).map(|r| (r, e))
                })
                .min();
            if let Some((_, e)) = target {
                let s = &self.ships[e];
                let dial = &content
                    .dials
                    .set(self.class_of(content, s).maneuver_set)
                    .expect("dial")
                    .maneuvers;
                let man = dial[s.plan.expect("filtered") as usize];
                events.push(format!(
                    "{}: Intelligence Agent — {} has dialed {}",
                    self.label(content, k),
                    self.label(content, e),
                    man.label()
                ));
            }
        }
        // Leia Organa (crew): discarded at the start of the Activation
        // phase when a friendly ship revealed a red maneuver.
        let planned_red = |gs: &Self, j: usize| -> bool {
            let s = &gs.ships[j];
            let Some(plan) = s.plan else { return false };
            let class = gs.class_of(content, s);
            let dial = &content.dials.set(class.maneuver_set).expect("validated").maneuvers;
            gs.maneuver_difficulty(content, j, &dial[plan as usize]).0 == Difficulty::Hard
        };
        for k in 0..self.ships.len() {
            if self.ships[k].destroyed
                || self.white_reds[self.ships[k].owner.0 as usize]
                || !self.has_effect(content, k, UpgradeEffect::CrewRedAsWhiteForAll)
            {
                continue;
            }
            let owner = self.ships[k].owner;
            let any_red = (0..self.ships.len()).any(|j| {
                self.allied(self.ships[j].owner, owner)
                    && !self.ships[j].destroyed
                    && planned_red(self, j)
            });
            if any_red
                && let Some(card) =
                    self.card_with_effect(content, k, UpgradeEffect::CrewRedAsWhiteForAll)
            {
                let e =
                    self.discard_card(content, k, card, "red maneuvers flown as white this round");
                events.push(e);
                self.white_reds[owner.0 as usize] = true;
            }
        }
        for id in order {
            let i = self.ship_index(id).expect("ordered ids exist");
            let (fp, dial_id) = {
                let class = self.class_of(content, &self.ships[i]);
                (class.footprint, class.maneuver_set)
            };
            let dial = &content.dials.set(dial_id).expect("validated").maneuvers;
            let mut man = dial[self.ships[i].plan.expect("commit checked plans") as usize];
            let (mut difficulty, mut rush) = self.maneuver_difficulty(content, i, &man);
            // p.17: a ship that is ALREADY stressed when it reveals a red
            // maneuver doesn't fly it — the opposing player picks any
            // non-red replacement. Planning a red maneuver while stressed
            // is rejected, so this only fires when stress arrives during
            // another ship's activation (Captain Yorr absorbing a friend's
            // stress). Automated as the slowest white straight, an
            // adversarial stand-in for the opponent's choice — decided
            // 2026-09-11 to stay automatic while the event is this rare.
            if self.ships[i].stress > 0
                && difficulty == Difficulty::Hard
                && let Some(sub) = substitute_non_red(dial, &self.ships[i].crits)
            {
                man = sub;
                (difficulty, rush) = self.maneuver_difficulty(content, i, &man);
            }
            // Ionized ships ignore their dial: a white straight 1, then the
            // ion tokens come off.
            if self.ships[i].ion > 0 {
                man = Maneuver {
                    steer: maneuver::Steer::Straight,
                    distance: 1,
                    difficulty: Difficulty::Normal,
                };
                self.ships[i].ion = 0;
                (difficulty, rush) = (Difficulty::Normal, None);
                events.push(format!("{}: ionized — drifts straight 1", self.label(content, i)));
            }
            // Dial-reveal bombs drop before the ship moves.
            let mut dropped_before = Vec::new();
            if let Some(card) = self.ships[i].bomb.take()
                && let Some(kind) = self.bomb_kind(content, i, card)
                && (!kind.is_mine()
                    || self.ability(content, &self.ships[i])
                        == Some(PilotAbility::FreeBombActionOnRevealOrAction))
            {
                dropped_before = self.drop_bomb(content, i, card, kind, &mut events);
            }
            // Everyone else still on the board, as obstacles.
            let obstacles: Vec<_> = self
                .ships
                .iter()
                .filter(|s| s.id != id && !s.destroyed)
                .filter_map(|s| {
                    s.pose.map(|p| rules::footprint_corners(p, self.class_of(content, s).footprint))
                })
                .collect();
            // Navigator / Stay on Target: a dial that would fly off the
            // board or bump is rotated to one that would not.
            if let Some((alt, who)) = self.dial_rotation(content, i, dial, &man, &obstacles) {
                events.push(format!(
                    "{}: {who} — dial rotated to {:?} {}",
                    self.label(content, i),
                    alt.steer,
                    alt.distance
                ));
                man = alt;
                (difficulty, rush) = self.maneuver_difficulty(content, i, &man);
            }
            let extras = self.action_extras(content, &self.ships[i]);
            // Turr Phennir's reposition waits for his attack.
            let mut planned2 = if extras.second == Some(SecondActionKind::RepositionAfterAttack) {
                None
            } else {
                self.ships[i].planned_action2.take()
            };
            // BB-8: a free barrel roll on a green reveal, before moving.
            let mut pre = None;
            if extras.second == Some(SecondActionKind::RollOnGreenReveal)
                && difficulty == Difficulty::Easy
                && matches!(
                    planned2,
                    Some(PlannedAction::BarrelRoll(_) | PlannedAction::BarrelRollFar(_))
                )
                && self.may_act_freely(content, i)
            {
                let a = planned2.take().expect("matched above");
                let (r, _) = self.perform_action(content, i, a, fp, &obstacles, &mut events);
                events.push(format!(
                    "{}: BB-8 — free barrel roll before moving",
                    self.label(content, i)
                ));
                pre = Some((a, r));
            }
            // Advanced Sensors: a token or lock action is taken before the
            // maneuver instead of after it (repositions stay after).
            if pre.is_none()
                && self.has_effect(content, i, UpgradeEffect::SystemFreeActionBeforeReveal)
                && matches!(
                    self.ships[i].planned_action,
                    Some(
                        PlannedAction::Focus
                            | PlannedAction::Evade
                            | PlannedAction::TargetLock(_)
                            | PlannedAction::CardAction(_)
                    )
                )
                && self.may_act_freely(content, i)
            {
                let a = self.ships[i].planned_action.take().expect("matched above");
                let (r, _) = self.perform_action(content, i, a, fp, &obstacles, &mut events);
                events.push(format!(
                    "{}: Advanced Sensors — action before the maneuver",
                    self.label(content, i)
                ));
                pre = Some((a, r));
            }
            let start = self.ships[i].pose.expect("placed");
            let path = maneuver::sample_path(start, man).expect("validated at plan time");

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
            let mut end = used_path[stop];
            let fled = !rules::within_board(&self.board, &rules::footprint_corners(end, fp));
            let exit = mission::exit_edge(&self.board, combat::base_center(end, fp));
            let escaped = fled && self.mission_escape(i, exit);

            {
                let ship = &mut self.ships[i];
                ship.pose = Some(end);
                ship.plan = None;
                if fled {
                    ship.destroyed = true;
                    ship.escaped = escaped;
                }
            }
            if fled {
                events.push(if escaped {
                    format!("{}: ESCAPED off the {} edge", self.label(content, i), exit.name())
                } else {
                    format!(
                        "{}: fled the battlefield off the {} edge — destroyed",
                        self.label(content, i),
                        exit.name()
                    )
                });
            }
            // Obstacles (p.20): a base or template crossing an asteroid
            // costs the action and rolls a die (hit: 1 damage, crit: a
            // faceup card); ending on one forbids attacking this round.
            // Debris: a stress token, and only a crit hurts.
            let mut obstacles_hit = Vec::new();
            let mut on_rock = false;
            if !self.ships[i].destroyed {
                let crossed: Vec<Obstacle> = self
                    .obstacles
                    .iter()
                    .filter(|o| {
                        let poly = o.polygon();
                        used_path[..=stop].iter().any(|p| {
                            obstacle::convex_overlap(&rules::footprint_corners(*p, fp), &poly)
                        })
                    })
                    .copied()
                    .collect();
                // R5-X3: discarded before the reveal to ignore the obstacles
                // this maneuver would hit.
                let crossed = if !crossed.is_empty()
                    && let Some(card) =
                        self.card_with_effect(content, i, UpgradeEffect::IgnoreObstaclesDiscard)
                {
                    let e = self.discard_card(content, i, card, "obstacles ignored this round");
                    events.push(e);
                    Vec::new()
                } else {
                    crossed
                };
                let detector =
                    self.has_effect(content, i, UpgradeEffect::SystemOverlapObstaclesOnReposition);
                for o in crossed {
                    obstacles_hit.push(o.id);
                    let who = self.label(content, i);
                    let die = AttackFace::from_d8(roll());
                    // Collision Detector: critical results are ignored.
                    let die =
                        if detector && die == AttackFace::Crit { AttackFace::Blank } else { die };
                    match o.kind {
                        ObstacleKind::Asteroid => {
                            on_rock = true;
                            match die {
                                AttackFace::Hit => {
                                    self.damage_point(i);
                                    events.push(format!(
                                        "{who}: hits an asteroid — 1 damage, no action"
                                    ));
                                }
                                AttackFace::Crit => {
                                    events.push(format!(
                                        "{who}: hits an asteroid — faceup damage card, no action"
                                    ));
                                    self.faceup_card(content, i, roll, &mut events);
                                }
                                _ => events.push(format!(
                                    "{who}: hits an asteroid — no damage, no action"
                                )),
                            }
                            if obstacle::convex_overlap(
                                &rules::footprint_corners(end, fp),
                                &o.polygon(),
                            ) {
                                self.ships[i].on_asteroid = true;
                                events.push(format!(
                                    "{who}: stuck on the asteroid — cannot attack this round"
                                ));
                            }
                        }
                        ObstacleKind::Debris => {
                            events.push(format!("{who}: flies through debris — stressed"));
                            self.gain_stress(content, i, &mut events);
                            if die == AttackFace::Crit {
                                events.push(format!("{who}: debris strike — faceup damage card"));
                                self.faceup_card(content, i, roll, &mut events);
                            }
                        }
                        ObstacleKind::BlackHole => {
                            self.ships[i].destroyed = true;
                            events.push(format!("{who}: swallowed by the black hole — LOST"));
                        }
                    }
                    if self.ships[i].destroyed {
                        break;
                    }
                }
            }

            // Stress by the EFFECTIVE color (see maneuver_difficulty).
            if let Some(card) = rush {
                let e = self.discard_card(content, i, card, "red maneuver flown as white");
                events.push(e);
            }
            let stress_before = self.ships[i].stress;
            match difficulty {
                Difficulty::Hard => {
                    self.gain_stress(content, i, &mut events);
                    // Targeting Astromech: a lock after a red maneuver.
                    if self.has_effect(content, i, UpgradeEffect::LockAfterRed) {
                        self.auto_lock(content, i, "Targeting Astromech", &mut events);
                    }
                }
                Difficulty::Easy => {
                    self.lose_stress(content, i, &mut events);
                    // Systems Officer: a friend at Range 1 may lock.
                    if !self.ships[i].destroyed
                        && self.has_effect(content, i, UpgradeEffect::CrewFriendlyLockAfterGreen)
                    {
                        for f in self.friends_at_range1(content, i) {
                            if self.auto_lock(content, f, "Systems Officer", &mut events) {
                                break;
                            }
                        }
                    }
                    // Lando Calrissian (pilot): a friend at Range 1 takes a
                    // free action from its bar — focus, else evade.
                    if !self.ships[i].destroyed
                        && self.ability(content, &self.ships[i])
                            == Some(PilotAbility::FriendlyFreeActionAfterGreen)
                        && let Some(f) = self
                            .friends_at_range1(content, i)
                            .into_iter()
                            .find(|&f| self.may_act_freely(content, f))
                    {
                        let bar = self.action_bar(content, &self.ships[f]);
                        let what = if bar.contains(&ActionKind::Focus) {
                            self.ships[f].focus += 1;
                            Some("focus")
                        } else if bar.contains(&ActionKind::Evade) {
                            self.ships[f].evade += 1;
                            Some("evade")
                        } else {
                            None
                        };
                        if let Some(what) = what {
                            events.push(format!(
                                "{}: Lando Calrissian — {} takes a free {what} action",
                                self.label(content, i),
                                self.label(content, f)
                            ));
                        }
                    }
                    // R2-D2 (astromech): a shield back after a green maneuver.
                    if !self.ships[i].destroyed
                        && self.has_effect(content, i, UpgradeEffect::RecoverShieldOnGreen)
                        && self.ships[i].shields < self.max_shields(content, &self.ships[i])
                    {
                        self.ships[i].shields += 1;
                        events
                            .push(format!("{}: R2-D2 — shield recovered", self.label(content, i)));
                    }
                }
                Difficulty::Normal => {}
            }
            // "Night Beast": a free focus action after a green maneuver.
            if difficulty == Difficulty::Easy
                && !self.ships[i].destroyed
                && self.ships[i].stress == 0
                && self.ability(content, &self.ships[i]) == Some(PilotAbility::FreeFocusAfterGreen)
            {
                self.ships[i].focus += 1;
                events.push(format!(
                    "{}: ability — free focus after a green maneuver",
                    self.label(content, i)
                ));
            }

            // Mines: any token the base or template crossed goes off on
            // this ship, right after the maneuver.
            let mut mines_hit = Vec::new();
            let mut netted = false;
            if !self.ships[i].destroyed {
                let crossed: Vec<BombToken> = self
                    .bombs
                    .iter()
                    .filter(|t| t.kind.is_mine())
                    .filter(|t| {
                        let tc = t.corners();
                        used_path[..=stop]
                            .iter()
                            .any(|p| rules::obbs_overlap(&rules::footprint_corners(*p, fp), &tc))
                    })
                    .copied()
                    .collect();
                for token in crossed {
                    self.bombs.retain(|t| t.id != token.id);
                    if self.ships[i].destroyed {
                        break;
                    }
                    netted |= token.kind == BombKind::ConnerNet;
                    mines_hit.push(self.detonate(content, token, &[i], roll, &mut events));
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
            // Snap Shot: an enemy carrying it fires at this ship right
            // after its maneuver (Range 1, in arc; unmodified dice; once
            // per Activation phase — policy: always, at the first chance).
            let mut snap_shots = Vec::new();
            if !self.ships[i].destroyed {
                for s in 0..self.ships.len() {
                    if s == i
                        || self.ships[s].destroyed
                        || self.ships[s].snap_shot_fired
                        || self.allied(self.ships[s].owner, self.ships[i].owner)
                        || self.ships[i].destroyed
                    {
                        continue;
                    }
                    let Some(card) =
                        self.card_with_effect(content, s, UpgradeEffect::SnapShotReaction)
                    else {
                        continue;
                    };
                    if self.range_between(content, s, i) != Some(1)
                        || !self.ship_in_front_arc(content, s, i)
                    {
                        continue;
                    }
                    self.ships[s].snap_shot_fired = true;
                    events.push(format!(
                        "{}: Snap Shot at {} as it completes its maneuver",
                        self.label(content, s),
                        self.label(content, i)
                    ));
                    let shot = Shot {
                        d_idx: i,
                        range: 1,
                        weapon: Some(card),
                        second: false,
                        focus_hit: false,
                        no_mods: true,
                    };
                    snap_shots.push(self.perform_attack_on(content, s, shot, roll, &mut events));
                }
            }
            // Lightning Reflexes (switched on this round): after a white or
            // green maneuver the ship spins 180° in place, the card is
            // discarded and a stress token follows.
            if !self.ships[i].destroyed
                && difficulty != Difficulty::Hard
                && let Some(card) = self.using(content, i, UpgradeEffect::RotateShip180Discard)
            {
                let centre = combat::base_center(end, fp);
                end = Pose::new(
                    2.0 * centre.x - end.anchor.x,
                    2.0 * centre.y - end.anchor.y,
                    end.heading + std::f64::consts::PI,
                );
                self.ships[i].pose = Some(end);
                let e = self.discard_card(content, i, card, "rotated 180°, then stress");
                events.push(e);
                self.gain_stress(content, i, &mut events);
            }
            let destroyed = self.ships[i].destroyed;

            // "Snap" Wexley: a free boost after a 2-4 speed maneuver when
            // not touching a ship, before the Perform Action step.
            let mut second = None;
            if extras.second == Some(SecondActionKind::BoostAfterMove)
                && (2..=4).contains(&man.distance)
                && !bumped
                && matches!(planned2, Some(PlannedAction::Boost(_)))
                && self.may_act_freely(content, i)
            {
                let a = planned2.take().expect("matched above");
                let (r, _) = self.perform_action(content, i, a, fp, &obstacles, &mut events);
                events.push(format!(
                    "{}: ability — free boost after the maneuver",
                    self.label(content, i)
                ));
                second = Some((a, r));
            }

            // Perform Action step: one action, right after moving. Stress,
            // bumping, destruction, or damaged sensors all forfeit it.
            let planned = self.ships[i].planned_action.take().unwrap_or(PlannedAction::Pass);
            let mut dropped_after = Vec::new();
            let mut seismic = None;
            // Pattern Analyzer: this maneuver's stress lands after the
            // action; Primed Thrusters: boosts and rolls under 3 stress.
            let stress_now = if self.has_effect(content, i, UpgradeEffect::ResolveStressAfterAction)
            {
                stress_before
            } else {
                self.ships[i].stress
            };
            let primed = stress_now < 3
                && self.has_effect(content, i, UpgradeEffect::StressAllowsRepositionUnder3)
                && matches!(
                    planned,
                    PlannedAction::Boost(_)
                        | PlannedAction::BarrelRoll(_)
                        | PlannedAction::BarrelRollFar(_)
                        | PlannedAction::BarrelRollBank(..)
                );
            let action_result = if destroyed {
                ActionResult::Failed
            } else if stress_now > 0
                && !primed
                && self.ability(content, &self.ships[i]) != Some(PilotAbility::ActionsWhileStressed)
            {
                ActionResult::SkippedStressed
            } else if bumped {
                ActionResult::SkippedBumped
            } else if on_rock {
                ActionResult::SkippedObstacle
            } else if netted {
                ActionResult::SkippedNetted
            } else if planned != PlannedAction::Pass
                && self.ships[i].crits.contains(&CritEffect::DamagedSensorArray)
            {
                ActionResult::SkippedDamaged
            } else {
                let (r, tokens) = if let PlannedAction::CardActionAt(card, ob) = planned {
                    let (r, blast) =
                        self.fire_seismic_torpedo(content, i, card, ob, roll, &mut events);
                    seismic = blast;
                    (r, Vec::new())
                } else if let PlannedAction::CardAction(card) = planned
                    && Self::needs_dice(content, card)
                {
                    (self.dice_card_action(content, i, card, roll, &mut events), Vec::new())
                } else {
                    self.perform_action(content, i, planned, fp, &obstacles, &mut events)
                };
                dropped_after = tokens;
                // Daredevil without the boost icon: the dice are rolled here,
                // where dice are available.
                if r == ActionResult::Performed
                    && extras.daredevil
                    && !extras.turn_boost
                    && matches!(
                        planned,
                        PlannedAction::Boost(
                            action::BoostDir::TurnLeft | action::BoostDir::TurnRight
                        )
                    )
                    && !self.action_bar(content, &self.ships[i]).contains(&ActionKind::Boost)
                {
                    self.daredevil_damage(content, i, roll, &mut events);
                }
                // Second action: Darth Vader (always), Push the Limit
                // (after a performed action, then stress), Jake Farrell
                // (a reposition after a focus action).
                let grants = match extras.second {
                    Some(SecondActionKind::TwoActions) => true,
                    Some(SecondActionKind::FreeBarAction) => {
                        r == ActionResult::Performed && planned != PlannedAction::Pass
                    }
                    Some(SecondActionKind::RepositionAfterFocus) => {
                        r == ActionResult::Performed && planned == PlannedAction::Focus
                    }
                    Some(SecondActionKind::CardActionThenStress) => {
                        r == ActionResult::Performed && planned != PlannedAction::Pass
                    }
                    _ => false,
                };
                if grants
                    && let Some(a) = planned2.take()
                    && self.may_act_freely(content, i)
                {
                    let (r2, more) =
                        self.perform_action(content, i, a, fp, &obstacles, &mut events);
                    dropped_after.extend(more);
                    if extras.second == Some(SecondActionKind::FreeBarAction) {
                        events.push(format!(
                            "{}: Push the Limit — second action, then stress",
                            self.label(content, i)
                        ));
                        self.gain_stress(content, i, &mut events);
                    }
                    if extras.second == Some(SecondActionKind::CardActionThenStress) {
                        if let Some(card) =
                            self.card_with_effect(content, i, UpgradeEffect::ExtraActionThenStress)
                        {
                            self.ships[i].used_round.push(card);
                        }
                        events.push(format!(
                            "{}: Experimental Interface — free card action, then stress",
                            self.label(content, i)
                        ));
                        self.gain_stress(content, i, &mut events);
                    }
                    second = Some((a, r2));
                }
                r
            };

            let stress = self.ships[i].stress;
            records.push(MoveRecord {
                ship: id,
                maneuver: man,
                path: used_path[..=stop].to_vec(),
                end,
                bumped,
                destroyed,
                escaped: self.ships[i].escaped,
                snap_shots,
                stress,
                action: planned,
                action_result,
                dropped_before,
                dropped_after,
                mines_hit,
                pre,
                second,
                obstacles_hit,
                seismic,
            });
        }

        // Black holes drag everything nearby once all ships have moved.
        let pulls = self.gravity_pulls(content, &mut events);

        // End of the Activation phase: every dial-reveal bomb goes off
        // against all ships (either side) within Range 1 of its token.
        let mut detonations = Vec::new();
        let armed: Vec<BombToken> =
            self.bombs.iter().filter(|t| !t.kind.is_mine()).copied().collect();
        for token in armed {
            self.bombs.retain(|t| t.id != token.id);
            let tc = token.corners();
            let victims: Vec<usize> = (0..self.ships.len())
                .filter(|&k| !self.ships[k].destroyed)
                .filter(|&k| {
                    self.ships[k].pose.is_some_and(|p| {
                        let fp = self.class_of(content, &self.ships[k]).footprint;
                        combat::base_distance(&tc, &rules::footprint_corners(p, fp))
                            <= combat::RANGE_BAND_UNITS
                    })
                })
                .collect();
            detonations.push(self.detonate(content, token, &victims, roll, &mut events));
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

        (records, pulls, detonations, events)
    }

    /// Black holes: every ship within Range 5 of a core is dragged one
    /// unit straight toward it, heading unchanged, stopping short of any
    /// ship in the way; a base that reaches the core is swallowed.
    fn gravity_pulls(&mut self, content: &Content, events: &mut Vec<String>) -> Vec<Pull> {
        let holes: Vec<Obstacle> =
            self.obstacles.iter().filter(|o| o.kind == ObstacleKind::BlackHole).copied().collect();
        let mut pulls = Vec::new();
        for hole in holes {
            let core = hole.polygon();
            for i in 0..self.ships.len() {
                let Some(pose) = self.ships[i].pose else { continue };
                if self.ships[i].destroyed {
                    continue;
                }
                let fp = self.class_of(content, &self.ships[i]).footprint;
                let corners = rules::footprint_corners(pose, fp);
                if obstacle::polygon_distance(&corners, &core)
                    > obstacle::GRAVITY_BANDS * combat::RANGE_BAND_UNITS
                {
                    continue;
                }
                let d = hole.center - pose.anchor;
                let len = (d.x * d.x + d.y * d.y).sqrt();
                if len < 1e-9 {
                    continue;
                }
                let dir = Vec2::new(d.x / len, d.y / len);
                let others: Vec<[Vec2; 4]> = self
                    .ships
                    .iter()
                    .filter(|s| s.id != self.ships[i].id && !s.destroyed)
                    .filter_map(|s| {
                        s.pose.map(|p| {
                            rules::footprint_corners(p, self.class_of(content, s).footprint)
                        })
                    })
                    .collect();
                let mut best = pose;
                let mut swallowed = false;
                let steps = 10;
                for k in 1..=steps {
                    let t = obstacle::PULL_UNITS * k as f64 / steps as f64;
                    let cand = Pose {
                        anchor: Vec2::new(pose.anchor.x + dir.x * t, pose.anchor.y + dir.y * t),
                        heading: pose.heading,
                    };
                    let c = rules::footprint_corners(cand, fp);
                    if others.iter().any(|oc| rules::obbs_overlap(&c, oc)) {
                        break;
                    }
                    best = cand;
                    if obstacle::convex_overlap(&c, &core) {
                        swallowed = true;
                        break;
                    }
                }
                if best == pose {
                    continue;
                }
                let who = self.label(content, i);
                self.ships[i].pose = Some(best);
                if swallowed {
                    self.ships[i].destroyed = true;
                    events.push(format!("{who}: dragged into the black hole — SWALLOWED"));
                } else {
                    events.push(format!("{who}: pulled toward the black hole"));
                }
                pulls.push(Pull {
                    ship: self.ships[i].id,
                    hole: hole.id,
                    from: pose,
                    to: best,
                    swallowed,
                });
            }
        }
        pulls
    }

    /// End phase and turn bookkeeping once combat is complete.
    fn finish_turn(
        &mut self,
        content: &Content,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        // End of the Combat phase: Electronic Baffle sheds ion tokens, and
        // Fel's Wrath finally goes down.
        self.baffle_ion(content, events);
        for i in 0..self.ships.len() {
            if self.ships[i].hull == 0 && !self.ships[i].destroyed {
                self.ships[i].destroyed = true;
                events.push(format!(
                    "{}: Fel's Wrath — destroyed at the end of the Combat phase",
                    self.label(content, i)
                ));
            }
        }
        // Start of the End phase: Lieutenant Colzet spends his lock to
        // flip a facedown card on the locked ship.
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed
                || self.ability(content, &self.ships[i])
                    != Some(PilotAbility::SpendLockToFlipFacedownCrit)
            {
                continue;
            }
            let Some(l) = self.ships[i].lock else { continue };
            let Ok(e) = self.ship_index(l) else { continue };
            if self.ships[e].destroyed || self.facedown_cards(content, e) == 0 {
                continue;
            }
            self.ships[i].drop_lock(l);
            let effect = crit::draw(roll());
            events.push(format!(
                "{}: Lieutenant Colzet — lock spent, {}'s facedown card turns faceup: {}",
                self.label(content, i),
                self.label(content, e),
                effect.name()
            ));
            self.apply_crit_effect(content, e, effect, roll, events);
        }
        // End of the Combat phase: Mara Jade stresses every unstressed
        // enemy at Range 1.
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed
                || !self.has_effect(content, i, UpgradeEffect::CrewStressEnemiesAtRange1EndOfCombat)
            {
                continue;
            }
            let enemies: Vec<usize> = (0..self.ships.len())
                .filter(|&e| {
                    !self.allied(self.ships[e].owner, self.ships[i].owner)
                        && !self.ships[e].destroyed
                        && self.ships[e].stress == 0
                        && self.range_between(content, i, e) == Some(1)
                })
                .collect();
            for e in enemies {
                events.push(format!(
                    "{}: Mara Jade — {} is stressed",
                    self.label(content, i),
                    self.label(content, e)
                ));
                self.gain_stress(content, e, events);
            }
        }
        // End of the Combat phase: R5-P9 trades a focus token for a shield.
        for i in 0..self.ships.len() {
            let s = &self.ships[i];
            if !s.destroyed
                && s.focus > 0
                && s.shields < self.max_shields(content, s)
                && self.has_effect(content, i, UpgradeEffect::RecoverShieldSpendFocus)
            {
                self.ships[i].focus -= 1;
                self.ships[i].shields += 1;
                events.push(format!(
                    "{}: R5-P9 — focus spent, shield recovered",
                    self.label(content, i)
                ));
            }
        }
        // End phase: unspent focus and evade tokens are removed from all
        // ships; target locks persist, except locks on ships that are now
        // destroyed. Timed crits (Weapons Failure) tick down here.
        let dead: Vec<ShipId> = self.ships.iter().filter(|s| s.destroyed).map(|s| s.id).collect();
        for i in 0..self.ships.len() {
            // R5 Astromech: one Ship-trait faceup card is flipped facedown.
            if !self.ships[i].destroyed
                && self.has_effect(content, i, UpgradeEffect::FlipShipCritFacedown)
                && let Some(k) = self.ships[i].crits.iter().position(|c| !c.is_pilot_trait())
            {
                let c = self.ships[i].crits.remove(k);
                events.push(format!(
                    "{}: R5 Astromech — {} repaired (flipped facedown)",
                    self.label(content, i),
                    c.name()
                ));
            }
        }
        // Comm Relay: one unused evade token survives the End phase. Rey
        // (crew) stores one unused focus token on her card.
        let keep_evade: Vec<bool> = (0..self.ships.len())
            .map(|i| self.has_effect(content, i, UpgradeEffect::KeepOneEvade))
            .collect();
        let rey: Vec<bool> = (0..self.ships.len())
            .map(|i| self.has_effect(content, i, UpgradeEffect::CrewStoreFocusTokens))
            .collect();
        for ((k, ship), keep) in self.ships.iter_mut().enumerate().zip(keep_evade) {
            if rey[k] && ship.focus > 0 && !ship.destroyed {
                ship.stored_focus += 1;
                events.push(format!("{}: Rey — a focus token is stored", ship.callsign));
            }
            ship.focus = 0;
            ship.evade = if keep { ship.evade.min(1) } else { 0 };
            ship.tractor = 0;
            ship.used_round.clear();
            ship.card_uses.clear();
            ship.shield_lost_round = false;
            ship.on_asteroid = false;
            ship.card_actions.clear();
            ship.planned_action2 = None;
            for l in [ship.lock, ship.lock2].into_iter().flatten() {
                if dead.contains(&l) {
                    ship.drop_lock(l);
                }
            }
            for c in ship.crits.iter_mut() {
                if let CritEffect::WeaponsFailure { rounds } = c {
                    *rounds = rounds.saturating_sub(1);
                }
            }
            ship.crits.retain(|c| !matches!(c, CritEffect::WeaponsFailure { rounds: 0 }));
        }
        // End of the End phase: R2-D2 (crew) brings a shield back on a
        // shieldless ship, at the risk of a facedown card turning faceup.
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed
                || self.ships[i].shields > 0
                || !self.has_effect(content, i, UpgradeEffect::CrewRecoverShieldEndPhase)
            {
                continue;
            }
            self.ships[i].shields = 1;
            let who = self.label(content, i);
            if AttackFace::from_d8(roll()) == AttackFace::Hit
                && self.ships[i].hull < self.max_hull(content, &self.ships[i])
            {
                let effect = crit::draw(roll());
                events.push(format!(
                    "{who}: R2-D2 — shield recovered, but a facedown card turns faceup: {}",
                    effect.name()
                ));
                self.apply_crit_effect(content, i, effect, roll, events);
            } else {
                events.push(format!("{who}: R2-D2 — shield recovered"));
            }
        }

        self.mission_end_phase(content, events);

        let n = self.committed.len();
        self.white_reds = vec![false; n];
        self.committed = vec![false; n];
        self.turn += 1;
        self.check_victory();
    }

    /// One side left (or none): the game is over. With every last ship
    /// destroyed at once the side with initiative wins (core rules p.13).
    fn check_victory(&mut self) {
        if self.mission.is_some() {
            // Missions are won by their objectives only (p.21); a
            // reinforcement waiting to be placed reopens Placement.
            if let Some(w) = self.mission_winner() {
                self.phase = Phase::GameOver;
                self.winner = Some(w);
            } else if self.ships.iter().any(|s| !s.destroyed && s.pose.is_none()) {
                self.phase = Phase::Placement;
            } else {
                self.phase = Phase::Planning;
            }
            return;
        }
        let alive = self.alive_teams();
        match alive.len() {
            0 => {
                self.phase = Phase::GameOver;
                self.winner = Some(self.team(self.initiative));
            }
            1 => {
                self.phase = Phase::GameOver;
                self.winner = Some(alive[0]);
            }
            _ => self.phase = Phase::Planning,
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
        if self.ships[a_idx].on_asteroid
            || self.ships[a_idx]
                .crits
                .iter()
                .any(|c| matches!(c, CritEffect::WeaponsFailure { .. }))
        {
            return Vec::new();
        }
        let Some(a_pose) = self.ships[a_idx].pose else { return Vec::new() };
        let class = self.class_of(content, &self.ships[a_idx]);
        let a_fp = class.footprint;
        let a_corners = rules::footprint_corners(a_pose, a_fp);
        // (weapon, min range, max range, needs arc, requirement)
        // BTL-A4 Y-Wing: turret cards need the arc too.
        let arc_only = self.has_effect(content, a_idx, UpgradeEffect::TitleArcOnlyThenTurretAttack);
        let mut weapons: Vec<(Option<UpgradeId>, u8, u8, bool, AttackRequirement)> = Vec::new();
        // A token "ship" without a primary weapon (the senator's shuttle).
        if class.attack_dice > 0 {
            weapons.push((None, 1, 3, !class.turret_primary, AttackRequirement::Free));
        }
        for &u in &self.ships[a_idx].upgrades {
            if let Some(card) = content.upgrades.upgrade(u)
                && let Some(sw) = card.attack
                && matches!(card.slot, Slot::Torpedo | Slot::Missile | Slot::Cannon | Slot::Turret)
            {
                // Major Rhymer: secondary weapon ranges stretch by one
                // band each way (within Range 1-3).
                let rhymer = self.ability(content, &self.ships[a_idx])
                    == Some(PilotAbility::SecondaryRangePlusMinus1);
                let (lo, hi) = if rhymer {
                    ((sw.range_min - 1).max(1), (sw.range_max + 1).min(3))
                } else {
                    (sw.range_min, sw.range_max)
                };
                weapons.push((Some(u), lo, hi, card.slot != Slot::Turret || arc_only, sw.requires));
            }
        }
        let mut options = Vec::new();
        for s in &self.ships {
            if self.allied(s.owner, self.ships[a_idx].owner) || s.destroyed {
                continue;
            }
            let Some(pose) = s.pose else { continue };
            let fp = self.class_of(content, s).footprint;
            let corners = rules::footprint_corners(pose, fp);
            let dist = combat::base_distance(&a_corners, &corners);
            let in_arc = Self::base_in_front_arc(a_pose, a_fp, &corners);
            // Touching bases cannot be targeted — except by Arvel Crynyd,
            // who may shoot a touching ship inside his arc.
            let arvel = self.ability(content, &self.ships[a_idx])
                == Some(PilotAbility::TargetTouchingShipInArc);
            if dist <= 0.0 && !(arvel && in_arc) {
                continue;
            }
            let Some(band) = combat::range_band_between(&a_corners, &corners) else {
                continue;
            };
            for &(weapon, lo, hi, needs_arc, req) in &weapons {
                if band < lo || band > hi || (needs_arc && !in_arc) {
                    continue;
                }
                let deadeye = self.ships[a_idx].focus > 0
                    && self.has_effect(content, a_idx, UpgradeEffect::LockBecomesFocus);
                let armed = match req {
                    AttackRequirement::Free => true,
                    AttackRequirement::TargetLock => {
                        self.ships[a_idx].locks_on(s.id)
                            || deadeye
                            || self.synced_lock(content, a_idx, s.id)
                    }
                    AttackRequirement::Focus => self.ships[a_idx].focus > 0,
                };
                if armed {
                    let obstructed = self.obstructed_between(&a_corners, &corners);
                    options.push(AttackOption {
                        weapon,
                        target: s.id,
                        range: band,
                        dist,
                        obstructed,
                    });
                }
            }
        }
        // Biggs Darklighter: while he could be targeted (same weapon), his
        // friends at Range 1 of him cannot be.
        let biggs: Vec<(Option<UpgradeId>, usize)> = options
            .iter()
            .filter_map(|o| {
                let b = self.ships.iter().position(|s| s.id == o.target)?;
                (self.ability(content, &self.ships[b])
                    == Some(PilotAbility::ProtectFriendsAtRange1))
                .then_some((o.weapon, b))
            })
            .collect();
        if !biggs.is_empty() {
            options.retain(|o| {
                let t = self.ships.iter().position(|s| s.id == o.target).expect("option target");
                !biggs.iter().any(|&(w, b)| {
                    w == o.weapon
                        && b != t
                        && self.allied(self.ships[b].owner, self.ships[t].owner)
                        && self.range_between(content, b, t) == Some(1)
                })
            });
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
        let Shot { d_idx, range, weapon, second, focus_hit, no_mods: _ } = shot;
        let attacker = self.ships[a_idx].id;
        let defender = self.ships[d_idx].id;
        let a_pose = self.ships[a_idx].pose.expect("attackers are on the board");
        // Obstruction: the range ruler between the closest points crosses
        // an obstacle → the defender rolls one extra die (Trick Shot: the
        // attacker too).
        let obstructed = {
            let d_pose = self.ships[d_idx].pose.expect("targets are on the board");
            let a_fp = self.class_of(content, &self.ships[a_idx]).footprint;
            let d_fp = self.class_of(content, &self.ships[d_idx]).footprint;
            self.obstructed_between(
                &rules::footprint_corners(a_pose, a_fp),
                &rules::footprint_corners(d_pose, d_fp),
            )
        };
        if obstructed {
            events.push(format!(
                "{}: attack obstructed — defender +1 die",
                self.label(content, a_idx)
            ));
        }
        // Rebel Captive: the first ship to target this one each round is
        // stressed on declaring the attack.
        if !second
            && self.use_once(content, d_idx, UpgradeEffect::CrewStressFirstAttacker).is_some()
        {
            events.push(format!(
                "{}: Rebel Captive — {} receives a stress token",
                self.label(content, d_idx),
                self.label(content, a_idx)
            ));
            self.gain_stress(content, a_idx, events);
        }
        // R3-A2: an unstressed attacker takes a stress to stress a
        // defender inside its firing arc.
        if !second
            && self.ships[a_idx].stress == 0
            && self.has_effect(content, a_idx, UpgradeEffect::StressDefenderIfInArc)
            && self.ship_in_front_arc(content, a_idx, d_idx)
        {
            events.push(format!(
                "{}: R3-A2 — takes a stress, {} is stressed",
                self.label(content, a_idx),
                self.label(content, d_idx)
            ));
            self.gain_stress(content, a_idx, events);
            self.gain_stress(content, d_idx, events);
        }

        // Secondary weapon: its own dice, no range bonuses either way,
        // and the required token is spent up front when the card says so.
        let secondary = weapon
            .and_then(|u| content.upgrades.upgrade(u))
            .and_then(|c| c.attack.map(|sw| (c.name.clone(), sw)));
        let mut lock_spent = false;
        let mut attacker_focus_spent = false;
        if let Some((name, sw)) = &secondary {
            let again = if second { " again" } else { "" };
            events.push(format!("{}: fires {name}{again}", self.label(content, a_idx)));
            if sw.spend && !second {
                match sw.requires {
                    AttackRequirement::TargetLock if self.ships[a_idx].locks_on(defender) => {
                        self.ships[a_idx].drop_lock(defender);
                        lock_spent = true;
                    }
                    // Targeting Synchronizer: a friend's lock counts, and
                    // nothing is spent.
                    AttackRequirement::TargetLock if self.synced_lock(content, a_idx, defender) => {
                        events.push(format!(
                            "{}: Targeting Synchronizer — fires on a friend's lock",
                            self.label(content, a_idx)
                        ));
                    }
                    // Deadeye: the focus token is spent as the lock.
                    AttackRequirement::TargetLock => {
                        self.ships[a_idx].focus = self.ships[a_idx].focus.saturating_sub(1);
                        attacker_focus_spent = true;
                        events.push(format!(
                            "{}: Deadeye — focus token spent as the target lock",
                            self.label(content, a_idx)
                        ));
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
            None => {
                self.printed(content, &self.ships[a_idx]).attack
                    + u8::from(self.card_action_active(
                        content,
                        &self.ships[a_idx],
                        UpgradeEffect::ExposeAction,
                    ))
            }
        };
        // Zertik Strom: enemies at Range 1 of him get no Range-1 bonus.
        let zertik =
            self.enemy_ability_at_range1(content, a_idx, PilotAbility::DenyEnemyRange1Bonus);
        if zertik && range == 1 && secondary.is_none() {
            events.push(format!(
                "{}: Zertik Strom denies the Range 1 bonus die",
                self.label(content, a_idx)
            ));
        }
        let range_bonus = u8::from(range == 1 && secondary.is_none() && !zertik);
        let weapon_effect = weapon.and_then(|u| content.upgrades.upgrade(u)).and_then(|c| c.effect);
        let twice = matches!(
            weapon_effect,
            Some(UpgradeEffect::MissileAttackTwice | UpgradeEffect::TurretTwinLaserTwiceOneDamage)
        );
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
        let trick_shot = obstructed
            && self.has_effect(content, a_idx, UpgradeEffect::ExtraAttackDieIfObstructed);
        if trick_shot {
            events.push(format!("{}: Trick Shot — +1 attack die", self.label(content, a_idx)));
        }
        let weapon_extra = weapon_extra + u8::from(trick_shot);

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
            let extra = self.extra_attack_dice(content, a_idx, d_idx, range, events)
                + self.opportunist_die(content, a_idx, d_idx, events);
            (a_dice + range_bonus + extra + weapon_extra).saturating_sub(malfunctions)
        };
        let swarm = if secondary.is_none()
            && !blinded
            && self.has_effect(content, a_idx, UpgradeEffect::ExtraDiceFromFriendlyEvades)
        {
            self.swarm_leader_dice(content, a_idx, d_idx, events)
        } else {
            0
        };
        let n_atk = n_atk + swarm;
        let mut attack_faces: Vec<AttackFace> =
            (0..n_atk).map(|_| AttackFace::from_d8(roll())).collect();
        // Heavy Laser Cannon: crits become hits immediately after rolling.
        if weapon_effect == Some(UpgradeEffect::CannonCritsToHits) {
            for f in attack_faces.iter_mut().filter(|f| **f == AttackFace::Crit) {
                *f = AttackFace::Hit;
            }
        }
        // Finn: a blank joins a primary attack on a ship in arc (reroll
        // fodder).
        if secondary.is_none()
            && n_atk > 0
            && self.has_effect(content, a_idx, UpgradeEffect::CrewAddBlankIfEnemyInArc)
            && self.ship_in_front_arc(content, a_idx, d_idx)
        {
            attack_faces.push(AttackFace::Blank);
            events.push(format!("{}: Finn — adds a blank result", self.label(content, a_idx)));
        }
        // Sensor Jammer: the defender turns one hit into a focus result
        // that cannot be rerolled — held aside until the rerolls are over.
        let jammed = if self.has_effect(content, d_idx, UpgradeEffect::SystemAttackerHitToFocus)
            && let Some(k) = attack_faces.iter().position(|f| *f == AttackFace::Hit)
        {
            attack_faces.remove(k);
            events.push(format!(
                "{}: Sensor Jammer — one hit result becomes a focus result",
                self.label(content, d_idx)
            ));
            true
        } else {
            false
        };

        // Denials. Omega Leader: an enemy he has locked cannot modify any
        // dice against him, and cannot modify any when he attacks it.
        // Dark Curse: attackers cannot spend focus tokens or reroll.
        let omega = PilotAbility::LockedEnemiesCannotModifyDice;
        let attacker_may_modify = !(self.ability(content, &self.ships[d_idx]) == Some(omega)
            && self.ships[d_idx].locks_on(attacker))
            && !shot.no_mods;
        let defender_may_modify = !(self.ability(content, &self.ships[a_idx]) == Some(omega)
            && self.ships[a_idx].locks_on(defender));
        let dark_curse = self.ability(content, &self.ships[d_idx])
            == Some(PilotAbility::DefenderDeniesFocusAndRerolls);
        let attacker_may_spend = attacker_may_modify && !dark_curse;
        // Carnor Jax at Range 1: focus and evade tokens cannot be spent.
        let attacker_may_focus = attacker_may_spend && !self.carnor_near(content, a_idx);
        if !attacker_may_modify {
            events.push(format!(
                "{}: ability — locked attacker cannot modify dice",
                self.label(content, d_idx)
            ));
        } else if dark_curse
            && (self.ships[a_idx].focus > 0 || self.ships[a_idx].locks_on(defender))
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
        // Han Solo (pilot): a poor roll — fewer than half the dice landing,
        // focus results counted with a token to spend — is rerolled whole.
        if attacker_may_modify
            && self.ability(content, &self.ships[a_idx]) == Some(PilotAbility::RerollAllDice)
            && !attack_faces.is_empty()
        {
            let good = attack_faces
                .iter()
                .filter(|f| {
                    matches!(f, AttackFace::Hit | AttackFace::Crit)
                        || (**f == AttackFace::Focus && self.ships[a_idx].focus > 0)
                })
                .count();
            if good * 2 < attack_faces.len() {
                for f in attack_faces.iter_mut() {
                    *f = AttackFace::from_d8(roll());
                }
                events.push(format!(
                    "{}: Han Solo — rerolls every attack die",
                    self.label(content, a_idx)
                ));
            }
        }
        // Adv. Targeting Computer: a free critical hit with a lock on the
        // defender, which then cannot be spent.
        let mut lock_frozen = false;
        if attacker_may_modify
            && secondary.is_none()
            && self.ships[a_idx].locks_on(defender)
            && self.has_effect(content, a_idx, UpgradeEffect::SystemAddCritWithLock)
        {
            attack_faces.push(AttackFace::Crit);
            lock_frozen = true;
            events.push(format!(
                "{}: Adv. Targeting Computer — adds a critical hit, lock kept",
                self.label(content, a_idx)
            ));
        }
        // Han Solo (crew): the lock turns every focus result into a hit
        // when there is no token for them and that beats rerolling blanks.
        if attacker_may_spend
            && !lock_frozen
            && self.ships[a_idx].locks_on(defender)
            && self.ships[a_idx].focus == 0
            && self.has_effect(content, a_idx, UpgradeEffect::CrewLockAllFocusToHit)
        {
            let eyes = attack_faces.iter().filter(|f| **f == AttackFace::Focus).count();
            let blanks = attack_faces.iter().filter(|f| **f == AttackFace::Blank).count();
            if eyes > 0 && eyes >= blanks {
                for f in attack_faces.iter_mut().filter(|f| **f == AttackFace::Focus) {
                    *f = AttackFace::Hit;
                }
                self.ships[a_idx].drop_lock(defender);
                lock_spent = true;
                lock_frozen = true;
                events.push(format!(
                    "{}: Han Solo — lock spent, all focus results to hits",
                    self.label(content, a_idx)
                ));
            }
        }
        let all_crits = attacker_may_spend
            && !lock_frozen
            && self.spend_for_all_crits(content, a_idx, defender, &mut attack_faces, events);
        lock_spent |= all_crits;
        if attacker_may_spend && !all_crits && !lock_frozen && self.ships[a_idx].locks_on(defender)
        {
            let reroll_eyes = self.ships[a_idx].focus == 0;
            let mut any = false;
            for f in attack_faces.iter_mut() {
                if *f == AttackFace::Blank || (reroll_eyes && *f == AttackFace::Focus) {
                    *f = AttackFace::from_d8(roll());
                    any = true;
                }
            }
            if any {
                self.ships[a_idx].drop_lock(defender);
                lock_spent = true;
            }
        }
        if attacker_may_spend && !all_crits {
            let n = self.friendly_rerolls(content, a_idx, true);
            let done = self.reroll_attack_dice(a_idx, &mut attack_faces, n, roll);
            if done > 0 {
                events.push(format!(
                    "{}: rerolls {done} attack dice (friends at Range 1)",
                    self.label(content, a_idx)
                ));
            }
            self.talent_attack_rerolls(content, a_idx, d_idx, &mut attack_faces, roll, events);
            // Captain Jonus: a friend at Range 1 firing a secondary weapon
            // rerolls up to two dice.
            if secondary.is_some()
                && self.friends_at_range1(content, a_idx).into_iter().any(|f| {
                    self.ability(content, &self.ships[f])
                        == Some(PilotAbility::FriendlySecondaryReroll2AtRange1)
                })
            {
                let done = self.reroll_attack_dice(a_idx, &mut attack_faces, 2, roll);
                if done > 0 {
                    events.push(format!(
                        "{}: Captain Jonus — rerolls {done} attack dice",
                        self.label(content, a_idx)
                    ));
                }
            }
        }
        if jammed {
            attack_faces.push(AttackFace::Focus);
        }
        if attacker_may_modify {
            self.free_attack_mods(content, a_idx, range, &mut attack_faces, events);
            self.weapon_attack_mods(content, a_idx, weapon_effect, &mut attack_faces, events);
            // Kir Kanos: an evade token buys a hit result at Range 2-3.
            if (2..=3).contains(&range)
                && self.ships[a_idx].evade > 0
                && self.ability(content, &self.ships[a_idx])
                    == Some(PilotAbility::SpendEvadeForHitAtRange2To3)
            {
                self.ships[a_idx].evade -= 1;
                attack_faces.push(AttackFace::Hit);
                events.push(format!(
                    "{}: Kir Kanos — evade token spent for a hit result",
                    self.label(content, a_idx)
                ));
            }
            // Luke Skywalker (crew): the second attack turns a focus result
            // into a hit for free.
            if focus_hit && let Some(f) = attack_faces.iter_mut().find(|f| **f == AttackFace::Focus)
            {
                *f = AttackFace::Hit;
                events.push(format!(
                    "{}: Luke Skywalker — focus result to hit",
                    self.label(content, a_idx)
                ));
            }
            // Mercenary Copilot: a hit becomes a critical hit at Range 3.
            if range == 3
                && self.has_effect(content, a_idx, UpgradeEffect::CrewHitToCritAtRange3)
                && let Some(f) = attack_faces.iter_mut().find(|f| **f == AttackFace::Hit)
            {
                *f = AttackFace::Crit;
                events.push(format!(
                    "{}: Mercenary Copilot — hit result to critical hit",
                    self.label(content, a_idx)
                ));
            }
            // Agent Kallus: one focus result to a hit against his mark.
            let kallus = UpgradeEffect::CrewChosenEnemyFocusToHitOrEvade;
            if self.marked_enemy(content, a_idx, kallus) == Some(defender)
                && let Some(f) = attack_faces.iter_mut().find(|f| **f == AttackFace::Focus)
            {
                *f = AttackFace::Hit;
                events.push(format!(
                    "{}: Agent Kallus — focus result to hit",
                    self.label(content, a_idx)
                ));
            }
            // A Score to Settle: one focus result to a critical hit
            // against the marked enemy.
            if self.marked_enemy(content, a_idx, UpgradeEffect::ScoreToSettle) == Some(defender)
                && let Some(f) = attack_faces.iter_mut().find(|f| **f == AttackFace::Focus)
            {
                *f = AttackFace::Crit;
                events.push(format!(
                    "{}: A Score to Settle — focus result to critical hit",
                    self.label(content, a_idx)
                ));
            }
            // Guidance Chips: once per round a torpedo or missile die
            // becomes a hit (a critical hit with a 3+ primary weapon) — a
            // blank first, else a focus result no token will convert.
            let weapon_slot = weapon.and_then(|u| content.upgrades.upgrade(u)).map(|c| c.slot);
            if matches!(weapon_slot, Some(Slot::Torpedo | Slot::Missile)) {
                let pick =
                    attack_faces.iter().position(|f| *f == AttackFace::Blank).or_else(|| {
                        (self.ships[a_idx].focus == 0)
                            .then(|| attack_faces.iter().position(|f| *f == AttackFace::Focus))
                            .flatten()
                    });
                if let Some(k) = pick
                    && self.use_once(content, a_idx, UpgradeEffect::OrdnanceDieToHit).is_some()
                {
                    let big = self.printed(content, &self.ships[a_idx]).attack >= 3;
                    attack_faces[k] = if big { AttackFace::Crit } else { AttackFace::Hit };
                    events.push(format!(
                        "{}: Guidance Chips — die result to {}",
                        self.label(content, a_idx),
                        if big { "critical hit" } else { "hit" }
                    ));
                }
            }
            // Marksmanship (action this round): one focus result to a
            // critical hit, the rest to hits, no token needed.
            if self.card_action_active(
                content,
                &self.ships[a_idx],
                UpgradeEffect::FocusToCritOthersToHitAction,
            ) && attack_faces.contains(&AttackFace::Focus)
            {
                let mut first = true;
                for f in attack_faces.iter_mut().filter(|f| **f == AttackFace::Focus) {
                    *f = if first { AttackFace::Crit } else { AttackFace::Hit };
                    first = false;
                }
                events.push(format!(
                    "{}: Marksmanship — focus results converted",
                    self.label(content, a_idx)
                ));
            }
            // Expertise: every focus result becomes a hit for free while
            // unstressed (so the focus token is kept).
            if self.has_effect(content, a_idx, UpgradeEffect::AllFocusToHitIfUnstressed)
                && self.ships[a_idx].stress == 0
                && attack_faces.contains(&AttackFace::Focus)
            {
                for f in attack_faces.iter_mut().filter(|f| **f == AttackFace::Focus) {
                    *f = AttackFace::Hit;
                }
                events.push(format!(
                    "{}: Expertise — focus results to hits",
                    self.label(content, a_idx)
                ));
            }
        }
        attacker_focus_spent |= all_crits;
        // Calculation: with exactly one focus result, the focus token buys
        // a critical hit instead of a plain hit.
        if attacker_may_focus
            && self.ships[a_idx].focus > 0
            && self.has_effect(content, a_idx, UpgradeEffect::FocusToCritSpendFocus)
            && attack_faces.iter().filter(|f| **f == AttackFace::Focus).count() == 1
        {
            self.ships[a_idx].focus -= 1;
            attacker_focus_spent = true;
            if let Some(f) = attack_faces.iter_mut().find(|f| **f == AttackFace::Focus) {
                *f = AttackFace::Crit;
            }
            events.push(format!(
                "{}: Calculation — focus spent, focus result to critical hit",
                self.label(content, a_idx)
            ));
        }
        if attacker_may_focus
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
        // Weapons Guidance: a focus token with no focus result left to
        // convert turns a blank into a hit instead.
        if attacker_may_focus
            && self.ships[a_idx].focus > 0
            && self.has_effect(content, a_idx, UpgradeEffect::BlankToHitSpendFocus)
            && let Some(f) = attack_faces.iter_mut().find(|f| **f == AttackFace::Blank)
        {
            *f = AttackFace::Hit;
            self.ships[a_idx].focus -= 1;
            attacker_focus_spent = true;
            events.push(format!(
                "{}: Weapons Guidance — focus spent, blank to hit",
                self.label(content, a_idx)
            ));
        }
        if attacker_focus_spent {
            self.friend_spent_focus(content, a_idx, events);
        }
        // R3 Astromech: once per round a focus result nothing will convert
        // is cancelled for an evade token (primary weapon only).
        if secondary.is_none()
            && attacker_may_modify
            && let Some(k) = attack_faces.iter().position(|f| *f == AttackFace::Focus)
            && self.use_once(content, a_idx, UpgradeEffect::CancelFocusForEvade).is_some()
        {
            attack_faces.remove(k);
            self.ships[a_idx].evade += 1;
            events.push(format!(
                "{}: R3 Astromech — focus result cancelled, evade token gained",
                self.label(content, a_idx)
            ));
        }
        if defender_may_modify {
            self.defender_forces_rerolls(content, a_idx, d_idx, &mut attack_faces, roll, events);
        }
        // Accuracy Corrector: fewer than two results landing → cancel all
        // and add two hits; nothing may touch the dice afterwards.
        if attacker_may_modify
            && self.has_effect(content, a_idx, UpgradeEffect::SystemCancelAllAddTwoHits)
            && attack_faces
                .iter()
                .filter(|f| matches!(f, AttackFace::Hit | AttackFace::Crit))
                .count()
                < 2
        {
            attack_faces = vec![AttackFace::Hit, AttackFace::Hit];
            events.push(format!(
                "{}: Accuracy Corrector — all dice cancelled, two hits added",
                self.label(content, a_idx)
            ));
        }
        // Wampa: a lone critical (at most one result landing) is cashed in
        // for a facedown Damage card straight to the hull, no defense.
        let wampa = self.ability(content, &self.ships[a_idx])
            == Some(PilotAbility::CancelAllForFacedownDamage)
            && attack_faces.contains(&AttackFace::Crit)
            && attack_faces
                .iter()
                .filter(|f| matches!(f, AttackFace::Hit | AttackFace::Crit))
                .count()
                <= 1;
        if wampa {
            attack_faces.clear();
            events.push(format!(
                "{}: Wampa — cancels all dice for a facedown Damage card",
                self.label(content, a_idx)
            ));
        }
        let raw_hits = attack_faces.iter().filter(|f| **f == AttackFace::Hit).count() as u8;
        let raw_crits = attack_faces.iter().filter(|f| **f == AttackFace::Crit).count() as u8;

        // Roll defense dice (+1 at range 3 vs primary weapons).
        let mut d_agility = self.agility(content, &self.ships[d_idx]);
        // Outmaneuver: a defender in the attacker's arc that does not have
        // the attacker in its own arc loses one agility.
        if self.has_effect(content, a_idx, UpgradeEffect::ReduceAgilityIfNotInDefenderArc)
            && d_agility > 0
            && self.ship_in_front_arc(content, a_idx, d_idx)
            && !self.ship_in_front_arc(content, d_idx, a_idx)
        {
            d_agility -= 1;
            events
                .push(format!("{}: Outmaneuver — defender agility -1", self.label(content, a_idx)));
        }
        // Wedge Antilles: the defender's agility drops by one (minimum 0).
        if d_agility > 0
            && self.ability(content, &self.ships[a_idx])
                == Some(PilotAbility::DefenderAgilityMinus1)
        {
            d_agility -= 1;
            events.push(format!(
                "{}: Wedge Antilles — defender agility -1",
                self.label(content, a_idx)
            ));
        }
        // Intimidation: a defender touching an enemy that carries it loses
        // one agility.
        if d_agility > 0 && self.touching_intimidator(content, d_idx) {
            d_agility -= 1;
            events.push(format!(
                "{}: Intimidation — touching defender's agility -1",
                self.label(content, d_idx)
            ));
        }
        let n_def = d_agility + u8::from(range == 3 && secondary.is_none()) + u8::from(obstructed);
        let mut defense_faces: Vec<DefenseFace> =
            (0..n_def).map(|_| DefenseFace::from_d8(roll())).collect();
        // Lightweight Frame: outgunned, roll one more defense die.
        if attack_faces.len() as u8 > n_def
            && self.has_effect(content, d_idx, UpgradeEffect::ExtraDefenseDieIfOutgunned)
        {
            defense_faces.push(DefenseFace::from_d8(roll()));
            events.push(format!(
                "{}: Lightweight Frame — +1 defense die",
                self.label(content, d_idx)
            ));
        }
        // Finn (defending): a blank joins the roll when the attacker is in
        // this ship's arc.
        if self.has_effect(content, d_idx, UpgradeEffect::CrewAddBlankIfEnemyInArc)
            && self.ship_in_front_arc(content, d_idx, a_idx)
        {
            defense_faces.push(DefenseFace::Blank);
            events.push(format!("{}: Finn — adds a blank result", self.label(content, d_idx)));
        }

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
        // C-3PO: once per round, "zero evades" is guessed before the roll;
        // a roll with none gains one evade result.
        if incoming > 0
            && !defense_faces.contains(&DefenseFace::Evade)
            && self.use_once(content, d_idx, UpgradeEffect::CrewGuessEvades).is_some()
        {
            defense_faces.push(DefenseFace::Evade);
            events.push(format!(
                "{}: C-3PO — guessed zero evades correctly, +1 evade",
                self.label(content, d_idx)
            ));
        }
        if defender_may_modify {
            let evading = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
            if evading < incoming {
                let n = self.friendly_rerolls(content, d_idx, false);
                let done = self.reroll_defense_dice(d_idx, &mut defense_faces, n, roll);
                if done > 0 {
                    events.push(format!(
                        "{}: rerolls {done} defense dice (friends at Range 1)",
                        self.label(content, d_idx)
                    ));
                }
                self.talent_defense_rerolls(
                    content,
                    a_idx,
                    d_idx,
                    &mut defense_faces,
                    roll,
                    events,
                );
                // Flight Instructor: reroll one focus result — or a blank
                // against a pilot of skill 2 or less.
                if self.has_effect(content, d_idx, UpgradeEffect::CrewRerollDefenseDie) {
                    let low = self.effective_skill(content, &self.ships[a_idx]) <= 2;
                    let pick =
                        defense_faces.iter().position(|f| *f == DefenseFace::Focus).or_else(|| {
                            low.then(|| defense_faces.iter().position(|f| *f == DefenseFace::Blank))
                                .flatten()
                        });
                    if let Some(k) = pick {
                        defense_faces[k] = DefenseFace::from_d8(roll());
                        events.push(format!(
                            "{}: Flight Instructor — rerolls a defense die",
                            self.label(content, d_idx)
                        ));
                    }
                }
            }
            self.free_defense_mods(content, d_idx, &mut defense_faces, incoming, events);
            // Autothrusters: beyond Range 2 or outside the attacker's arc,
            // a blank becomes an evade.
            let evading = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
            if evading < incoming
                && (range == 3 || !self.ship_in_front_arc(content, a_idx, d_idx))
                && self.has_effect(content, d_idx, UpgradeEffect::BlankToEvadeAtRange3OrOutsideArc)
                && let Some(f) = defense_faces.iter_mut().find(|f| **f == DefenseFace::Blank)
            {
                *f = DefenseFace::Evade;
                events.push(format!(
                    "{}: Autothrusters — blank result to evade",
                    self.label(content, d_idx)
                ));
            }
            // Agent Kallus: one focus result to an evade against his mark.
            let evading = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
            if evading < incoming
                && self.marked_enemy(
                    content,
                    d_idx,
                    UpgradeEffect::CrewChosenEnemyFocusToHitOrEvade,
                ) == Some(attacker)
                && let Some(f) = defense_faces.iter_mut().find(|f| **f == DefenseFace::Focus)
            {
                *f = DefenseFace::Evade;
                events.push(format!(
                    "{}: Agent Kallus — focus result to evade",
                    self.label(content, d_idx)
                ));
            }
            // Luke Skywalker: one focus result to an evade, no token needed.
            let evading = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
            if evading < incoming
                && self.ability(content, &self.ships[d_idx])
                    == Some(PilotAbility::DefenseFocusToEvade)
                && let Some(f) = defense_faces.iter_mut().find(|f| **f == DefenseFace::Focus)
            {
                *f = DefenseFace::Evade;
                events.push(format!(
                    "{}: Luke Skywalker — focus result to evade",
                    self.label(content, d_idx)
                ));
            }
        }
        let mut evades = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
        let mut defender_focus_spent = false;
        let eyes = defense_faces.iter().filter(|f| **f == DefenseFace::Focus).count() as u8;
        let defender_may_spend =
            defender_may_modify && !defender_in_bullseye && !self.carnor_near(content, d_idx);
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
        if defender_focus_spent {
            self.friend_spent_focus(content, d_idx, events);
        }
        // Sensor Cluster: with no focus result to convert, the focus token
        // turns one blank into an evade instead.
        if defender_may_spend
            && self.ships[d_idx].focus > 0
            && evades < incoming
            && self.has_effect(content, d_idx, UpgradeEffect::BlankToEvadeSpendFocus)
            && let Some(f) = defense_faces.iter_mut().find(|f| **f == DefenseFace::Blank)
        {
            *f = DefenseFace::Evade;
            self.ships[d_idx].focus -= 1;
            defender_focus_spent = true;
            evades += 1;
            events.push(format!(
                "{}: Sensor Cluster — focus spent, blank to evade",
                self.label(content, d_idx)
            ));
        }
        // Homing Missiles: the defender cannot spend evade tokens.
        let evade_allowed = weapon_effect != Some(UpgradeEffect::MissileDenyEvadeTokens);
        let mut evade_spent = false;
        if defender_may_spend && evade_allowed && self.ships[d_idx].evade > 0 && evades < incoming {
            self.ships[d_idx].evade -= 1;
            evade_spent = true;
            evades += 1;
        }
        // Attacker's turn on the defense dice, taken only when one fewer
        // evade lets another result land. Juke: an evade token turns one
        // evade result into a focus. Crack Shot: discard the card to
        // cancel one evade result against a defender in the firing arc.
        let worth = |evades: u8| evades > 0 && evades <= incoming;
        if attacker_may_modify
            && worth(evades)
            && self.ships[a_idx].evade > 0
            && self.has_effect(content, a_idx, UpgradeEffect::EvadeToFocusIfEvadeToken)
            && let Some(f) = defense_faces.iter_mut().find(|f| **f == DefenseFace::Evade)
        {
            *f = DefenseFace::Focus;
            evades -= 1;
            events.push(format!(
                "{}: Juke — defender's evade result to focus",
                self.label(content, a_idx)
            ));
        }
        if worth(evades)
            && self.ship_in_front_arc(content, a_idx, d_idx)
            && let Some(card) = self.ships[a_idx].upgrades.iter().copied().find(|u| {
                content.upgrades.upgrade(*u).and_then(|c| c.effect)
                    == Some(UpgradeEffect::CancelEvadeDiscard)
            })
        {
            if self.tomax_keeps(content, a_idx, card) {
                events.push(format!(
                    "{}: Tomax Bren — Crack Shot flipped back faceup",
                    self.label(content, a_idx)
                ));
            } else {
                self.ships[a_idx].upgrades.retain(|u| *u != card);
            }
            evades -= 1;
            events.push(format!(
                "{}: Crack Shot — cancels an evade result, card discarded",
                self.label(content, a_idx)
            ));
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

        // R4-D6: with three or more uncancelled hits, cancel down to two
        // for a stress token each — only as far as the hull is at risk.
        if hits >= 3 && self.has_effect(content, d_idx, UpgradeEffect::CancelHitsForStress) {
            let to_hull = (hits + crits).saturating_sub(self.ships[d_idx].shields);
            let n = (hits - 2).min(to_hull);
            if n > 0 {
                hits -= n;
                events.push(format!(
                    "{}: R4-D6 — cancels {n} hit results for {n} stress",
                    self.label(content, d_idx)
                ));
                for _ in 0..n {
                    self.gain_stress(content, d_idx, events);
                }
            }
        }

        // "If this attack hits, … Then cancel all dice results": ion and
        // flechette weapons deal a fixed 1 damage (plus their token), Adv.
        // Homing Missiles a faceup card straight to the hull.
        let landed = hits + crits > 0;
        let mut bypass_shields = false;
        if landed {
            let who = self.label(content, d_idx);
            match weapon_effect {
                Some(UpgradeEffect::TurretIonOneDamage | UpgradeEffect::CannonIonOneDamage) => {
                    (hits, crits) = (1, 0);
                    self.ships[d_idx].ion += 1;
                    events.push(format!("{who}: ion cannon — 1 damage, ionized"));
                }
                Some(UpgradeEffect::MissileIonOneDamage) => {
                    (hits, crits) = (1, 0);
                    self.ships[d_idx].ion += 2;
                    events.push(format!("{who}: ion pulse — 1 damage, 2 ion tokens"));
                }
                Some(UpgradeEffect::CannonOneDamageAndStress) => {
                    (hits, crits) = (1, 0);
                    let stressed = if self.ships[d_idx].stress == 0 {
                        self.gain_stress(content, d_idx, events);
                        " and stressed"
                    } else {
                        ""
                    };
                    events.push(format!("{who}: flechette — 1 damage{stressed}"));
                }
                Some(UpgradeEffect::MissileFaceupDamage) => {
                    (hits, crits) = (0, 1);
                    bypass_shields = true;
                    events.push(format!("{who}: homing warhead — faceup damage card"));
                }
                Some(UpgradeEffect::TurretTwinLaserTwiceOneDamage) => {
                    (hits, crits) = (1, 0);
                    events.push(format!("{who}: twin laser — 1 damage"));
                }
                Some(UpgradeEffect::CannonTractorToken) => {
                    (hits, crits) = (0, 0);
                    self.ships[d_idx].tractor += 1;
                    events.push(format!("{who}: tractor beam — tractored (agility -1)"));
                }
                _ => {}
            }
        }

        // Draw Their Fire: a friend at Range 1 takes one critical hit that
        // would otherwise reach the defender's hull.
        if crits > 0
            && self.ships[d_idx].shields == 0
            && let Some(f) = self
                .friends_at_range1(content, d_idx)
                .into_iter()
                .find(|&f| self.has_effect(content, f, UpgradeEffect::SufferCritForFriendly))
        {
            crits -= 1;
            events.push(format!(
                "{}: Draw Their Fire — takes a critical hit for {}",
                self.label(content, f),
                self.label(content, d_idx)
            ));
            if self.damage_point(f) == DamagePoint::Hull && !self.ships[f].destroyed {
                let effect = crit::draw(roll());
                events.push(format!("{}: critical — {}", self.label(content, f), effect.name()));
                self.apply_crit_effect(content, f, effect, roll, events);
            }
        }

        // Mission 1: critical hits against the senator's shuttle are hits.
        if self.mission.as_ref().is_some_and(|m| m.shuttle == Some(self.ships[d_idx].id)) {
            hits += crits;
            crits = 0;
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
            let point =
                if bypass_shields { self.hull_point(d_idx) } else { self.damage_point(d_idx) };
            match point {
                DamagePoint::Shield => shields_lost += 1,
                DamagePoint::Hull => {
                    hull_lost += 1;
                    crits_to_hull += 1;
                    if !self.ships[d_idx].destroyed {
                        let effect = self.draw_crit_for(content, a_idx, roll, events);
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

        if wampa && !self.ships[d_idx].destroyed && self.hull_point(d_idx) == DamagePoint::Hull {
            hull_lost += 1;
            let died = if self.ships[d_idx].destroyed { " — DESTROYED" } else { "" };
            events.push(format!(
                "{}: facedown Damage card from Wampa{died}",
                self.label(content, d_idx)
            ));
        }
        if shields_lost > 0 && !self.ships[d_idx].destroyed {
            self.after_shield_loss(content, d_idx, events);
        }
        // Reinforced Deflectors: three or more damage brings a shield back.
        if shields_lost + hull_lost >= 3
            && !self.ships[d_idx].destroyed
            && self.has_effect(content, d_idx, UpgradeEffect::SystemRecoverShieldAfter3Damage)
            && self.ships[d_idx].shields < self.max_shields(content, &self.ships[d_idx])
        {
            self.ships[d_idx].shields += 1;
            events.push(format!(
                "{}: Reinforced Deflectors — shield recovered",
                self.label(content, d_idx)
            ));
        }
        // "If you are hit by an attack": at least one uncanceled result.
        if hits + crits > 0 {
            self.discard_on_hit(content, d_idx, events);
        }
        if let Some(effect) = weapon_effect {
            self.after_attack_effects(content, a_idx, d_idx, effect, landed, events);
        }
        if !twice || second {
            let outcome = AttackOutcome { landed, lock_spent };
            self.after_attack_cards(content, a_idx, shot, outcome, roll, events);
        }
        // Ordnance is discarded once fired (after the repeat, if any);
        // Munitions Failsafe keeps it after a miss.
        if let Some((name, sw)) = &secondary
            && sw.discard_to_fire
            && (!twice || second)
            && (landed || !self.has_effect(content, a_idx, UpgradeEffect::KeepOrdnanceOnMiss))
            && !weapon.is_some_and(|w| self.spend_ordnance(content, a_idx, w, events))
        {
            self.ships[a_idx].upgrades.retain(|u| Some(*u) != weapon);
            events.push(format!("{}: {name} discarded (fired)", self.label(content, a_idx)));
        }
        // Turr Phennir: a free boost or barrel roll after the attack.
        let mut reposition = None;
        if (!twice || second)
            && self.ability(content, &self.ships[a_idx])
                == Some(PilotAbility::FreeRepositionAfterAttack)
            && let Some(a) = self.ships[a_idx].planned_action2.take()
            && self.may_act_freely(content, a_idx)
        {
            let fp = self.class_of(content, &self.ships[a_idx]).footprint;
            let others: Vec<[Vec2; 4]> = self
                .ships
                .iter()
                .enumerate()
                .filter(|(k, s)| *k != a_idx && !s.destroyed)
                .filter_map(|(_, s)| {
                    s.pose.map(|p| rules::footprint_corners(p, self.class_of(content, s).footprint))
                })
                .collect();
            let (result, _) = self.perform_action(content, a_idx, a, fp, &others, events);
            events.push(format!(
                "{}: Turr Phennir — free reposition after attacking{}",
                self.label(content, a_idx),
                if result == ActionResult::Performed { "" } else { " FAILED" }
            ));
            let to = self.ships[a_idx].pose.expect("attacker stays on the board");
            reposition = Some(Reposition { action: a, result, to });
        }
        AttackRecord {
            attacker,
            defender,
            range,
            weapon,
            obstructed,
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
            reposition,
        }
    }

    /// Concede: the player's ships leave the game. With more than two
    /// sides the others fight on; returns the winning side once the game
    /// is over (a resigning player's side may still win through a
    /// teammate).
    pub fn resign(&mut self, player: PlayerId) -> Option<u8> {
        for s in self.ships.iter_mut().filter(|s| s.owner == player) {
            s.destroyed = true;
        }
        let seat = player.0 as usize;
        if seat < self.committed.len() {
            self.committed[seat] = true;
        }
        if self.mission.is_some()
            && let Some(w) = self.mission_winner()
        {
            self.phase = Phase::GameOver;
            self.winner = Some(w);
            return self.winner;
        }
        let alive = self.alive_teams();
        if alive.len() >= 2 {
            return None;
        }
        self.phase = Phase::GameOver;
        self.winner = Some(alive.first().copied().unwrap_or_else(|| {
            let mut others: Vec<u8> = (0..self.committed.len() as u8)
                .map(|s| self.team(PlayerId(u32::from(s))))
                .filter(|t| *t != self.team(player))
                .collect();
            others.sort_unstable();
            others.first().copied().unwrap_or(0)
        }));
        self.winner
    }

    /// Scatter obstacle tokens on the board before setup (core rules
    /// p.20 spacing; drawn at random instead of placed by the players).
    pub fn place_obstacles(&mut self, kinds: &[ObstacleKind], seed: u64) {
        self.obstacles = obstacle::scatter(&self.board, kinds, seed);
    }

    /// Does a base at `pose` overlap any obstacle token?
    fn on_obstacle(&self, corners: &[Vec2; 4]) -> Option<&Obstacle> {
        self.obstacles.iter().find(|o| obstacle::convex_overlap(corners, &o.polygon()))
    }

    /// Is the range line between two bases (closest points) crossing an
    /// obstacle token?
    pub fn obstructed_between(&self, a: &[Vec2; 4], b: &[Vec2; 4]) -> bool {
        let (p, q) = combat::closest_points(a, b);
        self.obstacles.iter().any(|o| obstacle::segment_hits_polygon(p, q, &o.polygon()))
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
                    team: self.team(s.owner),
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
                    upgrade_ids: s.upgrades.clone(),
                    // Setup placement is hidden; a mission reinforcement
                    // is placed in the open.
                    pose: if own
                        || self.phase != Phase::Placement
                        || self.turn > 1
                        || self.late_setup_pending()
                    {
                        s.pose
                    } else {
                        None
                    },
                    hull: s.hull,
                    shields: s.shields,
                    stress: s.stress,
                    focus: s.focus,
                    evade: s.evade,
                    ion: s.ion,
                    lock: s.lock,
                    lock2: s.lock2,
                    crits: s.crits.clone(),
                    destroyed: s.destroyed,
                    escaped: s.escaped,
                    satellites: s.satellites,
                    card_uses: if own { s.card_uses.clone() } else { Vec::new() },
                    on_asteroid: s.on_asteroid,
                    plan: if own { s.plan } else { None },
                    planned_action: if own { s.planned_action } else { None },
                    bomb: if own { s.bomb } else { None },
                    planned_action2: if own { s.planned_action2 } else { None },
                    extras: if own {
                        self.action_extras(content, s)
                    } else {
                        ActionExtras::default()
                    },
                }
            })
            .collect()
    }
}

/// Mission rules (core rules p.21-24). See `mission.rs` for the data
/// side; policies for the choices the rulebook leaves to a player are
/// noted on each function.
impl GameState {
    /// Turn a freshly built game into a mission game: assign sides,
    /// place the shuttle / satellites, mark the disabled ship, and give
    /// the Empire the initiative on a points tie (p.21). Call after the
    /// obstacles are placed. `points` is the squad size the host chose.
    pub fn start_mission(
        &mut self,
        content: &Content,
        kind: MissionKind,
        points: u32,
    ) -> Result<(), String> {
        if self.sides() != 2 {
            return Err("missions are played between two sides".into());
        }
        // The Rebel side is whichever side flies Rebel ships (side 0 if
        // nobody does — an empty board is not a game anyway).
        let rebel_side = self
            .ships
            .iter()
            .find(|s| self.class_of(content, s).faction == Faction::RebelAlliance)
            .map(|s| self.team(s.owner))
            .unwrap_or(0);
        let imperial_side = 1 - rebel_side;
        let first_seat = |side: u8| {
            PlayerId(self.teams.iter().position(|t| *t == side).unwrap_or(side as usize) as u32)
        };
        let mut state = MissionState {
            kind,
            rebel_side,
            imperial_side,
            shuttle: None,
            disabled: None,
            satellites: Vec::new(),
            reinforced: Vec::new(),
            spawned: 0,
        };
        let rebel_seat = Seat::for_side(rebel_side, 2);
        match kind {
            MissionKind::PoliticalEscort => {
                let pilot = content
                    .pilots
                    .pilots
                    .iter()
                    .find(|p| p.xws == "senatorsshuttle")
                    .ok_or("the senator's shuttle is missing from pilots.ron")?;
                let class = content.ships.class(pilot.class).ok_or("shuttle class")?;
                let id = ShipId(self.ships.len() as u32);
                let mut shuttle = ShipState::new(
                    id,
                    first_seat(rebel_side),
                    class.id,
                    pilot.id,
                    "Senator".into(),
                    class.hull,
                    kind.shuttle_shields(points),
                );
                shuttle.pose =
                    Some(mission::shuttle_pose(&self.board, rebel_seat, class.footprint.length));
                self.ships.push(shuttle);
                state.shuttle = Some(id);
            }
            MissionKind::AsteroidRun => {
                // "He chooses one of his ships": the first ship of the
                // Rebel squad (the builder's order is the player's choice).
                state.disabled =
                    self.ships.iter().find(|s| self.team(s.owner) == rebel_side).map(|s| s.id);
            }
            MissionKind::DarkWhispers => {
                let n = kind.satellite_count(points);
                state.satellites = mission::satellite_positions(&self.board, rebel_seat, n)
                    .into_iter()
                    .enumerate()
                    .map(|(i, pos)| mission::Satellite {
                        id: i as u32,
                        pos,
                        holder: None,
                        supply: false,
                    })
                    .collect();
            }
        }
        // Initiative: lowest squad total, ties to the Empire (p.21).
        let side_total = |side: u8| -> u32 {
            self.squad_totals
                .iter()
                .zip(&self.teams)
                .filter(|(_, t)| **t == side)
                .map(|(c, _)| *c)
                .sum()
        };
        if side_total(rebel_side) == side_total(imperial_side) {
            self.initiative = first_seat(imperial_side);
        }
        self.mission = Some(state);
        Ok(())
    }

    /// Setup has reached the late placements (Han Solo, HotR): every
    /// ordinary ship is down, and poses are public from here on.
    fn late_setup_pending(&self) -> bool {
        self.turn == 1
            && self.phase == Phase::Placement
            && self.ships.iter().all(|s| s.late_setup || s.pose.is_some())
            && self.ships.iter().any(|s| s.late_setup && s.pose.is_none())
    }

    /// Where `player` may place ships right now: the mission's zones,
    /// or the standard deployment band.
    pub fn deploy_zones(&self, player: PlayerId) -> Vec<mission::Rect> {
        if self.late_setup_pending()
            && self.ships.iter().any(|s| s.owner == player && s.late_setup && s.pose.is_none())
        {
            return vec![(0.0, 0.0, self.board.width, self.board.height)];
        }
        let own = self.seat_of(player);
        match &self.mission {
            None => vec![self.board.deploy_zone(own)],
            Some(m) => {
                let side = self.team(player);
                let other = Seat::for_side(1 - side, 2);
                mission::deploy_zones(
                    m.kind,
                    &self.board,
                    m.faction_of_side(side),
                    own,
                    other,
                    self.turn > 1,
                )
            }
        }
    }

    /// The mission as the client shows it to `viewer`.
    pub fn mission_view(&self, viewer: PlayerId) -> Option<MissionView> {
        self.mission.as_ref().map(|m| MissionView {
            kind: m.kind,
            rebel_side: m.rebel_side,
            objective: m.kind.objective(m.faction_of_side(self.team(viewer))).to_string(),
            satellites: m.satellites.clone(),
            shuttle: m.shuttle,
            disabled: m.disabled,
        })
    }

    /// Why the game ended, for the Game Over message.
    pub fn winner_reason(&self) -> String {
        match (&self.mission, self.winner) {
            (Some(m), Some(w)) => {
                format!("{} — {}", m.kind.name(), m.kind.objective(m.faction_of_side(w)))
            }
            _ => "fleet destroyed".into(),
        }
    }

    /// May the ship at `i`, leaving the board over `exit`, escape instead
    /// of being destroyed (p.22-24 "not considered destroyed")?
    fn mission_escape(&self, i: usize, exit: Seat) -> bool {
        let Some(m) = &self.mission else { return false };
        let ship = &self.ships[i];
        let imperial_edge = Seat::for_side(m.imperial_side, 2);
        let rebel_edge = Seat::for_side(m.rebel_side, 2);
        match m.kind {
            MissionKind::PoliticalEscort => m.shuttle == Some(ship.id) && exit == imperial_edge,
            MissionKind::AsteroidRun => {
                m.disabled == Some(ship.id)
                    && self.turn >= mission::REPAIR_ROUND
                    && (exit == rebel_edge || exit == imperial_edge)
            }
            MissionKind::DarkWhispers => {
                self.team(ship.owner) == m.imperial_side
                    && ship.satellites > 0
                    && exit == imperial_edge
                    && m.all_scanned()
            }
        }
    }

    /// Mission 3: an Imperial ship overlapping a satellite token, or
    /// touching a Rebel ship that overlaps one, scans it instead of
    /// attacking. Policy: always scan when possible (one token per
    /// Combat phase). Returns the log line when a scan happened.
    fn mission_scan(&mut self, content: &Content, a_idx: usize) -> Option<String> {
        let m = self.mission.as_ref()?;
        if m.kind != MissionKind::DarkWhispers
            || self.team(self.ships[a_idx].owner) != m.imperial_side
        {
            return None;
        }
        let pose = self.ships[a_idx].pose?;
        let fp = self.class_of(content, &self.ships[a_idx]).footprint;
        let mine = rules::footprint_corners(pose, fp);
        // Bases the scan may reach through: the ship's own, plus every
        // touching Rebel base.
        let mut reach = vec![mine];
        for k in 0..self.ships.len() {
            let other = &self.ships[k];
            if k == a_idx || other.destroyed || self.team(other.owner) != m.rebel_side {
                continue;
            }
            if let Some(p) = other.pose {
                let oc = rules::footprint_corners(p, self.class_of(content, other).footprint);
                if combat::base_distance(&mine, &oc) <= 1e-6 {
                    reach.push(oc);
                }
            }
        }
        let sat = m.satellites.iter().position(|s| {
            s.on_board() && reach.iter().any(|b| rules::obbs_overlap(b, &s.corners()))
        })?;
        let id = self.ships[a_idx].id;
        let who = self.label(content, a_idx);
        let m = self.mission.as_mut().expect("checked");
        m.satellites[sat].holder = Some(id);
        self.ships[a_idx].satellites += 1;
        let left = m.satellites.iter().filter(|s| s.on_board()).count();
        let num = m.satellites[sat].id + 1;
        Some(format!(
            "{who}: scans satellite {num} instead of attacking ({left} left on the board)"
        ))
    }

    /// Who has fulfilled their objective (side index), if anyone.
    fn mission_winner(&self) -> Option<u8> {
        let m = self.mission.as_ref()?;
        let ship = |id: Option<ShipId>| id.and_then(|id| self.ships.iter().find(|s| s.id == id));
        let imperial_alive =
            self.ships.iter().any(|s| !s.destroyed && self.team(s.owner) == m.imperial_side);
        match m.kind {
            MissionKind::PoliticalEscort => {
                let shuttle = ship(m.shuttle)?;
                if shuttle.escaped {
                    Some(m.rebel_side)
                } else if shuttle.destroyed {
                    Some(m.imperial_side)
                } else {
                    None
                }
            }
            MissionKind::AsteroidRun => {
                let disabled = ship(m.disabled)?;
                if disabled.escaped {
                    Some(m.rebel_side)
                } else if disabled.destroyed {
                    Some(m.imperial_side)
                } else {
                    None
                }
            }
            MissionKind::DarkWhispers => {
                let carried_home = self.ships.iter().any(|s| {
                    s.escaped && s.satellites > 0 && self.team(s.owner) == m.imperial_side
                });
                let in_supply = |s: &mission::Satellite| {
                    s.supply
                        || s.holder.is_some_and(|h| {
                            self.ships.iter().any(|x| x.id == h && x.destroyed && !x.escaped)
                        })
                };
                if carried_home {
                    Some(m.imperial_side)
                } else if m.satellites.iter().all(in_supply) || !imperial_alive {
                    Some(m.rebel_side)
                } else {
                    None
                }
            }
        }
    }

    /// End phase: satellites on destroyed ships go back to the supply,
    /// and the reinforcing side gets one generic pilot per ship it lost
    /// this round, to be placed before the next Planning phase
    /// (policy: reinforcements are always called for).
    fn mission_end_phase(&mut self, content: &Content, events: &mut Vec<String>) {
        let Some(m) = &self.mission else { return };
        let kind = m.kind;
        for k in 0..m.satellites.len() {
            let holder = self.mission.as_ref().expect("mission").satellites[k].holder;
            if let Some(h) = holder
                && self.ships.iter().any(|s| s.id == h && s.destroyed && !s.escaped)
            {
                let m = self.mission.as_mut().expect("mission");
                m.satellites[k].holder = None;
                m.satellites[k].supply = true;
                events.push(format!("satellite {} returns to the supply", m.satellites[k].id + 1));
            }
        }
        let (faction, pilot_xws, _) = kind.reinforcements();
        let m = self.mission.as_ref().expect("mission");
        let side = m.side_of(faction);
        let lost: Vec<ShipId> = self
            .ships
            .iter()
            .filter(|s| {
                s.destroyed
                    && !s.escaped
                    && self.team(s.owner) == side
                    && !m.reinforced.contains(&s.id)
            })
            .map(|s| s.id)
            .collect();
        if lost.is_empty() {
            return;
        }
        let Some(pilot) = content.pilots.pilots.iter().find(|p| p.xws == pilot_xws) else {
            return;
        };
        let Some(class) = content.ships.class(pilot.class) else { return };
        let owner = PlayerId(self.teams.iter().position(|t| *t == side).unwrap_or(0) as u32);
        for id in lost {
            let m = self.mission.as_mut().expect("mission");
            m.reinforced.push(id);
            m.spawned += 1;
            let n = m.spawned;
            let callsign = format!("Reserve-{n}");
            let ship = ShipState::new(
                ShipId(self.ships.len() as u32),
                owner,
                class.id,
                pilot.id,
                callsign.clone(),
                class.hull,
                class.shields,
            );
            self.ships.push(ship);
            events.push(format!(
                "reinforcement: {} ({}) arrives as {callsign} — place it within Range 1 of the edge",
                pilot.name, class.name
            ));
        }
    }
}

impl GameState {
    /// Weapons Engineer (crew): two target locks may be held.
    fn two_locks(&self, content: &Content, i: usize) -> bool {
        self.has_effect(content, i, UpgradeEffect::CrewTwoLocks)
    }

    /// Weapons Engineer: acquiring a lock may lock a second, different
    /// ship too — policy: the nearest other enemy within Range 1-3.
    fn weapons_engineer_second_lock(
        &mut self,
        content: &Content,
        i: usize,
        first: ShipId,
        events: &mut Vec<String>,
    ) {
        let Some(p) = self.ships[i].pose else { return };
        let mine = rules::footprint_corners(p, self.class_of(content, &self.ships[i]).footprint);
        let other = (0..self.ships.len())
            .filter(|&e| {
                let s = &self.ships[e];
                s.id != first
                    && !s.destroyed
                    && !self.allied(s.owner, self.ships[i].owner)
                    && !self.ships[i].locks_on(s.id)
            })
            .filter_map(|e| {
                let q = self.ships[e].pose?;
                let theirs =
                    rules::footprint_corners(q, self.class_of(content, &self.ships[e]).footprint);
                let d = combat::base_distance(&mine, &theirs);
                (d <= 3.0 * combat::RANGE_BAND_UNITS).then_some((e, d))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(e, _)| e);
        if let Some(e) = other {
            let eid = self.ships[e].id;
            self.ships[i].take_lock(eid, true);
            events.push(format!(
                "{}: Weapons Engineer — second lock on {}",
                self.label(content, i),
                self.label(content, e)
            ));
        }
    }

    /// Daredevil without the boost icon: two attack dice against the
    /// ship — hits are damage, criticals faceup cards.
    fn daredevil_damage(
        &mut self,
        content: &Content,
        i: usize,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) {
        let faces = [AttackFace::from_d8(roll()), AttackFace::from_d8(roll())];
        let mut hits = 0;
        let mut crits = 0;
        for f in faces {
            match f {
                AttackFace::Hit => hits += 1,
                AttackFace::Crit => crits += 1,
                _ => {}
            }
        }
        for _ in 0..hits {
            if !self.ships[i].destroyed {
                self.damage_point(i);
            }
        }
        for _ in 0..crits {
            if self.ships[i].destroyed {
                break;
            }
            if self.damage_point(i) == DamagePoint::Hull && !self.ships[i].destroyed {
                let effect = crit::draw(roll());
                self.apply_crit_effect(content, i, effect, roll, events);
            }
        }
        let died = if self.ships[i].destroyed { " — DESTROYED" } else { "" };
        events.push(format!(
            "{}: Daredevil — no boost icon: {hits} hit(s), {crits} critical(s) suffered{died}",
            self.label(content, i)
        ));
    }

    /// Electronic Baffle (switched on): at the end of the Combat phase each
    /// ion token is shed for one damage, never a fatal one. (The card acts
    /// on receipt; this happens before the next reveal, when ion matters.)
    fn baffle_ion(&mut self, content: &Content, events: &mut Vec<String>) {
        for i in 0..self.ships.len() {
            if self.ships[i].destroyed
                || self.using(content, i, UpgradeEffect::SystemDamageToDiscardToken).is_none()
            {
                continue;
            }
            while self.ships[i].ion > 0 && (self.ships[i].shields > 0 || self.ships[i].hull > 1) {
                self.damage_point(i);
                self.ships[i].ion -= 1;
                events.push(format!(
                    "{}: Electronic Baffle — 1 damage, ion token discarded",
                    self.label(content, i)
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::action::{BoostDir, Side};
    use crate::ship::ShipClassId;
    use crate::squad::SquadShip;
    use std::f64::consts::FRAC_PI_2;

    const TIE: ShipClassId = ShipClassId(1);
    const XWING: ShipClassId = ShipClassId(2);
    const P0: PlayerId = PlayerId(0);
    const P1: PlayerId = PlayerId(1);

    // ---------------- Missions (core rules p.21-24) ----------------

    fn mission_game(c: &Content, kind: MissionKind, points: u32) -> GameState {
        let rebel = mission::fixed_squad(c, kind, Faction::RebelAlliance).unwrap();
        let imperial = mission::fixed_squad(c, kind, Faction::Empire).unwrap();
        let mut gs =
            GameState::from_squads(board(), c, &[&rebel, &imperial], &[0, 1], AttackFace::Blank)
                .unwrap();
        gs.start_mission(c, kind, points).unwrap();
        gs
    }

    fn straight(c: &Content, class: ShipClassId, d: u8) -> u8 {
        let set = c.ships.class(class).unwrap().maneuver_set;
        let dial = &c.dials.set(set).unwrap().maneuvers;
        dial.iter()
            .position(|m| m.steer == crate::maneuver::Steer::Straight && m.distance == d)
            .unwrap() as u8
    }

    const T65: ShipClassId = ShipClassId(11);
    const SHUTTLE: ShipClassId = ShipClassId(12);

    /// Every living ship flies the given straight; both seats commit.
    fn fly_all(c: &Content, gs: &mut GameState, dist: u8) -> TurnRecords {
        for i in 0..gs.ships.len() {
            if gs.ships[i].destroyed || gs.ships[i].plan.is_some() {
                continue;
            }
            let (owner, id, class) = (gs.ships[i].owner, gs.ships[i].id, gs.ships[i].class);
            gs.plan_maneuver(c, owner, id, straight(c, class, dist)).unwrap();
        }
        gs.commit_plans(c, P0, &mut || 7).unwrap();
        gs.commit_plans(c, P1, &mut || 7).unwrap().unwrap()
    }

    #[test]
    fn t65_pilots_luke_wedge_biggs_and_garven() {
        let c = content();
        let north = FRAC_PI_2;
        let south = -FRAC_PI_2;
        // Luke, nose to nose with an Academy Pilot at Range 1, has no focus
        // token; his defense dice show focus + blank and the ability turns
        // the focus into an evade: two of three hits land, on the shields.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[("lukeskywalker", Pose::new(10.0, 8.0, south), 2)],
        );
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 4, 7]);
        let shot = imperial_shot(&rec);
        assert_eq!(shot.range, 1);
        assert!(shot.defense_faces.contains(&DefenseFace::Evade), "{:?}", shot.defense_faces);
        assert_eq!((shot.shields_lost, shot.hull_lost), (2, 0));
        assert!(rec.events.iter().any(|e| e.contains("Luke Skywalker — focus result to evade")));

        // Wedge at Range 2 of a TIE (agility 3): the defender rolls two dice.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 1.5, north), 2)],
            &[("wedgeantilles", Pose::new(10.0, 9.0, south), 2)],
        );
        let rec = resolve(&c, &mut gs, vec![7; 12]);
        let shot = rebel_shot(&rec);
        assert_eq!((shot.range, shot.defense_faces.len()), (2, 2));
        assert!(rec.events.iter().any(|e| e.contains("Wedge Antilles — defender agility -1")));

        // Biggs and a Rookie both at Range 1 of the TIE, the Rookie the
        // nearer: the TIE may only shoot Biggs.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[
                ("biggsdarklighter", Pose::new(9.0, 8.0, south), 2),
                ("rookiepilot", Pose::new(11.0, 7.5, south), 2),
            ],
        );
        let rec = resolve(&c, &mut gs, vec![7; 30]);
        assert_eq!(imperial_shot(&rec).defender, ShipId(1), "{:?}", rec.attacks);
        let opts = gs.attack_options(&c, 0);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].target, ShipId(1));

        // Garven spends his focus token on the attack; it moves to the
        // Rookie at Range 1 instead of being discarded.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[
                ("garvendreis", Pose::new(9.0, 8.0, south), 2),
                ("rookiepilot", Pose::new(11.0, 8.0, south), 2),
            ],
        );
        gs.ships[1].focus = 1;
        let rec = resolve(
            &c,
            &mut gs,
            vec![4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7],
        );
        let garven = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert!(garven.attacker_focus_spent);
        assert!(
            rec.events
                .iter()
                .any(|e| e.contains("Garven Dreis — spent focus token passed to Red-2")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn lightning_reflexes_spins_and_electronic_baffle_sheds_the_stress() {
        let c = content();
        let (north, south) = (FRAC_PI_2, -FRAC_PI_2);
        let lightning = UpgradeId(125);
        let baffle = UpgradeId(205);
        // A green straight 2, then the switched-on Lightning Reflexes spins
        // the X-Wing to face north, discards the card and stresses it.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 12.0, south), 2)],
        );
        gs.ships[1].upgrades.push(lightning);
        assert_eq!(gs.action_extras(&c, &gs.ships[1]).toggles, vec![lightning]);
        assert_eq!(
            gs.plan_card_use(&c, P1, ShipId(1), UpgradeId(108), true),
            Err(Rejection::NoSuchUpgrade)
        );
        gs.plan_card_use(&c, P1, ShipId(1), lightning, true).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 30]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert!(
            (mv.end.heading - north).abs() < 1e-9
                || (mv.end.heading - north - 2.0 * std::f64::consts::PI).abs() < 1e-9,
            "{}",
            mv.end.heading
        );
        assert!((mv.end.anchor.y - 11.0).abs() < 1e-9, "spun about the base centre: {:?}", mv.end);
        assert!(!gs.ships[1].upgrades.contains(&lightning));
        assert_eq!(gs.ships[1].stress, 1);
        assert!(gs.ships[1].card_uses.is_empty(), "toggles are per round");

        // With Electronic Baffle switched on too, the stress costs a shield.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 12.0, south), 2)],
        );
        gs.ships[1].upgrades.extend([lightning, baffle]);
        gs.plan_card_use(&c, P1, ShipId(1), lightning, true).unwrap();
        gs.plan_card_use(&c, P1, ShipId(1), baffle, true).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 30]);
        assert_eq!((gs.ships[1].stress, gs.ships[1].shields), (0, 2));
        assert!(rec.events.iter().any(|e| e.contains("Electronic Baffle — 1 damage, stress")));
        // Ion tokens received during the round are shed at the end of the
        // Combat phase the same way.
        gs.plan_card_use(&c, P1, ShipId(1), baffle, true).unwrap();
        for id in [0u32, 1] {
            let k = id as usize;
            let (owner, class) = (gs.ships[k].owner, gs.ships[k].class);
            let m = dial_index(&c, class, |m| {
                m.steer == crate::maneuver::Steer::Straight && m.distance == 2
            });
            gs.plan_maneuver(&c, owner, ShipId(id), m).unwrap();
        }
        let mut rolls = scripted(vec![7; 30]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        gs.ships[1].ion = 2;
        run_combat(&c, &mut gs, &mut rolls, |_| unreachable!("one target each"));
        assert_eq!((gs.ships[1].ion, gs.ships[1].shields), (0, 0));
    }

    #[test]
    fn jan_ors_swaps_a_friends_focus_for_evade_and_decoy_swaps_skill() {
        let c = content();
        let (north, south) = (FRAC_PI_2, -FRAC_PI_2);
        let jan = UpgradeId(165);
        let decoy = UpgradeId(120);
        // Red-leader (PS4, Jan Ors on) and Red-2 (PS2, Decoy on, focus
        // action) at Range 1 of each other; the TIE ends at Range 1 of both.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[
                ("redsquadronveteran", Pose::new(8.0, 8.0, south), 2),
                ("bluesquadronnovice", Pose::new(10.0, 8.0, south), 2),
            ],
        );
        gs.ships[1].upgrades.push(jan);
        gs.ships[2].upgrades.push(decoy);
        gs.plan_card_use(&c, P1, ShipId(1), jan, true).unwrap();
        gs.plan_card_use(&c, P1, ShipId(2), decoy, true).unwrap();
        gs.plan_action(&c, P1, ShipId(2), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 40]);
        assert!(
            rec.events
                .iter()
                .any(|e| e.contains("Jan Ors — evade token instead of focus for Red-2")),
            "{:?}",
            rec.events
        );
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(2)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        assert!(
            rec.events
                .iter()
                .any(|e| e.contains("Decoy — swaps pilot skill with Red-leader (4 / 2)")),
            "{:?}",
            rec.events
        );
        // Red-2 now fires first (skill 4), then Red-leader, then the TIE.
        let order: Vec<ShipId> = rec.attacks.iter().map(|a| a.attacker).collect();
        assert_eq!(order, vec![ShipId(2), ShipId(1), ShipId(0)]);
    }

    #[test]
    fn daredevil_red_turn_and_experimental_interface_free_card_action() {
        let c = content();
        let (north, south) = (FRAC_PI_2, -FRAC_PI_2);
        let daredevil = UpgradeId(118);
        // A T-65 (no boost icon) plans the Daredevil turn as its action:
        // it turns 90°, takes a stress and two attack dice (both hits) hit
        // its shields.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[("redsquadronpilot", Pose::new(10.0, 8.0, south), 2)],
        );
        gs.ships[1].upgrades.push(daredevil);
        assert!(gs.action_extras(&c, &gs.ships[1]).daredevil);
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::Boost(BoostDir::Straight)),
            Err(Rejection::ActionNotOnBar)
        );
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Boost(BoostDir::TurnLeft)).unwrap();
        let rec = resolve(&c, &mut gs, vec![0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        let turned = (gs.ships[1].pose.unwrap().heading - south).abs();
        assert!((turned - FRAC_PI_2).abs() < 1e-9, "turned 90°: {turned}");
        assert_eq!((gs.ships[1].stress, gs.ships[1].shields), (1, 0));
        assert!(
            rec.events.iter().any(|e| e.contains("Daredevil — no boost icon: 2 hit(s)")),
            "{:?}",
            rec.events
        );

        // Experimental Interface: Focus, then Marksmanship as a free card
        // action, then a stress token.
        let ei = UpgradeId(82);
        let marksmanship = UpgradeId(108);
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 12.0, south), 2)],
        );
        gs.ships[1].upgrades.extend([ei, marksmanship]);
        assert_eq!(
            gs.action_extras(&c, &gs.ships[1]).second,
            Some(SecondActionKind::CardActionThenStress)
        );
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        assert_eq!(
            gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::Evade)),
            Err(Rejection::SecondActionNotAllowed)
        );
        gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::CardAction(marksmanship)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 30]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(
            mv.second,
            Some((PlannedAction::CardAction(marksmanship), ActionResult::Performed))
        );
        assert_eq!(gs.ships[1].stress, 1);
        assert!(rec.events.iter().any(|e| e.contains("Experimental Interface")));
    }

    #[test]
    fn hyperwave_setup_tokens_intelligence_agent_reads_a_dial_and_snap_shot_fires() {
        let c = content();
        let (north, south) = (FRAC_PI_2, -FRAC_PI_2);
        let scanner = UpgradeId(26);
        let agent = UpgradeId(163);
        let snap = UpgradeId(130);
        // Hyperwave on the first TIE: the second, placed at Range 1, gets a
        // focus token as placement completes.
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let imperial = Squad {
            name: "i".into(),
            faction: crate::ship::Faction::Empire,
            ships: vec![
                SquadShip {
                    pilot: pilot("academypilot"),
                    upgrades: vec![scanner],
                    callsign: String::new(),
                },
                SquadShip {
                    pilot: pilot("academypilot"),
                    upgrades: vec![],
                    callsign: String::new(),
                },
            ],
        };
        let rebel = Squad {
            name: "r".into(),
            faction: crate::ship::Faction::RebelAlliance,
            ships: vec![SquadShip {
                pilot: pilot("redsquadronveteran"),
                upgrades: vec![agent, snap],
                callsign: String::new(),
            }],
        };
        let mut gs =
            GameState::from_squads(board(), &c, &[&imperial, &rebel], &[0, 1], AttackFace::Hit)
                .unwrap();
        gs.place_ship(&c, P0, ShipId(0), Pose::new(8.0, 2.5, north)).unwrap();
        gs.place_ship(&c, P0, ShipId(1), Pose::new(10.0, 2.5, north)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(10.0, 17.5, south)).unwrap();
        assert_eq!((gs.ships[0].focus, gs.ships[1].focus), (0, 1));
        // Stage: TIE #1 ends its straight 2 at Range 1 in the X-Wing's arc,
        // so Snap Shot fires (2 unmodified dice) during the move; the
        // X-Wing still attacks in the Combat phase. Intelligence Agent read
        // TIE #1's dial (Range 2) at the start of the phase.
        gs.ships[0].pose = Some(Pose::new(2.0, 2.5, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 4.0, north));
        gs.ships[2].pose = Some(Pose::new(10.0, 8.0, south));
        let tie_s2 = straight(&c, TIE, 2);
        let xw_s1 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 1
        });
        gs.plan_maneuver(&c, P0, ShipId(0), tie_s2).unwrap();
        gs.plan_maneuver(&c, P0, ShipId(1), tie_s2).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(2), xw_s1).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 40]);
        assert!(
            rec.events.iter().any(
                |e| e.contains("Intelligence Agent — Obsidian-2 has dialed straight 2 (green)")
            ),
            "{:?}",
            rec.events
        );
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.snap_shots.len(), 1, "{:?}", rec.events);
        let shot = &mv.snap_shots[0];
        assert_eq!(
            (shot.attacker, shot.defender, shot.weapon, shot.attack_faces.len()),
            (ShipId(2), ShipId(1), Some(snap), 2)
        );
        assert!(
            rec.attacks.iter().any(|a| a.attacker == ShipId(2) && a.weapon.is_none()),
            "normal attack still happens"
        );
    }

    #[test]
    fn weapons_engineer_holds_two_locks_and_spends_one() {
        let c = content();
        let (north, south) = (FRAC_PI_2, -FRAC_PI_2);
        let engineer = UpgradeId(160);
        // Two TIEs end at Range 1 of the X-Wing, which locks one of them:
        // Weapons Engineer locks the other as well.
        let mut gs = skirmish(
            &c,
            &[
                ("academypilot", Pose::new(8.0, 2.5, north), 2),
                ("academypilot", Pose::new(12.0, 2.5, north), 2),
            ],
            &[("redsquadronveteran", Pose::new(10.0, 8.0, south), 2)],
        );
        gs.ships[2].upgrades.push(engineer);
        gs.plan_action(&c, P1, ShipId(2), PlannedAction::TargetLock(ShipId(0))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 40]);
        assert!(
            rec.events.iter().any(|e| e.contains("Weapons Engineer — second lock on")),
            "{:?}",
            rec.events
        );
        // The attack spent the lock on its target; the other lock stays.
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(2)).unwrap();
        assert!(shot.lock_spent);
        let xw = &gs.ships[2];
        assert!(!xw.locks_on(shot.defender));
        let other = if shot.defender == ShipId(0) { ShipId(1) } else { ShipId(0) };
        assert!(xw.locks_on(other), "{:?} / {:?}", xw.lock, xw.lock2);
        assert!(xw.lock2.is_none(), "the remaining lock moved into the first slot");
        // Without the card a new lock replaces the old one.
        let mut plain = ShipState::new(ShipId(9), P0, TIE, gs.ships[0].pilot, "x".into(), 3, 0);
        plain.take_lock(ShipId(1), false);
        plain.take_lock(ShipId(2), false);
        assert_eq!((plain.lock, plain.lock2), (Some(ShipId(2)), None));
    }

    #[test]
    fn han_solo_hotr_is_placed_last_in_the_open_beyond_range_3() {
        let c = content();
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let imperial = Squad::basic(&c, "i", &[pilot("academypilot")]);
        let rebel = Squad::basic(&c, "r", &[pilot("hansolo_2"), pilot("rookiepilot")]);
        let mut gs =
            GameState::from_squads(board(), &c, &[&imperial, &rebel], &[0, 1], AttackFace::Hit)
                .unwrap();
        assert!(gs.ships[1].late_setup && !gs.ships[2].late_setup);
        // Han waits for everyone else; meanwhile placement stays hidden.
        assert_eq!(
            gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)),
            Err(Rejection::PlaceLast)
        );
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        assert!(gs.snapshot_for(&c, P1)[0].pose.is_none(), "hidden until the late step");
        gs.place_ship(&c, P1, ShipId(2), Pose::new(5.0, 17.5, -FRAC_PI_2)).unwrap();
        // Now the board is public and Han may go anywhere beyond Range 3
        // of the TIE.
        assert!(gs.snapshot_for(&c, P1)[0].pose.is_some());
        assert_eq!(gs.deploy_zones(P1), vec![(0.0, 0.0, 20.0, 20.0)]);
        assert_eq!(
            gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 8.0, -FRAC_PI_2)),
            Err(Rejection::TooCloseToEnemy)
        );
        gs.place_ship(&c, P1, ShipId(1), Pose::new(15.0, 12.0, -FRAC_PI_2)).unwrap();
        assert_eq!(gs.phase, Phase::Planning);
    }

    #[test]
    fn political_escort_shuttle_protect_escape_and_reinforcements() {
        let c = content();
        let mut gs = mission_game(&c, MissionKind::PoliticalEscort, 100);
        // Red Squadron Pilot (0) vs two Academy Pilots (1, 2); the shuttle
        // (3) is the Rebels', already on the board at the centre of their
        // edge, shields 6 at 100 points, and can neither act nor attack.
        assert_eq!(gs.ships.len(), 4);
        let sh = &gs.ships[3];
        assert_eq!((sh.class, sh.owner, sh.shields, sh.hull), (SHUTTLE, P0, 6, 6));
        let pose = sh.pose.unwrap();
        assert!((pose.anchor.x - 10.0).abs() < 1e-9 && pose.anchor.y < mission::R1);
        assert!(gs.action_bar(&c, sh).is_empty());
        assert_eq!(gs.mission.as_ref().unwrap().shuttle, Some(ShipId(3)));
        // Both sides deploy within Range 2 of their edge; the Empire has
        // the initiative on the points tie.
        assert_eq!(gs.deploy_zones(P0), vec![(0.0, 0.0, 20.0, 5.0)]);
        assert_eq!(gs.deploy_zones(P1), vec![(0.0, 15.0, 20.0, 20.0)]);
        assert_ne!(gs.squad_totals[0], gs.squad_totals[1]);
        gs.place_ship(&c, P0, ShipId(0), Pose::new(13.0, 2.0, FRAC_PI_2)).unwrap();
        assert_eq!(
            gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 14.0, -FRAC_PI_2)),
            Err(Rejection::OutOfZone)
        );
        gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 18.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(15.0, 18.0, -FRAC_PI_2)).unwrap();
        assert_eq!(gs.phase, Phase::Planning);
        // Protect is a Rebel-only mission action; it needs Range 1 of the
        // shuttle when performed. The shuttle's dial has speed 0-2 only.
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::Protect),
            Err(Rejection::NotInThisMission)
        );
        assert!(gs.attack_options(&c, 3).is_empty());
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Protect).unwrap();
        // One TIE is lost before the round resolves: the Empire gets an
        // Academy Pilot to place within Range 1 of its edge.
        gs.ships[1].destroyed = true;
        gs.ships[1].hull = 0;
        let rec = fly_all(&c, &mut gs, 2);
        assert!(rec.events.iter().any(|e| e.contains("protect — evade token")), "{:?}", rec.events);
        assert!(rec.events.iter().any(|e| e.contains("reinforcement: Academy Pilot")));
        assert_eq!(gs.ships.len(), 5);
        assert_eq!((gs.ships[4].owner, gs.ships[4].pose, gs.phase), (P1, None, Phase::Placement));
        assert_eq!(gs.deploy_zones(P1), vec![(0.0, 17.5, 20.0, 20.0)]);
        // Mid-game placement is in the open: the Rebel sees the TIE.
        assert!(gs.snapshot_for(&c, P0)[2].pose.is_some());
        assert_eq!(
            gs.place_ship(&c, P1, ShipId(4), Pose::new(10.0, 16.0, -FRAC_PI_2)),
            Err(Rejection::OutOfZone)
        );
        gs.place_ship(&c, P1, ShipId(4), Pose::new(3.0, 19.0, -FRAC_PI_2)).unwrap();
        assert_eq!(gs.phase, Phase::Planning);
        // Losing every Rebel fighter is not an Imperial win: the shuttle
        // must die. Getting the shuttle off the Imperial edge wins.
        gs.ships[0].destroyed = true;
        gs.ships[3].pose = Some(Pose::new(10.0, 18.5, FRAC_PI_2));
        let rec = fly_all(&c, &mut gs, 2);
        let shuttle_move = rec.moves.iter().find(|m| m.ship == ShipId(3)).unwrap();
        assert!(shuttle_move.destroyed && shuttle_move.escaped);
        assert!(rec.events.iter().any(|e| e.contains("Senator: ESCAPED off the north edge")));
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(0)));
        assert!(gs.winner_reason().contains("Political Escort"));

        // Off any other edge the shuttle is destroyed: the Empire wins.
        let mut gs = mission_game(&c, MissionKind::PoliticalEscort, 31);
        assert_eq!(gs.ships[3].shields, 0, "no shields below 100 points");
        gs.place_ship(&c, P0, ShipId(0), Pose::new(13.0, 2.0, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 18.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(15.0, 18.0, -FRAC_PI_2)).unwrap();
        gs.ships[3].pose = Some(Pose::new(1.5, 4.0, std::f64::consts::PI));
        let rec = fly_all(&c, &mut gs, 2);
        let shuttle_move = rec.moves.iter().find(|m| m.ship == ShipId(3)).unwrap();
        assert!(shuttle_move.destroyed && !shuttle_move.escaped);
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(1)));
    }

    #[test]
    fn asteroid_run_disabled_ship_flies_slow_until_round_5_then_escapes() {
        let c = content();
        let mut gs = mission_game(&c, MissionKind::AsteroidRun, 100);
        // Luke (0) vs Night Beast (1) and Mauler Mithel (2).
        assert_eq!(gs.ships[0].class, T65);
        assert_eq!(gs.mission.as_ref().unwrap().disabled, Some(ShipId(0)));
        // Rebels deploy in the middle band, the Empire at either edge.
        assert_eq!(gs.deploy_zones(P0), vec![(0.0, 7.5, 20.0, 12.5)]);
        assert_eq!(gs.deploy_zones(P1), vec![(0.0, 17.5, 20.0, 20.0), (0.0, 0.0, 20.0, 2.5)]);
        gs.obstacles.push(Obstacle {
            id: 0,
            kind: ObstacleKind::Asteroid,
            center: Vec2::new(5.0, 10.0),
            heading: 0.0,
            shape: 0,
        });
        assert_eq!(
            gs.place_ship(&c, P0, ShipId(0), Pose::new(7.0, 10.5, FRAC_PI_2)),
            Err(Rejection::TooCloseToObstacle)
        );
        gs.place_ship(&c, P0, ShipId(0), Pose::new(14.0, 10.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 19.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(15.0, 1.5, FRAC_PI_2)).unwrap();
        assert_eq!(gs.phase, Phase::Planning);
        assert_eq!(
            gs.plan_maneuver(&c, P0, ShipId(0), straight(&c, T65, 3)),
            Err(Rejection::ShipDisabled)
        );
        gs.plan_maneuver(&c, P0, ShipId(0), straight(&c, T65, 2)).unwrap();
        // Fleeing before Round 5 destroys the disabled ship: Imperial win.
        gs.ships[0].pose = Some(Pose::new(10.0, 11.5, -FRAC_PI_2));
        gs.turn = 4;
        gs.ships[0].pose = Some(Pose::new(10.0, 1.5, -FRAC_PI_2));
        let rec = fly_all(&c, &mut gs, 2);
        let luke = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert!(luke.destroyed && !luke.escaped);
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(1)));

        // From Round 5 the ship is repaired and may leave by either
        // player's edge alive.
        let mut gs = mission_game(&c, MissionKind::AsteroidRun, 100);
        gs.place_ship(&c, P0, ShipId(0), Pose::new(14.0, 10.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 19.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(15.0, 19.0, -FRAC_PI_2)).unwrap();
        gs.turn = mission::REPAIR_ROUND;
        gs.plan_maneuver(&c, P0, ShipId(0), straight(&c, T65, 3)).unwrap();
        gs.ships[0].plan = None;
        gs.ships[0].pose = Some(Pose::new(10.0, 1.5, -FRAC_PI_2));
        let rec = fly_all(&c, &mut gs, 2);
        assert!(rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap().escaped);
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(0)));
    }

    #[test]
    fn dark_whispers_scans_satellites_and_carries_them_home() {
        let c = content();
        let mut gs = mission_game(&c, MissionKind::DarkWhispers, 100);
        // Red Squadron Pilot (0) vs Black Squadron Pilot (1), Obsidian (2).
        let sats = gs.mission.as_ref().unwrap().satellites.clone();
        assert_eq!(sats.len(), 4);
        assert!(sats.iter().all(|s| s.on_board() && s.pos.y < mission::R3));
        assert_eq!(gs.deploy_zones(P0), vec![(0.0, 0.0, 20.0, 2.5)]);
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 1.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 19.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(15.0, 19.0, -FRAC_PI_2)).unwrap();
        // The Black Squadron TIE ends its move on satellite 1 and scans it
        // instead of shooting; everyone else is out of range.
        let s0 = sats[0].pos;
        gs.ships[1].pose = Some(Pose::new(s0.x, s0.y - 1.5, FRAC_PI_2));
        gs.ships[0].pose = Some(Pose::new(2.0, 13.0, FRAC_PI_2));
        let rec = fly_all(&c, &mut gs, 2);
        assert!(rec.events.iter().any(|e| e.contains("scans satellite 1")), "{:?}", rec.events);
        assert!(rec.attacks.is_empty());
        assert_eq!(gs.ships[1].satellites, 1);
        assert_eq!(gs.mission.as_ref().unwrap().satellites[0].holder, Some(ShipId(1)));
        // Leaving with satellites still on the board destroys the ship
        // and returns its token to the supply; the game goes on.
        gs.ships[1].pose = Some(Pose::new(10.0, 18.5, FRAC_PI_2));
        let rec = fly_all(&c, &mut gs, 2);
        let m1 = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert!(m1.destroyed && !m1.escaped);
        assert!(rec.events.iter().any(|e| e.contains("satellite 1 returns to the supply")));
        let sat0 = &gs.mission.as_ref().unwrap().satellites[0];
        assert!(sat0.supply && sat0.holder.is_none());
        assert_eq!((gs.phase, gs.winner), (Phase::Planning, None));
        // Once every satellite has left the board, an Imperial ship
        // carrying one flees off the Imperial edge alive: Imperial win.
        {
            let m = gs.mission.as_mut().unwrap();
            for s in &mut m.satellites[1..] {
                s.holder = Some(ShipId(2));
            }
        }
        gs.ships[2].satellites = 3;
        gs.ships[2].pose = Some(Pose::new(10.0, 18.5, FRAC_PI_2));
        let rec = fly_all(&c, &mut gs, 2);
        assert!(rec.moves.iter().find(|m| m.ship == ShipId(2)).unwrap().escaped);
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(1)));

        // Every token back in the supply is a Rebel win; so is a Rebel
        // loss answered by a Rookie Pilot reinforcement at the Rebel edge.
        let mut gs = mission_game(&c, MissionKind::DarkWhispers, 60);
        assert_eq!(gs.mission.as_ref().unwrap().satellites.len(), 2);
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 1.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(5.0, 19.0, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(15.0, 19.0, -FRAC_PI_2)).unwrap();
        gs.ships[0].destroyed = true;
        gs.ships[0].hull = 0;
        let rec = fly_all(&c, &mut gs, 2);
        assert!(rec.events.iter().any(|e| e.contains("reinforcement: Rookie Pilot")));
        assert_eq!((gs.ships[3].owner, gs.ships[3].class, gs.phase), (P0, T65, Phase::Placement));
        assert_eq!(gs.deploy_zones(P0), vec![(0.0, 0.0, 20.0, 2.5)]);
        gs.place_ship(&c, P0, ShipId(3), Pose::new(10.0, 1.5, FRAC_PI_2)).unwrap();
        for s in &mut gs.mission.as_mut().unwrap().satellites {
            s.supply = true;
        }
        fly_all(&c, &mut gs, 2);
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(0)));
    }

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
        assert_eq!(
            movement_order(&ships, &[1, 0]),
            vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]
        );
        assert_eq!(combat_order(&ships, &[1, 0]), vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]);
    }

    #[test]
    fn team_game_seats_share_a_side_and_win_together() {
        use crate::squad::Squad;
        let c = content();
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let tie = Squad::basic(&c, "i", &[pilot("academypilot")]);
        let xw = Squad::basic(&c, "r", &[pilot("bluesquadronnovice")]);
        let mut gs = GameState::from_squads(
            board(),
            &c,
            &[&tie, &tie, &xw, &xw],
            &[0, 0, 1, 1],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        assert_eq!(gs.sides(), 2);
        assert!(gs.allied(P0, PlayerId(1)) && !gs.allied(P0, PlayerId(2)));
        assert_eq!((gs.seat_of(PlayerId(1)), gs.seat_of(PlayerId(3))), (Seat::South, Seat::North));
        // Initiative: the Imperial side (2 x 12 = 24) is cheaper than the Rebel one.
        assert_eq!(gs.initiative, P0);
        assert_eq!(gs.seat_ranks(), vec![0, 1, 2, 3]);
        // Teammates deploy on the same edge, side by side.
        gs.place_ship(&c, P0, ShipId(0), Pose::new(6.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, PlayerId(1), ShipId(1), Pose::new(14.0, 2.5, FRAC_PI_2)).unwrap();
        assert_eq!(
            gs.place_ship(&c, PlayerId(1), ShipId(1), Pose::new(6.0, 2.5, FRAC_PI_2)),
            Err(Rejection::OverlapsShip),
            "not onto a teammate"
        );
        // Squad callsigns stay distinct across four seats.
        let names: Vec<&str> = gs.ships.iter().map(|s| s.callsign.as_str()).collect();
        assert_eq!(names, vec!["Obsidian-leader", "Onyx-leader", "Red-leader", "Gold-leader"]);
        // One Rebel resigning leaves the game on; the second ends it.
        assert_eq!(gs.resign(PlayerId(2)), None);
        assert_eq!(gs.phase, Phase::Placement);
        assert_eq!(gs.resign(PlayerId(3)), Some(0));
        assert_eq!(gs.winner, Some(0));
    }

    #[test]
    fn free_for_all_deploys_each_side_on_its_own_edge() {
        use crate::squad::Squad;
        let c = content();
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let tie = Squad::basic(&c, "i", &[pilot("academypilot")]);
        let xw = Squad::basic(&c, "r", &[pilot("bluesquadronnovice")]);
        let mut gs = GameState::from_squads(
            board(),
            &c,
            &[&tie, &xw, &tie],
            &[],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        assert_eq!(gs.teams, vec![0, 1, 2]);
        assert!(!gs.allied(P0, PlayerId(2)), "same faction, different sides");
        assert_eq!(gs.seat_of(PlayerId(2)), Seat::East);
        assert_eq!(
            gs.place_ship(&c, PlayerId(2), ShipId(2), Pose::new(10.0, 10.0, std::f64::consts::PI)),
            Err(Rejection::OutOfZone)
        );
        gs.place_ship(&c, PlayerId(2), ShipId(2), Pose::new(18.5, 10.0, std::f64::consts::PI))
            .unwrap();
        // Two sides gone: the last one standing wins.
        gs.ships[0].destroyed = true;
        gs.ships[1].destroyed = true;
        gs.check_victory();
        assert_eq!((gs.phase, gs.winner), (Phase::GameOver, Some(2)));
    }

    #[test]
    fn initiative_setup_rules() {
        use crate::dice::AttackFace;
        // Lower squad total takes it outright — die irrelevant.
        assert_eq!(initiative_seat(&[12, 24], AttackFace::Blank), 0);
        assert_eq!(initiative_seat(&[48, 24], AttackFace::Hit), 1);
        // Tie: seat 0 rolls. Hit/Crit keeps, Focus/Blank hands over.
        assert_eq!(initiative_seat(&[24, 24], AttackFace::Hit), 0);
        assert_eq!(initiative_seat(&[24, 24], AttackFace::Crit), 0);
        assert_eq!(initiative_seat(&[24, 24], AttackFace::Focus), 1);
        assert_eq!(initiative_seat(&[24, 24], AttackFace::Blank), 1);
        // Three sides: the lowest total wins outright; a tie between the
        // last two hands over to the second of them on a blank.
        assert_eq!(initiative_seat(&[30, 20, 25], AttackFace::Blank), 1);
        assert_eq!(initiative_seat(&[30, 20, 20], AttackFace::Blank), 2);
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
        assert_eq!(gs.committed, vec![false, false]);
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
        assert_eq!(gs.winner, Some(1));
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
        let s2 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 2);
        gs.plan_maneuver(&c, P0, ShipId(0), s3).unwrap();
        gs.plan_maneuver(&c, P0, ShipId(1), s2).unwrap();
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
        let mut gs = GameState::from_squads(
            board(),
            &c,
            &[&imperial, &rebel],
            &[0, 1],
            crate::dice::AttackFace::Hit,
        )
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
            GameState::from_squads(board(), c, &[&a, &b], &[0, 1], crate::dice::AttackFace::Hit)
                .unwrap();
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
            GameState::from_squads(board(), c, &[&a, &b], &[0, 1], crate::dice::AttackFace::Hit)
                .unwrap();
        let all: Vec<_> = imperial.iter().chain(rebel.iter()).collect();
        for (k, _) in all.iter().enumerate() {
            let player = if k < imperial.len() { P0 } else { P1 };
            // Any pose is allowed for staging: place legally in the zone
            // (spread along the edge), then move the ship where asked.
            let legal = if k < imperial.len() {
                Pose::new(2.0 + 3.0 * k as f64, 2.5, FRAC_PI_2)
            } else {
                Pose::new(2.0 + 3.0 * k as f64, 17.5, -FRAC_PI_2)
            };
            gs.place_ship(c, player, ShipId(k as u32), legal).unwrap();
        }
        for (k, (_, pose, _)) in all.iter().enumerate() {
            gs.ships[k].pose = Some(*pose);
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
            &[("academypilot", Pose::new(10.0, 1.5, north), 2)],
            &[("outerrimsmuggler", Pose::new(10.0, 17.5, -north), 1)],
        );
        // The Outer Rim Smuggler's card prints 2/1/6/4 on a 3/1/8/5 hull.
        assert_eq!((gs.ships[1].hull, gs.ships[1].shields), (6, 4));
        assert_eq!(gs.printed(&c, &gs.ships[1]).attack, 2);
        // Stage both heading north with the TIE 6 units behind the
        // freighter (range 3, outside the freighter's forward arc).
        gs.ships[0].pose = Some(Pose::new(10.0, 3.0, north));
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
            &[("academypilot", Pose::new(10.0, 1.5, north), 2)],
            &[("goldsquadronpilot", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[1].upgrades.push(ion_turret);
        gs.ships[0].pose = Some(Pose::new(10.0, 3.0, north));
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
            &[("academypilot", Pose::new(10.0, 1.5, north), 2)],
            &[("goldsquadronpilot", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[1].upgrades.push(dorsal);
        gs.ships[0].pose = Some(Pose::new(10.0, 3.0, north));
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
            &[("academypilot", Pose::new(10.0, 1.5, north), 2)],
            &[("prototypepilot", Pose::new(10.0, 17.5, -north), 2)],
        );
        gs.ships[1].upgrades.push(rockets);
        gs.ships[0].pose = Some(Pose::new(10.0, 3.0, north)); // nose 5 after straight 2
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

    #[test]
    fn ion_cannon_turret_deals_one_damage_and_the_ionized_ship_drifts_next_round() {
        let c = content();
        let ion_turret = UpgradeId(10);
        let north = FRAC_PI_2;
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 1.5, north), 2)],
            &[("goldsquadronpilot", Pose::new(10.0, 17.5, -north), 1)],
        );
        gs.ships[1].upgrades.push(ion_turret);
        gs.ships[0].pose = Some(Pose::new(10.0, 3.0, north));
        gs.ships[1].pose = Some(Pose::new(10.0, 9.0, north));
        // Turret [Hit, Hit, Hit] vs 3 blanks: exactly 1 damage lands and
        // the TIE is ionized. The TIE (behind, out of arc) has no shot.
        let mut rolls = scripted(vec![0, 0, 0, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(ion_turret));
        let shot = &rec.attacks[0];
        assert_eq!((shot.weapon, shot.hits, shot.crits), (Some(ion_turret), 1, 0));
        assert_eq!((gs.ships[0].hull, gs.ships[0].ion), (2, 1));

        // Next round the TIE dials a straight 5 but drifts a white 1.
        let s5 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 5);
        gs.plan_maneuver(&c, P0, ShipId(0), s5).unwrap();
        let s1 = {
            let set = c.ships.class(gs.ships[1].class).unwrap().maneuver_set;
            c.dials
                .set(set)
                .unwrap()
                .maneuvers
                .iter()
                .position(|m| m.steer == crate::maneuver::Steer::Straight && m.distance == 1)
                .unwrap() as u8
        };
        gs.plan_maneuver(&c, P1, ShipId(1), s1).unwrap();
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        let act = gs.commit_plans_begin(&c, P1, &mut rolls).unwrap().unwrap();
        let tie = gs.ships[0].pose.unwrap();
        assert!((tie.anchor.y - 6.0).abs() < 1e-9, "drifted 1 from y=5: {}", tie.anchor.y);
        assert_eq!((gs.ships[0].ion, gs.ships[0].stress), (0, 0));
        assert!(act.events.iter().any(|e| e.contains("ionized — drifts")), "{:?}", act.events);
    }

    #[test]
    fn cluster_missiles_attack_twice_spending_the_lock_once_and_discarding_after() {
        let c = content();
        let cluster = UpgradeId(141); // 3 dice, R1-2, spend lock, twice
        let north = FRAC_PI_2;
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, north), 5)],
            &[("greensquadronpilot", Pose::new(10.0, 17.5, -north), 4)],
        );
        gs.ships[1].upgrades.push(cluster);
        gs.ships[1].lock = Some(ShipId(0));
        gs.ships[1].pose = Some(Pose::new(10.0, 14.5, -north)); // nose 10.5 vs 7.5: R2
        // First salvo [Hit, Blank, Blank] vs 3 blanks: 1 damage. Second
        // [Hit, Hit, Blank] vs 3 blanks: 2 more — the TIE is destroyed.
        let mut rolls = scripted(vec![0, 7, 7, 7, 7, 7, 0, 0, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(cluster));
        assert_eq!(rec.attacks.len(), 2, "{:?}", rec.attacks);
        assert_eq!((rec.attacks[0].weapon, rec.attacks[0].hits), (Some(cluster), 1));
        assert_eq!((rec.attacks[1].weapon, rec.attacks[1].hits), (Some(cluster), 2));
        assert!(rec.attacks[0].lock_spent && !rec.attacks[1].lock_spent);
        assert!(rec.attacks[1].defender_destroyed);
        assert!(!gs.ships[1].upgrades.contains(&cluster));
        assert_eq!(rec.events.iter().filter(|e| e.contains("discarded (fired)")).count(), 1);
        assert!(rec.events.iter().any(|e| e.contains("fires Cluster Missiles again")));
    }

    #[test]
    fn assault_missiles_splash_and_flechette_torpedoes_stress() {
        let c = content();
        let assault = UpgradeId(143); // 4 dice, R2-3
        let north = FRAC_PI_2;
        // Two TIEs abreast at Range 1 of each other; the X-Wing (with a
        // lock on the first) fires first at PS2 vs PS1.
        let mut gs = skirmish(
            &c,
            &[
                ("academypilot", Pose::new(9.0, 2.5, north), 5),
                ("academypilot", Pose::new(11.0, 2.5, north), 5),
            ],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -north), 4)],
        );
        gs.ships[2].upgrades.push(assault);
        gs.ships[2].lock = Some(ShipId(0));
        let mut rolls = scripted(vec![0, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(assault));
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(2)).unwrap();
        assert!(shot.defender_destroyed, "{shot:?}");
        assert_eq!(gs.ships[1].hull, 2, "wingman splashed");
        assert!(rec.events.iter().any(|e| e.contains("splash")), "{:?}", rec.events);

        // Flechette Torpedoes stress a hull-3 TIE even on a miss.
        let flechette = UpgradeId(3);
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.ships[1].upgrades.push(flechette);
        gs.ships[1].lock = Some(ShipId(0));
        let mut rolls = scripted(vec![7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(flechette));
        assert_eq!(rec.attacks[0].hits + rec.attacks[0].crits, 0);
        assert_eq!(gs.ships[0].stress, 1);
    }

    #[test]
    fn advanced_homing_missiles_deal_a_faceup_card_through_the_shields() {
        let c = content();
        let adv_homing = UpgradeId(146); // 3 dice, Range 2 only, lock kept
        let north = FRAC_PI_2;
        let mut gs = skirmish(
            &c,
            &[("tempestsquadronpilot", Pose::new(10.0, 2.5, north), 5)],
            &[("greensquadronpilot", Pose::new(10.0, 17.5, -north), 4)],
        );
        gs.ships[1].upgrades.push(adv_homing);
        gs.ships[1].lock = Some(ShipId(0));
        gs.ships[1].pose = Some(Pose::new(10.0, 16.5, -north)); // nose 12.5 vs 7.5: R2
        // [Hit, Blank, Blank]; the kept lock rerolls the blanks (blank
        // again) vs 3 blanks → one faceup card: shields untouched, hull
        // 3→2, crit drawn (4 = Damaged Sensor Array).
        let mut rolls = scripted(vec![0, 7, 7, 7, 7, 7, 7, 7, 4, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(adv_homing));
        let shot = &rec.attacks[0];
        assert_eq!((shot.weapon, shot.range, shot.hits, shot.crits), (Some(adv_homing), 2, 0, 1));
        assert_eq!((shot.shields_lost, shot.hull_lost, shot.crits_to_hull), (0, 1, 1));
        assert_eq!((gs.ships[0].shields, gs.ships[0].hull), (2, 2));
        assert_eq!(gs.ships[0].crits, vec![CritEffect::DamagedSensorArray]);
        assert!(shot.lock_spent, "the lock survives the launch and is spent on the reroll");
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
        let s2 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 2);
        // Turn 1 (Range 2): every attack 2 hits, every defense 3 blanks.
        // Turn 2 (Range 1, three attack dice): 3 hits vs 3 blanks.
        let mut rolls =
            scripted(vec![0, 0, 7, 7, 7, 0, 0, 7, 7, 7, 0, 0, 0, 7, 7, 7, 0, 0, 0, 7, 7, 7]);
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
        gs.plan_maneuver(&c, P0, ShipId(0), s2).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s2).unwrap();
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.attacks.len(), 2, "destroyed ship of equal skill still fires");
        assert!(rec.attacks.iter().all(|a| a.defender_destroyed));
        assert_eq!(gs.phase, Phase::GameOver);
        assert_eq!(gs.winner, Some(0), "initiative wins the mutual kill");
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
        assert_eq!(gs.resign(P1), Some(0));
        assert_eq!(gs.phase, Phase::GameOver);
        assert_eq!(gs.winner, Some(0));
    }

    // ---------------- Bombs ----------------

    /// A Gamma Squadron Pilot (TIE Bomber) at the south edge carrying
    /// `card`, an X-Wing far north; both fly straight `bomber_dist`/1.
    fn bomber_duel(c: &Content, card: UpgradeId, bomber_dist: u8) -> GameState {
        let mut gs = skirmish(
            c,
            &[("gammasquadronpilot", Pose::new(10.0, 3.0, FRAC_PI_2), bomber_dist)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].upgrades.push(card);
        gs
    }

    #[test]
    fn seismic_charge_drops_behind_and_blows_at_end_of_activation() {
        let c = content();
        let seismic = UpgradeId(181);
        let mut gs = bomber_duel(&c, seismic, 1);
        gs.plan_bomb(&c, P0, ShipId(0), Some(seismic)).unwrap();
        // Mines cannot be planned as dial-reveal bombs, nor unequipped cards.
        assert_eq!(
            gs.plan_bomb(&c, P0, ShipId(0), Some(UpgradeId(182))),
            Err(Rejection::NoSuchUpgrade)
        );
        let mut blanks = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut blanks).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut blanks).unwrap().unwrap();
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.dropped_before.len(), 1);
        let token = mv.dropped_before[0];
        assert_eq!(token.kind, BombKind::Seismic);
        // Rear edge at y=2, template to y=1: token front-center there.
        assert!((token.pose.anchor.y - 1.0).abs() < 1e-9, "{:?}", token.pose);
        // A straight 1 leaves the bomber's rear 2 units from the token —
        // inside Range 1 — so it eats its own charge; the X-Wing is safe.
        assert_eq!(rec.detonations.len(), 1);
        assert_eq!(rec.detonations[0].hits.len(), 1);
        assert_eq!(rec.detonations[0].hits[0].ship, ShipId(0));
        assert_eq!(rec.detonations[0].hits[0].damage, 1);
        assert_eq!(gs.ships[0].hull, 5);
        assert_eq!(gs.ships[1].hull, 3);
        assert!(gs.bombs.is_empty(), "reveal bombs never linger");
        assert!(!gs.ships[0].upgrades.contains(&seismic), "card discarded");
        assert!(rec.events.iter().any(|e| e.contains("drops Seismic Charges")), "{:?}", rec.events);
    }

    #[test]
    fn proton_bomb_deals_a_faceup_card_and_a_fast_bomber_escapes_it() {
        let c = content();
        let proton = UpgradeId(180);
        let mut gs = bomber_duel(&c, proton, 1);
        gs.ships[0].shields = 2; // pretend shields: the faceup card ignores them
        gs.plan_bomb(&c, P0, ShipId(0), Some(proton)).unwrap();
        // crit::draw(9) = Stunned Pilot: no immediate extra damage.
        let mut rolls = scripted(vec![9]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        assert_eq!(rec.detonations[0].hits[0].crits, 1);
        assert_eq!(gs.ships[0].shields, 2);
        assert_eq!(gs.ships[0].hull, 5);
        assert!(gs.ships[0].crits.contains(&CritEffect::StunnedPilot));

        // Straight 2 puts the rear 3 units away: out of Range 1.
        let mut gs = bomber_duel(&c, proton, 2);
        gs.plan_bomb(&c, P0, ShipId(0), Some(proton)).unwrap();
        let mut blanks = scripted(vec![7]);
        gs.commit_plans(&c, P0, &mut blanks).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut blanks).unwrap().unwrap();
        assert_eq!(rec.detonations.len(), 1);
        assert!(rec.detonations[0].hits.is_empty());
        assert_eq!(gs.ships[0].hull, 6);
    }

    /// Turn 1: the bomber drops `card` as its action; turn 2: the X-Wing
    /// is moved by hand to just south of the token and flies straight 2
    /// through it. Returns the state and the X-Wing's turn-2 move record.
    fn cross_mine(c: &Content, card: UpgradeId, rolls: Vec<u8>) -> (GameState, MoveRecord) {
        let mut gs = bomber_duel(c, card, 1);
        gs.plan_action(c, P0, ShipId(0), PlannedAction::DropMine(card)).unwrap();
        let mut blanks = scripted(vec![7]);
        gs.commit_plans(c, P0, &mut blanks).unwrap();
        let rec = gs.commit_plans(c, P1, &mut blanks).unwrap().unwrap();
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        assert!(!mv.dropped_after.is_empty());
        assert!(!gs.ships[0].upgrades.contains(&card));
        assert_eq!(gs.bombs.len(), mv.dropped_after.len());
        // Bomber now front y=4; token behind it at y 1..2. Park the
        // bomber out of the way and aim the X-Wing at the token.
        gs.ships[0].pose = Some(Pose::new(4.0, 12.0, FRAC_PI_2));
        gs.ships[1].pose = Some(Pose::new(10.0, 1.0, FRAC_PI_2));
        let s1 = dial_index(c, gs.ships[0].class, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 1
        });
        let s2 = dial_index(c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 2
        });
        gs.plan_maneuver(c, P0, ShipId(0), s1).unwrap();
        gs.plan_maneuver(c, P1, ShipId(1), s2).unwrap();
        gs.plan_action(c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let mut rolls = scripted(rolls);
        gs.commit_plans(c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(c, P1, &mut rolls).unwrap().unwrap();
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap().clone();
        (gs, mv)
    }

    #[test]
    fn proximity_mine_rolls_three_dice_on_the_ship_that_crosses_it() {
        let c = content();
        // hit, crit, blank; everything after is blank.
        let (gs, mv) = cross_mine(&c, UpgradeId(182), vec![0, 3, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(mv.mines_hit.len(), 1);
        let hit = mv.mines_hit[0].hits[0];
        assert_eq!(hit.ship, ShipId(1));
        assert_eq!(hit.damage, 2);
        assert_eq!(hit.crits, 0, "shields absorb the critical");
        assert_eq!(gs.ships[1].shields, 1);
        assert_eq!(gs.ships[1].hull, 3);
        assert_eq!(mv.action_result, ActionResult::Performed, "mines don't cost the action");
        assert!(gs.bombs.is_empty(), "a detonated mine is removed");
    }

    #[test]
    fn conner_net_ionizes_and_denies_the_action() {
        let c = content();
        let (gs, mv) = cross_mine(&c, UpgradeId(185), vec![7]);
        let hit = mv.mines_hit[0].hits[0];
        assert_eq!((hit.damage, hit.ion), (1, 2));
        assert_eq!(gs.ships[1].shields, 2);
        assert_eq!(gs.ships[1].ion, 2);
        assert_eq!(mv.action_result, ActionResult::SkippedNetted);
        assert_eq!(gs.ships[1].focus, 0);
    }

    #[test]
    fn cluster_mines_drop_three_tokens_each_firing_two_dice() {
        let c = content();
        let (gs, mv) = cross_mine(&c, UpgradeId(184), vec![0, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        // The X-Wing (1 wide) flies up the middle: the center token for
        // sure; the outer two only graze its edges (touching counts as
        // overlap, so either outcome of the rounding is accepted).
        assert!(!mv.mines_hit.is_empty());
        assert_eq!(mv.mines_hit[0].hits[0].damage, 2, "two hits from the first token");
        assert_eq!(gs.bombs.len() + mv.mines_hit.len(), 3, "untriggered mines stay armed");
    }

    // ---------------- Talent cards ----------------

    fn rebel_shot(rec: &TurnRecords) -> &AttackRecord {
        rec.attacks.iter().find(|a| a.attacker == ShipId(1)).expect("the X-Wing fired")
    }

    /// A Red Squadron Veteran (PS4, fires first) carrying `card` against
    /// an Obsidian Squadron Pilot (PS3), nose to nose at Range 3: the
    /// X-Wing rolls 3 attack dice, the TIE 4 defense dice.
    fn talent_duel(c: &Content, card: UpgradeId) -> GameState {
        let mut gs = duel(c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[1].upgrades.push(card);
        gs
    }

    fn resolve(c: &Content, gs: &mut GameState, rolls: Vec<u8>) -> TurnRecords {
        let mut rolls = scripted(rolls);
        gs.commit_plans(c, P0, &mut rolls).unwrap();
        gs.commit_plans(c, P1, &mut rolls).unwrap().unwrap()
    }

    #[test]
    fn mercenary_copilot_crits_at_range_3_and_sensor_jammer_blunts_a_hit() {
        let c = content();
        // X-Wing rolls [Hit, Blank, Blank] at Range 3: the hit becomes a crit.
        let mut gs = talent_duel(&c, UpgradeId(158));
        let rec = resolve(&c, &mut gs, vec![0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!((rebel_shot(&rec).hits, rebel_shot(&rec).crits), (0, 1));
        // Sensor Jammer on the TIE: the same hit becomes a focus result
        // the X-Wing has no token to convert.
        let mut gs = duel(&c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[0].upgrades.push(UpgradeId(202));
        let rec = resolve(&c, &mut gs, vec![0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 0);
        assert!(rebel_shot(&rec).attack_faces.contains(&AttackFace::Focus));
        assert!(rec.events.iter().any(|e| e.contains("Sensor Jammer")), "{:?}", rec.events);
    }

    #[test]
    fn accuracy_corrector_turns_a_whiff_into_two_hits() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(203));
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
        assert_eq!(gs.ships[0].hull, 1);
    }

    #[test]
    fn weapons_guidance_spends_a_spare_focus_on_a_blank() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(20));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 1);
        assert!(rebel_shot(&rec).attacker_focus_spent);
    }

    #[test]
    fn han_solo_crew_spends_the_lock_to_convert_all_focus_results() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(152));
        gs.ships[1].lock = Some(ShipId(0));
        // [Focus, Focus, Blank] with no focus token: Han beats rerolling.
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
        assert!(rebel_shot(&rec).lock_spent);
        assert_eq!(gs.ships[1].lock, None);
    }

    #[test]
    fn tactician_stresses_a_range_2_target_and_fire_control_locks_it() {
        let c = content();
        // X-Wing ends at anchor y=10.5: base 10.5..11.5 vs TIE nose 7.5 → Range 2.
        let mut gs = duel_at(
            &c,
            "obsidiansquadronpilot",
            "redsquadronveteran",
            Pose::new(10.0, 14.5, -FRAC_PI_2),
        );
        gs.ships[1].upgrades.push(UpgradeId(162));
        gs.ships[1].upgrades.push(UpgradeId(200));
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        assert_eq!(rebel_shot(&rec).range, 2);
        assert_eq!(gs.ships[0].stress, 1, "{:?}", rec.events);
        assert_eq!(gs.ships[1].lock, Some(ShipId(0)));
    }

    #[test]
    fn comm_relay_keeps_one_evade_and_r4d6_trades_hits_for_stress() {
        let c = content();
        let mut gs = duel(&c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[0].upgrades.push(UpgradeId(21));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Evade).unwrap();
        resolve(&c, &mut gs, vec![7; 16]);
        assert_eq!(gs.ships[0].evade, 1, "Comm Relay keeps the token through the End phase");

        // R4-D6 on the shieldless TIE: three hits → one cancelled for a stress.
        let mut gs = duel(&c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[0].upgrades.push(UpgradeId(47));
        let rec = resolve(&c, &mut gs, vec![0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
        assert_eq!(gs.ships[0].stress, 1);
        assert_eq!(gs.ships[0].hull, 1);
    }

    #[test]
    fn lightweight_frame_and_mara_jade_at_range_1() {
        let c = content();
        // X-Wing anchor y=9.0 after its straight 4: base 9..10 vs TIE nose 7.5 → Range 1.
        let stage = |c: &Content| {
            duel_at(
                c,
                "obsidiansquadronpilot",
                "redsquadronveteran",
                Pose::new(10.0, 13.0, -FRAC_PI_2),
            )
        };
        let mut gs = stage(&c);
        gs.ships[0].upgrades.push(UpgradeId(79));
        // X-Wing: 4 dice, all hits. TIE: 3 dice blank, the extra die an evade.
        let rec = resolve(&c, &mut gs, vec![0, 0, 0, 0, 7, 7, 7, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).range, 1);
        assert_eq!(rebel_shot(&rec).defense_faces.len(), 4);
        assert_eq!(rebel_shot(&rec).hits, 3);

        let mut gs = stage(&c);
        gs.ships[0].upgrades.push(UpgradeId(179));
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        assert_eq!(gs.ships[1].stress, 1, "{:?}", rec.events);
        assert!(rec.events.iter().any(|e| e.contains("Mara Jade")));
    }

    #[test]
    fn long_range_scanners_and_st321_change_who_can_be_locked() {
        let c = content();
        // Range 3 after the moves: Long-Range Scanners allow the lock.
        let mut gs = talent_duel(&c, UpgradeId(83));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(0))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        // Range 2: refused.
        let mut gs = duel_at(
            &c,
            "obsidiansquadronpilot",
            "redsquadronveteran",
            Pose::new(10.0, 14.5, -FRAC_PI_2),
        );
        gs.ships[1].upgrades.push(UpgradeId(83));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(0))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Failed);
        // ST-321: a lock from the far corner of the board.
        let mut gs = duel_at(
            &c,
            "obsidiansquadronpilot",
            "redsquadronveteran",
            Pose::new(2.0, 17.5, -FRAC_PI_2),
        );
        gs.ships[1].upgrades.push(UpgradeId(97));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::TargetLock(ShipId(0))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed, "{:?}", rec.events);
        assert_eq!(gs.ships[1].lock, Some(ShipId(0)));
    }

    #[test]
    fn targeting_astromech_locks_after_a_red_maneuver() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(55));
        let k4 =
            dial_index(&c, XWING, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 4);
        gs.plan_maneuver(&c, P1, ShipId(1), k4).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        assert_eq!(gs.ships[1].stress, 1);
        assert_eq!(gs.ships[1].lock, Some(ShipId(0)), "{:?}", rec.events);
    }

    #[test]
    fn primed_thrusters_let_a_stressed_tie_barrel_roll() {
        let c = content();
        let mut gs = duel(&c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[0].upgrades.push(UpgradeId(23));
        let k3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(0), k3).unwrap();
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRoll(Side::Left)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed, "{:?}", rec.events);
        assert_eq!(gs.ships[0].stress, 1);
    }

    #[test]
    fn advanced_sensors_take_the_focus_before_the_maneuver() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(201));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.pre, Some((PlannedAction::Focus, ActionResult::Performed)));
        assert_eq!(mv.action, PlannedAction::Pass);
        assert!(rec.events.iter().any(|e| e.contains("Advanced Sensors")), "{:?}", rec.events);
    }

    #[test]
    fn gunner_and_luke_fire_again_after_a_miss() {
        let c = content();
        // Gunner: the miss is followed by a primary attack that lands three hits.
        let mut gs = talent_duel(&c, UpgradeId(157));
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7]);
        let shots: Vec<&AttackRecord> =
            rec.attacks.iter().filter(|a| a.attacker == ShipId(1)).collect();
        assert_eq!(shots.len(), 2, "{:?}", rec.events);
        assert_eq!(shots[1].hits, 3);
        assert!(gs.ships[0].destroyed);
        // Luke: the second attack turns a focus result into a hit for free.
        let mut gs = talent_duel(&c, UpgradeId(151));
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shots: Vec<&AttackRecord> =
            rec.attacks.iter().filter(|a| a.attacker == ShipId(1)).collect();
        assert_eq!(shots.len(), 2, "{:?}", rec.events);
        assert_eq!(shots[1].hits, 1);
        assert!(rec.events.iter().any(|e| e.contains("Luke Skywalker")), "{:?}", rec.events);
    }

    #[test]
    fn extra_munitions_keep_the_bomb_card_and_black_one_shakes_a_lock() {
        let c = content();
        let seismic = UpgradeId(181);
        let mut gs = bomber_duel(&c, seismic, 1);
        gs.ships[0].upgrades.push(UpgradeId(6));
        gs.ships[0].ordnance = vec![seismic];
        gs.plan_bomb(&c, P0, ShipId(0), Some(seismic)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        assert!(gs.ships[0].upgrades.contains(&seismic), "card kept: {:?}", rec.events);
        assert!(gs.ships[0].ordnance.is_empty());
        assert_eq!(rec.detonations.len(), 1);

        // Black One on the TIE: its barrel roll removes the X-Wing's lock on it.
        let mut gs = duel(&c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[0].upgrades.push(UpgradeId(90));
        gs.ships[1].lock = Some(ShipId(0));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRoll(Side::Left)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        assert_eq!(gs.ships[1].lock, None, "{:?}", rec.events);
    }

    #[test]
    fn tractor_beam_tractors_instead_of_damaging() {
        let c = content();
        let beam = UpgradeId(195);
        let mut gs = talent_duel(&c, beam);
        // X-Wing first: 3 beam dice all hits, TIE defends 4 blanks.
        let mut rolls = scripted(vec![0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let rec = run_combat(&c, &mut gs, &mut rolls, prefer(beam));
        let shot = rebel_shot(&rec);
        assert_eq!(shot.weapon, Some(beam));
        assert_eq!((shot.hits, shot.crits), (0, 0));
        assert_eq!(gs.ships[0].hull, 3, "no damage");
        // The tractor token is gone with the End phase; agility was 2 meanwhile.
        assert!(rec.events.iter().any(|e| e.contains("tractored")), "{:?}", rec.events);
        assert_eq!(gs.ships[0].tractor, 0);
    }

    #[test]
    fn btl_a4_title_fires_the_turret_after_the_primary_weapon() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[("goldsquadronpilot", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        // Staged by hand so the turret (Range 1-2) reaches the TIE.
        gs.ships[1].pose = Some(Pose::new(10.0, 14.5, -FRAC_PI_2));
        gs.ships[1].upgrades.push(UpgradeId(92));
        gs.ships[1].upgrades.push(UpgradeId(10));
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        let shots: Vec<&AttackRecord> =
            rec.attacks.iter().filter(|a| a.attacker == ShipId(1)).collect();
        assert_eq!(shots.len(), 2, "{:?}", rec.events);
        assert_eq!(shots[0].weapon, None);
        assert_eq!(shots[1].weapon, Some(UpgradeId(10)));
    }

    #[test]
    fn navigator_rotates_a_dial_that_would_leave_the_board() {
        let c = content();
        // Straight 4 south from y=3.5 would fly off; straight 3 stays on.
        let mut gs = duel_at(
            &c,
            "obsidiansquadronpilot",
            "redsquadronveteran",
            Pose::new(10.0, 3.5, -FRAC_PI_2),
        );
        gs.ships[1].upgrades.push(UpgradeId(159));
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.maneuver.distance, 3, "{:?}", rec.events);
        assert!(!mv.destroyed);
        assert!(rec.events.iter().any(|e| e.contains("Navigator")), "{:?}", rec.events);
    }

    #[test]
    fn leia_is_discarded_to_fly_a_friends_red_maneuver_as_white() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[
                ("redsquadronveteran", Pose::new(6.0, 17.5, -FRAC_PI_2), 4),
                ("bluesquadronnovice", Pose::new(14.0, 17.5, -FRAC_PI_2), 1),
            ],
        );
        let leia = UpgradeId(153);
        gs.ships[2].upgrades.push(leia);
        let k4 =
            dial_index(&c, XWING, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 4);
        gs.plan_maneuver(&c, P1, ShipId(1), k4).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        assert_eq!(gs.ships[1].stress, 0, "{:?}", rec.events);
        assert!(!gs.ships[2].upgrades.contains(&leia), "Leia discarded");
        assert!(!gs.white_reds[1], "cleared with the End phase");
    }

    #[test]
    fn fleet_officer_lando_and_r5d8_card_actions() {
        let c = content();
        // Fleet Officer: two TIEs abreast; the officer's ship and its
        // wingman get a focus, the officer's ship a stress.
        let mut gs = skirmish(
            &c,
            &[
                ("obsidiansquadronpilot", Pose::new(9.0, 2.5, FRAC_PI_2), 2),
                ("academypilot", Pose::new(11.0, 2.5, FRAC_PI_2), 2),
            ],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].upgrades.push(UpgradeId(174));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::CardAction(UpgradeId(174))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        assert_eq!(gs.ships[0].stress, 1, "{:?}", rec.events);
        assert!(
            rec.events.iter().any(|e| e.contains("focus token to Obsidian-2")),
            "{:?}",
            rec.events
        );

        // Lando: two defense dice [Focus, Evade] → one token each.
        let mut gs = talent_duel(&c, UpgradeId(166));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(166))).unwrap();
        let rec = resolve(&c, &mut gs, vec![4, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert!(rec.events.iter().any(|e| e.contains("1 focus, 1 evade")), "{:?}", rec.events);

        // R5-D8: one facedown card, an evade result repairs it.
        let mut gs = talent_duel(&c, UpgradeId(49));
        gs.ships[1].hull = 2;
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(49))).unwrap();
        let rec = resolve(&c, &mut gs, vec![0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(gs.ships[1].hull, 3, "{:?}", rec.events);
    }

    #[test]
    fn rey_stores_an_unspent_focus_for_the_next_round() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(167));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 16]);
        assert_eq!(gs.ships[1].stored_focus, 1, "{:?}", rec.events);
        assert_eq!(gs.ships[1].focus, 0);
    }

    #[test]
    fn horton_salm_rerolls_blanks_at_range_3() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[("hortonsalm", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        // Horton (PS8) fires first: [Blank, Blank] rerolled into two hits.
        let rec = resolve(&c, &mut gs, vec![7, 7, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).range, 3);
        assert_eq!(rebel_shot(&rec).hits, 2, "{:?}", rec.events);
    }

    #[test]
    fn rey_and_han_solo_reroll_their_dice() {
        let c = content();
        let stage = |c: &Content, pilot: &str| {
            skirmish(
                c,
                &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
                &[(pilot, Pose::new(10.0, 17.5, -FRAC_PI_2), 3)],
            )
        };
        // Rey: two of three blanks rerolled (the TIE is in her arc).
        let mut gs = stage(&c, "rey");
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2, "{:?}", rec.events);
        // Han Solo: a whiff is rerolled whole.
        let mut gs = stage(&c, "hansolo");
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 3, "{:?}", rec.events);
    }

    #[test]
    fn kir_kanos_spends_an_evade_for_a_hit_at_range_3() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("kirkanos", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        // X-Wing staged so its straight 4 ends exactly at Range 3 of Kir's nose (y=5).
        gs.ships[1].pose = Some(Pose::new(10.0, 16.5, -FRAC_PI_2));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Evade).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        assert_eq!(imperial_shot(&rec).range, 3);
        assert_eq!(imperial_shot(&rec).hits, 1, "{:?}", rec.events);
        assert_eq!(gs.ships[0].evade, 0);
    }

    #[test]
    fn carnor_jax_and_zertik_strom_punish_ships_at_range_1() {
        let c = content();
        // Carnor Jax: the X-Wing (PS4) acts before Carnor (PS8) moves, so
        // it must end at Range 1 of his starting base (nose y=3).
        let mut gs = skirmish(
            &c,
            &[("carnorjax", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[1].pose = Some(Pose::new(10.0, 9.5, -FRAC_PI_2));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(rebel_shot(&rec).range, 1);
        assert_eq!(mv.action_result, ActionResult::Failed, "{:?}", rec.events);
        // Zertik Strom: the X-Wing's Range 1 shot rolls 3 dice, not 4.
        let mut gs = skirmish(
            &c,
            &[("zertikstrom", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[1].pose = Some(Pose::new(10.0, 11.0, -FRAC_PI_2));
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        assert_eq!(rebel_shot(&rec).range, 1);
        assert_eq!(rebel_shot(&rec).attack_faces.len(), 3, "{:?}", rec.events);
    }

    #[test]
    fn major_rhymer_fires_torpedoes_at_range_1() {
        let c = content();
        let torps = UpgradeId(1);
        let mut gs = skirmish(
            &c,
            &[("majorrhymer", Pose::new(10.0, 3.0, FRAC_PI_2), 1)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[1].pose = Some(Pose::new(10.0, 10.0, -FRAC_PI_2));
        gs.ships[0].upgrades.push(torps);
        gs.ships[0].lock = Some(ShipId(1));
        let mut rolls = scripted(vec![7; 24]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let CombatStep::NeedTarget(p) = gs.combat_step(&c, &mut rolls).unwrap() else {
            panic!("expected a weapon choice")
        };
        assert_eq!(p.attacker, ShipId(0));
        let torp = p.options.iter().find(|o| o.weapon == Some(torps)).expect("torpedo offered");
        assert_eq!(torp.range, 1);
    }

    #[test]
    fn wampa_trades_a_lone_crit_for_a_facedown_card() {
        let c = content();
        let mut gs = duel(&c, "wampa", "bluesquadronnovice");
        // Wampa (PS4) fires first: [Crit, Blank] → cancelled, one hull straight away.
        let rec = resolve(&c, &mut gs, vec![3, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!((imperial_shot(&rec).hits, imperial_shot(&rec).crits), (0, 0));
        assert_eq!(imperial_shot(&rec).hull_lost, 1);
        assert_eq!((gs.ships[1].hull, gs.ships[1].shields), (2, 3), "{:?}", rec.events);
    }

    #[test]
    fn maarek_stele_picks_the_worst_of_three_damage_cards() {
        let c = content();
        let mut gs = duel(&c, "maarekstele", "bluesquadronnovice");
        gs.ships[1].shields = 0;
        // Maarek (PS7) fires first: [Crit, Blank], no evades; three cards
        // drawn from raws 9, 1 and 2 — the most severe is chosen.
        let rec = resolve(&c, &mut gs, vec![3, 7, 7, 7, 7, 9, 1, 2, 7, 7, 7, 7, 7, 7, 7, 7]);
        let expected = [crit::draw(9), crit::draw(1), crit::draw(2)]
            .into_iter()
            .max_by_key(|e| e.severity())
            .unwrap();
        assert!(
            rec.events.iter().any(|e| e.contains(&format!("chooses {}", expected.name()))),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn captain_yorr_absorbs_a_friends_stress() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[
                ("captainyorr", Pose::new(10.0, 3.0, FRAC_PI_2), 1),
                // PS7: moves after Yorr, whose green straight 1 comes first.
                ("maulermithel", Pose::new(13.0, 2.5, FRAC_PI_2), 2),
            ],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        let k3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::KTurn && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(1), k3).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        assert_eq!(gs.ships[1].stress, 0, "{:?}", rec.events);
        assert_eq!(gs.ships[0].stress, 1);
    }

    #[test]
    fn captain_kagi_draws_enemy_target_locks() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[
                ("obsidiansquadronpilot", Pose::new(6.0, 2.5, FRAC_PI_2), 5),
                ("captainkagi", Pose::new(14.0, 3.0, FRAC_PI_2), 1),
            ],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[2].pose = Some(Pose::new(12.0, 11.5, -FRAC_PI_2));
        gs.plan_action(&c, P1, ShipId(2), PlannedAction::TargetLock(ShipId(0))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 30]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(2)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed, "{:?}", rec.events);
        // The lock went on Kagi: the X-Wing shoots him and spends it.
        let shot = rec.attacks.iter().find(|a| a.attacker == ShipId(2)).expect("X-Wing fired");
        assert_eq!(shot.defender, ShipId(1), "{:?}", rec.events);
        assert!(shot.lock_spent);
    }

    #[test]
    fn fels_wrath_fires_back_at_zero_hull_then_dies() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("felswrath", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("hortonsalm", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[1].pose = Some(Pose::new(10.0, 16.5, -FRAC_PI_2));
        gs.ships[0].hull = 1;
        // Horton (PS8) first: two hits through four blank defense dice.
        let rec = resolve(&c, &mut gs, vec![0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert!(
            rec.attacks.iter().any(|a| a.attacker == ShipId(0)),
            "still fires: {:?}",
            rec.events
        );
        assert!(gs.ships[0].destroyed, "gone with the End phase");
        assert!(rec.events.iter().any(|e| e.contains("Fel\'s Wrath")), "{:?}", rec.events);
    }

    #[test]
    fn juno_and_tetran_change_speed_to_stay_on_the_board() {
        let c = content();
        // Juno flies south from the edge: straight 3 would leave, straight 2 stays.
        let mut gs = skirmish(
            &c,
            &[("junoeclipse", Pose::new(10.0, 2.0, -FRAC_PI_2), 3)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].pose = Some(Pose::new(10.0, 2.5, -FRAC_PI_2));
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.maneuver.distance, 2, "{:?}", rec.events);
        assert!(!mv.destroyed);
        // Tetran: a Koiogran 5 from y=16 would leave; speed 3 is the nearest that stays.
        let mut gs = skirmish(
            &c,
            &[("tetrancowall", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].pose = Some(Pose::new(10.0, 16.0, FRAC_PI_2));
        gs.ships[1].pose = Some(Pose::new(3.0, 17.5, -FRAC_PI_2));
        let k5 = dial_index(&c, gs.ships[0].class, |m| {
            m.steer == crate::maneuver::Steer::KTurn && m.distance == 5
        });
        gs.plan_maneuver(&c, P0, ShipId(0), k5).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.maneuver.distance, 3, "{:?}", rec.events);
        assert!(!mv.destroyed);
    }

    #[test]
    fn chewbacca_attacks_when_a_friend_is_destroyed() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[
                ("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 4),
                ("chewbacca_2", Pose::new(6.0, 17.5, -FRAC_PI_2), 3),
            ],
        );
        gs.ships[1].hull = 1;
        gs.ships[1].shields = 0;
        // Chewbacca (PS5) blanks; the TIE (PS3) kills the T-70; Chewbacca fires again.
        let rec = resolve(
            &c,
            &mut gs,
            vec![7, 7, 7, 7, 7, 7, 7, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7],
        );
        assert!(gs.ships[1].destroyed, "{:?}", rec.events);
        let chewie: Vec<&AttackRecord> =
            rec.attacks.iter().filter(|a| a.attacker == ShipId(2)).collect();
        assert_eq!(chewie.len(), 2, "{:?}", rec.events);
    }

    #[test]
    fn alozen_jendon_and_colzet_manage_target_locks() {
        let c = content();
        // Alozen locks the X-Wing at Range 1 when combat starts.
        let mut gs = skirmish(
            &c,
            &[("commanderalozen", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[1].pose = Some(Pose::new(10.0, 11.0, -FRAC_PI_2));
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        // Locked at combat start, then spent rerolling his blanks.
        assert!(imperial_shot(&rec).lock_spent, "{:?}", rec.events);

        // Jendon hands his lock to the TIE beside him.
        let mut gs = skirmish(
            &c,
            &[
                ("coloneljendon", Pose::new(10.0, 3.0, FRAC_PI_2), 1),
                ("academypilot", Pose::new(13.0, 2.5, FRAC_PI_2), 2),
            ],
            &[("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].lock = Some(ShipId(2));
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        assert_eq!(gs.ships[1].lock, Some(ShipId(2)), "{:?}", rec.events);
        assert_eq!(gs.ships[0].lock, None);

        // Colzet spends his lock at the End phase to flip a facedown card
        // (the X-Wing is staged out of range so no attack spends it first).
        let mut gs =
            duel_at(&c, "lieutenantcolzet", "bluesquadronnovice", Pose::new(2.0, 17.5, -FRAC_PI_2));
        gs.ships[0].lock = Some(ShipId(1));
        gs.ships[1].shields = 0;
        gs.ships[1].hull = 2;
        let rec = resolve(&c, &mut gs, vec![9, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert!(rec.attacks.is_empty(), "{:?}", rec.attacks);
        assert_eq!(gs.ships[0].lock, None, "{:?}", rec.events);
        assert_eq!(gs.ships[1].crits, vec![CritEffect::StunnedPilot], "{:?}", rec.events);
    }

    #[test]
    fn deathfire_drops_a_mine_on_reveal_and_tomax_keeps_crack_shot() {
        let c = content();
        let mines = UpgradeId(182);
        let mut gs = skirmish(
            &c,
            &[("deathfire", Pose::new(10.0, 3.0, FRAC_PI_2), 1)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].upgrades.push(mines);
        gs.plan_bomb(&c, P0, ShipId(0), Some(mines)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.dropped_before.len(), 1, "{:?}", rec.events);
        assert_eq!(mv.dropped_before[0].kind, BombKind::ProximityMine);

        // Tomax Bren (PS8): Crack Shot cancels the lone evade and stays equipped.
        let crack = UpgradeId(105);
        let mut gs = duel(&c, "tomaxbren", "bluesquadronnovice");
        gs.ships[0].upgrades.push(crack);
        let rec = resolve(&c, &mut gs, vec![0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(imperial_shot(&rec).hits, 2, "{:?}", rec.events);
        assert!(gs.ships[0].upgrades.contains(&crack), "{:?}", rec.events);
    }

    #[test]
    fn autothrusters_turn_a_blank_into_an_evade_at_range_3() {
        let c = content();
        let mut gs = duel(&c, "obsidiansquadronpilot", "redsquadronveteran");
        gs.ships[0].upgrades.push(UpgradeId(75));
        let rec = resolve(&c, &mut gs, vec![0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).range, 3);
        assert_eq!(rebel_shot(&rec).hits, 2, "{:?}", rec.events);
    }

    #[test]
    fn targeting_synchronizer_shares_a_friends_lock_for_ordnance() {
        let c = content();
        let torps = UpgradeId(1);
        let mut gs = skirmish(
            &c,
            &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[
                ("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4),
                ("bluesquadronnovice", Pose::new(13.0, 17.5, -FRAC_PI_2), 1),
            ],
        );
        gs.ships[1].upgrades.push(torps);
        gs.ships[2].upgrades.push(UpgradeId(25));
        gs.ships[2].lock = Some(ShipId(0));
        let mut rolls = scripted(vec![7; 30]);
        gs.commit_plans_begin(&c, P0, &mut rolls).unwrap();
        gs.commit_plans_begin(&c, P1, &mut rolls).unwrap();
        let CombatStep::NeedTarget(p) = gs.combat_step(&c, &mut rolls).unwrap() else {
            panic!("expected a weapon choice for the X-Wing")
        };
        assert_eq!(p.attacker, ShipId(1));
        assert!(p.options.iter().any(|o| o.weapon == Some(torps)), "{:?}", p.options);
        let rec = gs.declare_target(&c, P1, ShipId(0), Some(torps), &mut rolls).unwrap();
        assert!(!rec.lock_spent && !rec.attacker_focus_spent);
        assert_eq!(gs.ships[2].lock, Some(ShipId(0)), "the friend keeps the lock");
    }

    #[test]
    fn predator_rerolls_one_die_or_two_against_low_skill() {
        let c = content();
        let predator = UpgradeId(101);
        // [Blank, Blank, Hit] → one blank rerolled into a hit.
        let mut gs = talent_duel(&c, predator);
        let rec = resolve(&c, &mut gs, vec![7, 7, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
        assert!(rec.events.iter().any(|e| e.contains("Predator")), "{:?}", rec.events);
        // Against an Academy Pilot (PS1): both blanks rerolled.
        let mut gs = duel(&c, "academypilot", "redsquadronveteran");
        gs.ships[1].upgrades.push(predator);
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
    }

    #[test]
    fn lone_wolf_rerolls_a_blank_only_without_friends_within_range_2() {
        let c = content();
        let lone_wolf = UpgradeId(104);
        let mut gs = talent_duel(&c, lone_wolf);
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 1);
        assert!(rec.events.iter().any(|e| e.contains("Lone Wolf")), "{:?}", rec.events);

        // A wingman at Range 1 silences it.
        let mut gs = skirmish(
            &c,
            &[("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[
                ("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4),
                ("bluesquadronnovice", Pose::new(12.0, 17.5, -FRAC_PI_2), 4),
            ],
        );
        gs.ships[1].upgrades.push(lone_wolf);
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(rebel_shot(&rec).hits, 0);
        assert!(!rec.events.iter().any(|e| e.contains("Lone Wolf")), "{:?}", rec.events);
    }

    #[test]
    fn wired_rerolls_focus_results_while_stressed() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(100));
        gs.ships[1].stress = 1;
        // [Eye, Eye, Hit] with no focus token: both eyes rerolled to hits.
        let rec = resolve(&c, &mut gs, vec![4, 4, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 3);
        assert!(rec.events.iter().any(|e| e.contains("Wired")), "{:?}", rec.events);
    }

    #[test]
    fn expertise_converts_focus_results_without_spending_the_token() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(123));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shot = rebel_shot(&rec);
        assert_eq!(shot.hits, 2);
        assert!(!shot.attacker_focus_spent);
        assert!(rec.events.iter().any(|e| e.contains("Expertise")), "{:?}", rec.events);
    }

    #[test]
    fn calculation_buys_a_crit_when_exactly_one_focus_result_shows() {
        let c = content();
        let calc = UpgradeId(116);
        let mut gs = talent_duel(&c, calc);
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![4, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shot = rebel_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (1, 1));
        assert!(shot.attacker_focus_spent);
        assert!(rec.events.iter().any(|e| e.contains("Calculation")), "{:?}", rec.events);
        // Two focus results: the plain spend (two hits) is better.
        let mut gs = talent_duel(&c, calc);
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shot = rebel_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (2, 0));
        assert!(!rec.events.iter().any(|e| e.contains("Calculation")), "{:?}", rec.events);
    }

    #[test]
    fn opportunist_takes_a_stress_for_an_extra_die_against_a_tokenless_defender() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(126));
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(rebel_shot(&rec).attack_faces.len(), 4);
        assert_eq!(gs.ships[1].stress, 1);
        assert!(rec.events.iter().any(|e| e.contains("Opportunist")), "{:?}", rec.events);
        // A stressed attacker cannot use it.
        let mut gs = talent_duel(&c, UpgradeId(126));
        gs.ships[1].stress = 1;
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(rebel_shot(&rec).attack_faces.len(), 3);
    }

    #[test]
    fn outmaneuver_strips_an_agility_from_a_defender_that_cannot_see_the_attacker() {
        let c = content();
        let outmaneuver = UpgradeId(127);
        // Nose to nose: the TIE has the X-Wing in arc, full 4 dice at Range 3.
        let mut gs = talent_duel(&c, outmaneuver);
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(rebel_shot(&rec).defense_faces.len(), 4);
        // From behind (Range 2): agility 3 → 2, no range bonus.
        let mut gs = duel_at(
            &c,
            "obsidiansquadronpilot",
            "redsquadronveteran",
            Pose::new(10.0, -1.0, FRAC_PI_2),
        );
        gs.ships[1].upgrades.push(outmaneuver);
        let rec = resolve(&c, &mut gs, vec![7]);
        let shot = rebel_shot(&rec);
        assert_eq!((shot.range, shot.defense_faces.len()), (2, 2));
        assert!(rec.events.iter().any(|e| e.contains("Outmaneuver")), "{:?}", rec.events);
    }

    #[test]
    fn crack_shot_cancels_one_evade_and_is_discarded() {
        let c = content();
        let crack_shot = UpgradeId(105);
        let mut gs = talent_duel(&c, crack_shot);
        // [Hit, Hit, Blank] against [Evade, Blank, Blank, Blank].
        let rec = resolve(&c, &mut gs, vec![0, 0, 7, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
        assert!(!gs.ships[1].upgrades.contains(&crack_shot));
        assert!(rec.events.iter().any(|e| e.contains("Crack Shot")), "{:?}", rec.events);
    }

    #[test]
    fn juke_turns_an_evade_into_a_focus_while_holding_an_evade_token() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(106));
        gs.ships[1].evade = 1;
        let rec = resolve(&c, &mut gs, vec![0, 0, 7, 0, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 2);
        assert!(rec.events.iter().any(|e| e.contains("Juke")), "{:?}", rec.events);
    }

    #[test]
    fn sensor_cluster_spends_focus_to_turn_a_blank_into_an_evade() {
        let c = content();
        // Offset from the TIE's nose: dead ahead would be its bullseye
        // lane, where the defender may not spend tokens at all.
        let mut gs = duel_at(
            &c,
            "obsidiansquadronpilot",
            "redsquadronveteran",
            Pose::new(11.5, 17.5, -FRAC_PI_2),
        );
        gs.ships[1].upgrades.push(UpgradeId(24));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // X-Wing attack blanks, TIE defense blanks, TIE attack [Hit, Blank],
        // X-Wing defense all blank → the focus buys one evade.
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 0, 7, 7, 7, 7, 7]);
        let shot = imperial_shot(&rec);
        assert_eq!(shot.hits, 0);
        assert!(shot.defender_focus_spent);
        assert!(rec.events.iter().any(|e| e.contains("Sensor Cluster")), "{:?}", rec.events);
    }

    // ---------------- Token, stress and movement abilities ----------------

    #[test]
    fn night_beast_gets_a_free_focus_after_a_green_maneuver() {
        let c = content();
        // Straight 3 is green on the TIE dial; the X-Wing starts closer so
        // the two still end at Range 3.
        let mut gs =
            duel_at(&c, "nightbeast", "bluesquadronnovice", Pose::new(10.0, 16.0, -FRAC_PI_2));
        let s3 =
            dial_index(&c, TIE, |m| m.steer == crate::maneuver::Steer::Straight && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(0), s3).unwrap();
        // Night Beast (PS5) fires first: [Eye, Eye] spent with the free focus.
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shot = imperial_shot(&rec);
        assert!(shot.attacker_focus_spent);
        assert_eq!(shot.hits, 2);
        assert!(rec.events.iter().any(|e| e.contains("free focus")), "{:?}", rec.events);
    }

    #[test]
    fn red_ace_gains_one_evade_per_round_on_the_first_shield_lost() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[
                ("academypilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5),
                ("academypilot", Pose::new(12.0, 2.5, FRAC_PI_2), 5),
            ],
            &[("redace", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        // Red Ace (PS6) fires blanks; TIE 0 lands two hits (two shields
        // gone, one evade token); TIE 1's single hit meets that evade.
        let rolls = vec![7, 7, 7, 7, 7, 7, 7, 0, 0, 7, 7, 7, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7];
        let rec = resolve(&c, &mut gs, rolls);
        let second = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert!(second.evade_spent);
        assert_eq!(second.hits, 0);
        assert_eq!(gs.ships[2].shields, 1);
        assert_eq!(rec.events.iter().filter(|e| e.contains("first shield lost")).count(), 1);
    }

    #[test]
    fn epsilon_leader_and_wingman_clear_stress_when_combat_starts() {
        let c = content();
        let wingman = UpgradeId(134);
        let mut gs = skirmish(
            &c,
            &[
                ("epsilonleader", Pose::new(10.0, 2.5, FRAC_PI_2), 5),
                ("zetasquadronpilot", Pose::new(11.5, 2.5, FRAC_PI_2), 5),
            ],
            &[
                ("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4),
                ("bluesquadronnovice", Pose::new(11.5, 17.5, -FRAC_PI_2), 4),
            ],
        );
        gs.ships[2].upgrades.push(wingman);
        gs.ships[1].stress = 1;
        gs.ships[3].stress = 1;
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(gs.ships[1].stress, 0);
        assert_eq!(gs.ships[3].stress, 0);
        assert!(rec.events.iter().any(|e| e.contains("Wingman")), "{:?}", rec.events);
        assert!(
            rec.events.iter().any(|e| e.contains("stress removed from Obsidian-2")),
            "{:?}",
            rec.events
        );
    }

    #[test]
    fn stress_riders_nien_nunb_soontir_fel_and_cool_hand() {
        let c = content();
        // Nien Nunb facing a TIE at Range 1: the stress is discarded.
        let mut gs = duel(&c, "academypilot", "niennunb");
        gs.ships[0].pose = Some(Pose::new(10.0, 7.5, FRAC_PI_2));
        gs.ships[1].pose = Some(Pose::new(10.0, 9.5, -FRAC_PI_2));
        let mut ev = Vec::new();
        gs.gain_stress(&c, 1, &mut ev);
        assert_eq!(gs.ships[1].stress, 0);
        assert!(ev.iter().any(|e| e.contains("stress discarded")), "{ev:?}");
        // Facing away: the TIE is behind, the stress stays.
        gs.ships[1].pose = Some(Pose::new(10.0, 9.5, FRAC_PI_2));
        gs.gain_stress(&c, 1, &mut ev);
        assert_eq!(gs.ships[1].stress, 1);

        // Soontir Fel takes a focus token with every stress.
        let mut gs = duel(&c, "soontirfel", "bluesquadronnovice");
        gs.gain_stress(&c, 0, &mut ev);
        assert_eq!((gs.ships[0].stress, gs.ships[0].focus), (1, 1));

        // Cool Hand: discarded for a focus token.
        let mut gs = talent_duel(&c, UpgradeId(117));
        gs.gain_stress(&c, 1, &mut ev);
        assert_eq!((gs.ships[1].stress, gs.ships[1].focus), (1, 1));
        assert!(!gs.ships[1].upgrades.contains(&UpgradeId(117)));
    }

    #[test]
    fn a_wing_aces_tycho_acts_while_stressed_and_gemmer_dodges_up_close() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[("tychocelchu", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.ships[1].stress = 1;
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);

        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[("gemmersojan", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        assert_eq!(gs.agility(&c, &gs.ships[1]), 3);
        gs.ships[0].pose = Some(Pose::new(10.0, 7.5, FRAC_PI_2));
        gs.ships[1].pose = Some(Pose::new(10.0, 9.5, -FRAC_PI_2));
        assert_eq!(gs.agility(&c, &gs.ships[1]), 4);
    }

    #[test]
    fn epsilon_ace_flies_at_skill_12_until_damaged() {
        let c = content();
        let mut gs = duel(&c, "epsilonace", "bluesquadronnovice");
        assert_eq!(gs.effective_skill(&c, &gs.ships[0]), 12);
        gs.ships[0].hull -= 1;
        assert_eq!(gs.effective_skill(&c, &gs.ships[0]), 4);
    }

    #[test]
    fn chaser_gains_a_focus_when_a_friend_at_range_1_spends_one() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[
                ("obsidiansquadronpilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5),
                ("chaser", Pose::new(11.5, 2.5, FRAC_PI_2), 5),
            ],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        // Obsidian [Eye, Eye] spends its focus → Chaser gets one and spends
        // it on its own [Eye, Blank].
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let chaser = rec.attacks.iter().find(|a| a.attacker == ShipId(1)).unwrap();
        assert!(chaser.attacker_focus_spent);
        assert_eq!(chaser.hits, 1);
        assert!(rec.events.iter().any(|e| e.contains("friend spent one")), "{:?}", rec.events);
    }

    #[test]
    fn cards_and_pilots_recolour_maneuvers() {
        let c = content();
        use crate::maneuver::Steer;
        // R2 Astromech greens a white speed-2 straight (hand-built: every
        // dial in the data already prints those green), not a speed 3.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        let white = |steer, distance| Maneuver { steer, distance, difficulty: Difficulty::Normal };
        assert_eq!(gs.maneuver_difficulty(&c, 1, &white(Steer::Straight, 2)).0, Difficulty::Normal);
        gs.ships[1].upgrades.push(UpgradeId(41));
        assert_eq!(gs.maneuver_difficulty(&c, 1, &white(Steer::Straight, 2)).0, Difficulty::Easy);
        assert_eq!(gs.maneuver_difficulty(&c, 1, &white(Steer::Straight, 3)).0, Difficulty::Normal);

        // Twin Ion Engine Mk. II greens the TIE's white bank 3.
        let bank = white(Steer::BankLeft, 3);
        assert_eq!(gs.maneuver_difficulty(&c, 0, &bank).0, Difficulty::Normal);
        gs.ships[0].upgrades.push(UpgradeId(78));
        assert_eq!(gs.maneuver_difficulty(&c, 0, &bank).0, Difficulty::Easy);

        // Ello Asty: Tallon Rolls are white while unstressed only.
        let mut gs = duel(&c, "academypilot", "elloasty");
        let tallon = c.dials.set(c.ships.class(XWING).unwrap().maneuver_set).unwrap().maneuvers
            [dial_index(&c, XWING, |m| m.steer == Steer::TallonLeft) as usize];
        assert_eq!(tallon.difficulty, Difficulty::Hard);
        assert_eq!(gs.maneuver_difficulty(&c, 1, &tallon).0, Difficulty::Normal);
        gs.ships[1].stress = 1;
        assert_eq!(gs.maneuver_difficulty(&c, 1, &tallon).0, Difficulty::Hard);

        // Adrenaline Rush: a TIE's red K-turn flown white, card discarded.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.ships[0].upgrades.push(UpgradeId(114));
        let kturn = dial_index(&c, TIE, |m| m.steer == Steer::KTurn && m.distance == 3);
        gs.plan_maneuver(&c, P0, ShipId(0), kturn).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(gs.ships[0].stress, 0);
        assert!(!gs.ships[0].upgrades.contains(&UpgradeId(114)));
        assert!(rec.events.iter().any(|e| e.contains("Adrenaline Rush")), "{:?}", rec.events);
    }

    #[test]
    fn chewbacca_and_determination_neutralise_faceup_cards() {
        let c = content();
        let mut ev = Vec::new();
        let mut gs = duel(&c, "academypilot", "chewbacca");
        let mut rolls = scripted(vec![7]);
        gs.apply_crit_effect(&c, 1, CritEffect::DirectHit, &mut rolls, &mut ev);
        assert!(gs.ships[1].crits.is_empty());
        assert!(ev.iter().any(|e| e.contains("flipped facedown")), "{ev:?}");

        let mut gs = talent_duel(&c, UpgradeId(109));
        gs.apply_crit_effect(&c, 1, CritEffect::StunnedPilot, &mut rolls, &mut ev);
        assert!(gs.ships[1].crits.is_empty(), "Pilot card discarded");
        gs.apply_crit_effect(&c, 1, CritEffect::ConsoleFire, &mut rolls, &mut ev);
        assert_eq!(gs.ships[1].crits, vec![CritEffect::ConsoleFire], "Ship cards still attach");
    }

    // ---------------- Defender rerolls and damage-card riders ----------------

    #[test]
    fn elusiveness_takes_a_stress_to_reroll_the_attackers_best_die() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(113));
        // X-Wing blanks, TIE blanks, TIE [Hit, Crit] → the crit is rerolled blank.
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 0, 3, 7, 7, 7, 7, 7, 7]);
        let shot = imperial_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (1, 0));
        assert_eq!(gs.ships[1].stress, 1);
        assert!(rec.events.iter().any(|e| e.contains("Elusiveness")), "{:?}", rec.events);
    }

    #[test]
    fn r7_astromech_spends_its_lock_to_reroll_every_hit() {
        let c = content();
        // Mauler Mithel (PS7) fires first into an X-Wing that has him locked.
        let mut gs = duel(&c, "maulermithel", "redsquadronveteran");
        gs.ships[1].upgrades.push(UpgradeId(53));
        gs.ships[1].lock = Some(ShipId(0));
        let rec = resolve(&c, &mut gs, vec![0, 3, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shot = imperial_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (0, 0));
        assert_eq!(gs.ships[1].lock, None);
        assert!(rec.events.iter().any(|e| e.contains("R7 Astromech")), "{:?}", rec.events);
    }

    #[test]
    fn draw_their_fire_takes_a_crit_meant_for_a_friend() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("maulermithel", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[
                ("redsquadronveteran", Pose::new(10.0, 17.5, -FRAC_PI_2), 4),
                ("bluesquadronnovice", Pose::new(11.5, 17.5, -FRAC_PI_2), 4),
            ],
        );
        gs.ships[1].upgrades.push(UpgradeId(121));
        gs.ships[2].shields = 0;
        gs.ships[0].lock = Some(ShipId(2)); // fires at the novice
        // [Crit, Crit] vs blanks; the veteran absorbs one on its shields,
        // the other reaches the novice's hull (crit::draw(9) = Stunned Pilot).
        let rec = resolve(&c, &mut gs, vec![3, 3, 7, 7, 7, 9, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(gs.ships[1].shields, 2);
        assert_eq!(gs.ships[2].hull, 2);
        assert!(rec.events.iter().any(|e| e.contains("Draw Their Fire")), "{:?}", rec.events);
    }

    #[test]
    fn crew_and_astromech_riders_neutralise_faceup_cards() {
        let c = content();
        let mut ev = Vec::new();
        let mut rolls = scripted(vec![7]);
        // Chewbacca (crew): card discarded, hull point back, one shield back.
        let mut gs = talent_duel(&c, UpgradeId(150));
        gs.ships[1].hull = 2;
        gs.ships[1].shields = 1;
        gs.apply_crit_effect(&c, 1, CritEffect::DirectHit, &mut rolls, &mut ev);
        assert_eq!((gs.ships[1].hull, gs.ships[1].shields), (3, 2));
        assert!(gs.ships[1].crits.is_empty());
        assert!(!gs.ships[1].upgrades.contains(&UpgradeId(150)));

        // Moff Jerjerrod: discarded to flip the card facedown.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.ships[0].upgrades.push(UpgradeId(173));
        gs.apply_crit_effect(&c, 0, CritEffect::DamagedEngine, &mut rolls, &mut ev);
        assert!(gs.ships[0].crits.is_empty());
        assert!(!gs.ships[0].upgrades.contains(&UpgradeId(173)));

        // Integrated Astromech: the R2 Astromech goes instead of the card.
        let mut gs = talent_duel(&c, UpgradeId(76));
        gs.ships[1].upgrades.push(UpgradeId(41));
        gs.ships[1].hull = 2;
        gs.apply_crit_effect(&c, 1, CritEffect::DirectHit, &mut rolls, &mut ev);
        assert_eq!(gs.ships[1].hull, 3);
        assert!(gs.ships[1].crits.is_empty());
        assert!(!gs.ships[1].upgrades.contains(&UpgradeId(41)));
        assert!(gs.ships[1].upgrades.contains(&UpgradeId(76)));
    }

    #[test]
    fn end_phase_repairs_r5p9_r5_and_r2d2_crew() {
        let c = content();
        // R5-P9: the unspent focus buys a shield at the end of Combat.
        let mut gs = talent_duel(&c, UpgradeId(51));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        gs.ships[1].shields = 2;
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(gs.ships[1].shields, 3);
        assert!(rec.events.iter().any(|e| e.contains("R5-P9")), "{:?}", rec.events);

        // R5 Astromech: a Ship card is repaired in the End phase, a Pilot card stays.
        let mut gs = talent_duel(&c, UpgradeId(48));
        gs.ships[1].crits.push(CritEffect::StunnedPilot);
        gs.ships[1].crits.push(CritEffect::DamagedEngine);
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(gs.ships[1].crits, vec![CritEffect::StunnedPilot]);
        assert!(rec.events.iter().any(|e| e.contains("R5 Astromech")), "{:?}", rec.events);

        // R2-D2 (crew): shield back at the end of the End phase; the hit
        // rolled afterwards turns a facedown card faceup (Stunned Pilot).
        let mut gs = talent_duel(&c, UpgradeId(155));
        gs.ships[1].shields = 0;
        gs.ships[1].hull = 2;
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 0, 9]);
        assert_eq!(gs.ships[1].shields, 1);
        assert_eq!(gs.ships[1].hull, 2);
        assert!(gs.ships[1].crits.contains(&CritEffect::StunnedPilot));
        assert!(rec.events.iter().any(|e| e.contains("R2-D2")), "{:?}", rec.events);
    }

    #[test]
    fn r2d2_astromech_recovers_a_shield_after_a_green_maneuver() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(42));
        gs.ships[1].shields = 1;
        let green = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.difficulty == Difficulty::Easy
        });
        gs.plan_maneuver(&c, P1, ShipId(1), green).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(gs.ships[1].shields, 2);
        assert!(rec.events.iter().any(|e| e.contains("R2-D2")), "{:?}", rec.events);
    }

    // ---------------- Second actions, templates, card actions ----------------

    #[test]
    fn push_the_limit_grants_a_second_bar_action_then_stress() {
        let c = content();
        let ptl = UpgradeId(102);
        let mut gs = talent_duel(&c, ptl);
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // Only bar actions, and not on a ship without the card.
        assert_eq!(
            gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::Evade)),
            Err(Rejection::SecondActionNotAllowed)
        );
        assert_eq!(
            gs.plan_second_action(&c, P0, ShipId(0), Some(PlannedAction::Focus)),
            Err(Rejection::SecondActionNotAllowed)
        );
        gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::Boost(BoostDir::Straight)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        assert_eq!(
            mv.second,
            Some((PlannedAction::Boost(BoostDir::Straight), ActionResult::Performed))
        );
        assert_eq!(gs.ships[1].stress, 1);
        // Boosted one unit further south than the plain straight 4.
        assert!((gs.ships[1].pose.unwrap().anchor.y - 12.5).abs() < 1e-9);
        assert!(rec.events.iter().any(|e| e.contains("Push the Limit")), "{:?}", rec.events);
    }

    #[test]
    fn darth_vader_takes_two_actions_without_stress() {
        let c = content();
        let mut gs = duel(&c, "darthvader", "bluesquadronnovice");
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        gs.plan_second_action(&c, P0, ShipId(0), Some(PlannedAction::Evade)).unwrap();
        // Vader (PS9) fires first: [Eye, Eye] → the focus is spent.
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.second, Some((PlannedAction::Evade, ActionResult::Performed)));
        assert_eq!(gs.ships[0].stress, 0);
        assert!(imperial_shot(&rec).attacker_focus_spent);
    }

    #[test]
    fn snap_wexley_boosts_for_free_after_a_speed_2_to_4_maneuver() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "snapwexley");
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::Boost(BoostDir::Straight)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(
            mv.second,
            Some((PlannedAction::Boost(BoostDir::Straight), ActionResult::Performed))
        );
        assert_eq!(mv.action_result, ActionResult::Performed, "the focus action still happens");
        assert!((gs.ships[1].pose.unwrap().anchor.y - 12.5).abs() < 1e-9);
        assert_eq!(gs.ships[1].stress, 0);
    }

    #[test]
    fn blue_ace_turn_boosts_and_zeta_ace_far_rolls() {
        let c = content();
        // Turn-template boosts are Blue Ace only.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::Boost(BoostDir::TurnLeft)),
            Err(Rejection::TemplateNotAllowed)
        );
        let mut gs = duel(&c, "academypilot", "blueace");
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Boost(BoostDir::TurnLeft)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        // Heading south, a left turn ends facing east.
        assert!((gs.ships[1].pose.unwrap().heading.rem_euclid(std::f64::consts::TAU)).abs() < 1e-6);

        // Straight-2 barrel rolls are Zeta Ace only: 2 + base width sideways.
        let mut gs = duel(&c, "zetaace", "bluesquadronnovice");
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRollFar(Side::Left)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        assert!((gs.ships[0].pose.unwrap().anchor.x - 7.0).abs() < 1e-9, "{:?}", gs.ships[0].pose);
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        assert_eq!(
            gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRollFar(Side::Left)),
            Err(Rejection::TemplateNotAllowed)
        );
    }

    #[test]
    fn bb8_rolls_before_a_green_maneuver_and_jake_farrell_after_a_focus() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(40));
        let green = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.difficulty == Difficulty::Easy
        });
        gs.plan_maneuver(&c, P1, ShipId(1), green).unwrap();
        gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::BarrelRoll(Side::Left)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.pre, Some((PlannedAction::BarrelRoll(Side::Left), ActionResult::Performed)));
        // Heading south, "left" is +x: two units over, and the flown path
        // starts from the rolled position.
        assert!((mv.path[0].anchor.x - 12.0).abs() < 1e-9, "{:?}", mv.path[0]);

        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[("jakefarrell", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::BarrelRoll(Side::Right)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(
            mv.second,
            Some((PlannedAction::BarrelRoll(Side::Right), ActionResult::Performed))
        );
        // Not after an evade action.
        let mut gs = skirmish(
            &c,
            &[("academypilot", Pose::new(10.0, 2.5, FRAC_PI_2), 5)],
            &[("jakefarrell", Pose::new(10.0, 17.5, -FRAC_PI_2), 4)],
        );
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Evade).unwrap();
        gs.plan_second_action(&c, P1, ShipId(1), Some(PlannedAction::BarrelRoll(Side::Right)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.second, None);
    }

    #[test]
    fn card_actions_marksmanship_rage_expose_and_r2f2_last_the_round() {
        let c = content();
        // Marksmanship: [Eye, Eye, Blank] → one crit, one hit, no token.
        let mut gs = talent_duel(&c, UpgradeId(108));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(108))).unwrap();
        let rec = resolve(&c, &mut gs, vec![4, 4, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let shot = rebel_shot(&rec);
        assert_eq!((shot.hits, shot.crits), (1, 1));
        assert!(!shot.attacker_focus_spent);
        assert!(gs.ships[1].card_actions.is_empty(), "cleared in the End phase");

        // Rage: focus + 2 stress, then up to 3 rerolls: blanks become hits.
        let mut gs = talent_duel(&c, UpgradeId(128));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(128))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7, 7, 7, 0, 0, 0, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        assert_eq!(rebel_shot(&rec).hits, 3);
        assert_eq!(gs.ships[1].stress, 2);

        // Expose: 4 attack dice at Range 3, and only 2 defense dice.
        let mut gs = talent_duel(&c, UpgradeId(112));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(112))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(rebel_shot(&rec).attack_faces.len(), 4);
        assert_eq!(imperial_shot(&rec).defense_faces.len(), 2);

        // R2-F2: agility 3 → 4 defense dice at Range 3.
        let mut gs = talent_duel(&c, UpgradeId(44));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(44))).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(imperial_shot(&rec).defense_faces.len(), 4);
        // A card the ship does not carry is refused.
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::CardAction(UpgradeId(108))),
            Err(Rejection::NoSuchUpgrade)
        );
    }

    // ---------------- Obstacles ----------------

    fn rock(id: u32, kind: ObstacleKind, x: f64, y: f64) -> Obstacle {
        Obstacle { id, kind, center: Vec2::new(x, y), heading: 0.0, shape: 2 }
    }

    #[test]
    fn crossing_an_asteroid_costs_the_action_and_rolls_for_damage() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(rock(0, ObstacleKind::Asteroid, 10.0, 5.0));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        // The X-Wing (PS2) moves first without dice; the TIE's obstacle
        // die is the first roll: a hit.
        let rec = resolve(&c, &mut gs, vec![0, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.obstacles_hit, vec![0]);
        assert_eq!(mv.action_result, ActionResult::SkippedObstacle);
        assert_eq!(gs.ships[0].hull, 2);
        assert!(rec.attacks.iter().any(|a| a.attacker == ShipId(0)), "flew past: may still attack");
    }

    #[test]
    fn ending_on_an_asteroid_forbids_attacking_this_round_only() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(rock(0, ObstacleKind::Asteroid, 10.0, 7.0));
        let rec = resolve(&c, &mut gs, vec![7]);
        assert!(rec.attacks.iter().all(|a| a.attacker != ShipId(0)), "{:?}", rec.attacks);
        assert!(rec.events.iter().any(|e| e.contains("stuck on the asteroid")), "{:?}", rec.events);
        assert!(!gs.ships[0].on_asteroid, "cleared in the End phase");
    }

    #[test]
    fn debris_stresses_and_only_a_crit_hurts() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(rock(0, ObstacleKind::Debris, 10.0, 5.0));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        // Crit on the debris die, then crit::draw(9) = Stunned Pilot.
        let rec = resolve(&c, &mut gs, vec![3, 9, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::SkippedStressed);
        assert_eq!(gs.ships[0].stress, 1);
        assert_eq!(gs.ships[0].hull, 2);
        assert!(gs.ships[0].crits.contains(&CritEffect::StunnedPilot));
    }

    #[test]
    fn obstructed_attacks_give_the_defender_a_die_and_trick_shot_the_attacker() {
        let c = content();
        let mut gs = talent_duel(&c, UpgradeId(133));
        // Between the two after they move (TIE nose at y=7.5, X-Wing at 13.5).
        gs.obstacles.push(rock(0, ObstacleKind::Asteroid, 10.0, 10.5));
        let rec = resolve(&c, &mut gs, vec![7]);
        let xwing = rebel_shot(&rec);
        assert!(xwing.obstructed);
        assert_eq!(xwing.attack_faces.len(), 4, "3 + Trick Shot");
        assert_eq!(xwing.defense_faces.len(), 5, "3 + Range 3 + obstruction");
        let tie = imperial_shot(&rec);
        assert_eq!(tie.attack_faces.len(), 2);
        assert_eq!(tie.defense_faces.len(), 4, "2 + Range 3 + obstruction");
    }

    #[test]
    fn ships_neither_deploy_nor_roll_onto_obstacles() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(rock(0, ObstacleKind::Asteroid, 12.0, 2.0));
        gs.phase = Phase::Placement;
        assert_eq!(
            gs.place_ship(&c, P0, ShipId(0), Pose::new(12.0, 2.5, FRAC_PI_2)),
            Err(Rejection::OverlapsObstacle)
        );
        gs.phase = Phase::Planning;
        // A barrel roll to the right (toward +x from a north-facing ship
        // is "right" = -x… so roll left, onto the rock at x=12) fails.
        gs.obstacles[0] = rock(0, ObstacleKind::Asteroid, 8.0, 7.0);
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRoll(Side::Left)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Failed);
        assert!((gs.ships[0].pose.unwrap().anchor.x - 10.0).abs() < 1e-9);
    }

    #[test]
    fn seismic_torpedo_blasts_ships_around_an_obstacle_and_removes_it() {
        let c = content();
        let torpedo = UpgradeId(7);
        let mut gs = skirmish(
            &c,
            &[("gammasquadronpilot", Pose::new(10.0, 3.0, FRAC_PI_2), 1)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].upgrades.push(torpedo);
        // Stage the X-Wing mid-board by hand (outside its deployment zone).
        gs.ships[1].pose = Some(Pose::new(10.0, 9.0, -FRAC_PI_2));
        // Rock between them: ~2 units ahead of the bomber's base after its
        // straight 1, ~1 unit from the X-Wing's after its own.
        gs.obstacles.push(rock(3, ObstacleKind::Asteroid, 10.0, 6.5));
        assert_eq!(
            gs.plan_action(&c, P0, ShipId(0), PlannedAction::CardActionAt(torpedo, 9)),
            Err(Rejection::NoSuchObstacle)
        );
        assert_eq!(
            gs.plan_action(&c, P0, ShipId(0), PlannedAction::CardActionAt(UpgradeId(181), 3)),
            Err(Rejection::NoSuchUpgrade)
        );
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::CardActionAt(torpedo, 3)).unwrap();
        // The X-Wing (PS2) moves first without dice. The bomber's torpedo
        // rolls one die per ship in index order: a hit on the bomber
        // itself, a critical on the T-70 (absorbed by its 3 shields).
        let rec = resolve(&c, &mut gs, vec![0, 3, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7, 7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        let blast = mv.seismic.as_ref().expect("blast recorded");
        assert_eq!(blast.obstacle, 3);
        assert_eq!(blast.detonation.token.kind, BombKind::SeismicTorpedo);
        assert_eq!(blast.detonation.hits.len(), 2, "{:?}", blast.detonation.hits);
        assert_eq!(gs.ships[0].hull, 5, "{:?}", rec.events);
        assert_eq!(gs.ships[1].shields, 2, "{:?} {:?}", blast.detonation.hits, rec.events);
        assert_eq!(gs.ships[1].hull, 3);
        assert!(gs.obstacles.is_empty(), "the asteroid is removed");
        assert!(!gs.ships[0].upgrades.contains(&torpedo), "card discarded");
        assert!(rec.events.iter().any(|e| e.contains("breaks up")), "{:?}", rec.events);
    }

    #[test]
    fn seismic_torpedo_needs_the_obstacle_in_arc_at_range_1_to_2() {
        let c = content();
        let torpedo = UpgradeId(7);
        let mut gs = skirmish(
            &c,
            &[("gammasquadronpilot", Pose::new(10.0, 3.0, FRAC_PI_2), 1)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.ships[0].upgrades.push(torpedo);
        // Behind the bomber: never in the front arc.
        gs.obstacles.push(rock(3, ObstacleKind::Debris, 10.0, 1.0));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::CardActionAt(torpedo, 3)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 14]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Failed);
        assert!(mv.seismic.is_none());
        assert_eq!(gs.obstacles.len(), 1);
        assert!(gs.ships[0].upgrades.contains(&torpedo), "card kept");
        assert!(rec.events.iter().any(|e| e.contains("not at Range 1-2")), "{:?}", rec.events);
    }

    #[test]
    fn lorrir_rolls_with_a_bank_template_for_a_stress() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("lieutenantlorrir", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRollBank(Side::Left, true)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        let p = gs.ships[0].pose.unwrap();
        // Rolled to the left (-x), ahead of the straight-2 end (y=5), nose
        // turned 45° to the right; one stress for the template.
        assert!(p.anchor.x < 9.0, "{p:?}");
        assert!(p.anchor.y > 5.0, "{p:?}");
        assert!((p.heading - std::f64::consts::FRAC_PI_4).abs() < 1e-9, "{p:?}");
        assert_eq!(gs.ships[0].stress, 1);
        assert!(rec.events.iter().any(|e| e.contains("bank template roll")), "{:?}", rec.events);

        // Other pilots cannot use the bank templates.
        let mut gs = skirmish(
            &c,
            &[("turrphennir", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        assert_eq!(
            gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRollBank(Side::Left, true)),
            Err(Rejection::TemplateNotAllowed)
        );
    }

    #[test]
    fn turr_phennir_boosts_after_his_attack() {
        let c = content();
        let mut gs = skirmish(
            &c,
            &[("turrphennir", Pose::new(10.0, 3.0, FRAC_PI_2), 2)],
            &[("bluesquadronnovice", Pose::new(10.0, 17.5, -FRAC_PI_2), 1)],
        );
        // Stage the X-Wing mid-board: at Range 2 ahead of Turr after his
        // straight 2 and its straight 1.
        gs.ships[1].pose = Some(Pose::new(10.0, 9.0, -FRAC_PI_2));
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::Focus).unwrap();
        gs.plan_second_action(&c, P0, ShipId(0), Some(PlannedAction::Boost(BoostDir::Straight)))
            .unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 24]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert!(mv.second.is_none(), "the reposition waits for the attack");
        assert!((mv.end.anchor.y - 5.0).abs() < 1e-9, "{:?}", mv.end);
        let atk = rec.attacks.iter().find(|a| a.attacker == ShipId(0)).expect("Turr shoots first");
        let r = atk.reposition.expect("free boost recorded");
        assert_eq!(r.result, ActionResult::Performed);
        assert!((r.to.anchor.y - 6.0).abs() < 1e-9, "{r:?}");
        assert!((gs.ships[0].pose.unwrap().anchor.y - 6.0).abs() < 1e-9);
        assert!(rec.events.iter().any(|e| e.contains("Turr Phennir")), "{:?}", rec.events);
    }

    #[test]
    fn expert_handling_rolls_without_the_icon_and_strips_a_lock() {
        let c = content();
        let expert = UpgradeId(122);
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        // The T-70 has no barrel roll on its bar.
        assert_eq!(
            gs.plan_action(&c, P1, ShipId(1), PlannedAction::BarrelRoll(Side::Left)),
            Err(Rejection::ActionNotOnBar)
        );
        gs.ships[1].upgrades.push(expert);
        gs.ships[0].lock = Some(ShipId(1));
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::BarrelRoll(Side::Left)).unwrap();
        let rec = resolve(&c, &mut gs, vec![7; 20]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert_eq!(mv.action_result, ActionResult::Performed);
        // South-facing: "left" is +x; one template plus the base width.
        let p = gs.ships[1].pose.unwrap();
        assert!((p.anchor.x - 12.0).abs() < 1e-9, "{p:?}");
        assert_eq!(gs.ships[1].stress, 1, "no icon: stress");
        assert_eq!(gs.ships[0].lock, None, "the TIE's lock is removed");
        assert!(rec.events.iter().any(|e| e.contains("target lock removed")), "{:?}", rec.events);
    }

    #[test]
    fn a_black_hole_core_swallows_a_ship_that_touches_it() {
        let c = content();
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(Obstacle {
            id: 0,
            kind: ObstacleKind::BlackHole,
            center: Vec2::new(10.0, 5.0),
            heading: 0.0,
            shape: 0,
        });
        let rec = resolve(&c, &mut gs, vec![7]);
        let mv = rec.moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert_eq!(mv.obstacles_hit, vec![0]);
        assert!(gs.ships[0].destroyed);
        assert!(rec.events.iter().any(|e| e.contains("swallowed")), "{:?}", rec.events);
        assert!(rec.attacks.iter().all(|a| a.attacker != ShipId(0)));
    }

    #[test]
    fn black_holes_drag_ships_within_range_5_and_swallow_at_the_core() {
        let c = content();
        let hole = |x, y| Obstacle {
            id: 7,
            kind: ObstacleKind::BlackHole,
            center: Vec2::new(x, y),
            heading: 0.0,
            shape: 0,
        };
        // Hole far to the east of the TIE's end position (nose y=7.5):
        // within Range 5, so the TIE slides one unit toward it.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(hole(18.0, 7.0));
        let rec = resolve(&c, &mut gs, vec![7]);
        assert_eq!(rec.pulls.len(), 2, "both ships are within Range 5: {:?}", rec.pulls);
        let tie = rec.pulls.iter().find(|p| p.ship == ShipId(0)).unwrap();
        assert!(!tie.swallowed);
        let d = tie.to.anchor - tie.from.anchor;
        assert!(((d.x * d.x + d.y * d.y).sqrt() - 1.0).abs() < 1e-9);
        assert!(d.x > 0.9, "straight toward the hole: {d:?}");
        assert_eq!(tie.to.heading, tie.from.heading);
        assert!(rec.events.iter().any(|e| e.contains("pulled toward the black hole")));

        // Out of reach: a hole more than 12.5 units away does nothing.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(hole(2.0, 19.0));
        gs.ships[1].pose = Some(Pose::new(10.0, 17.5, -FRAC_PI_2));
        let rec = resolve(&c, &mut gs, vec![7]);
        assert!(rec.pulls.iter().all(|p| p.ship != ShipId(0)), "{:?}", rec.pulls);

        // Sitting just short of the core: the pull drags the base onto it.
        let mut gs = duel(&c, "academypilot", "bluesquadronnovice");
        gs.obstacles.push(hole(10.0, 8.7));
        let rec = resolve(&c, &mut gs, vec![7]);
        let tie = rec.pulls.iter().find(|p| p.ship == ShipId(0)).unwrap();
        assert!(tie.swallowed);
        assert!(gs.ships[0].destroyed);
        assert!(rec.attacks.iter().all(|a| a.attacker != ShipId(0)));
    }
}
