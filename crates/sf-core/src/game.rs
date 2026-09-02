//! The authoritative game state and turn phase machine:
//!
//! `Placement -> [Planning -> Resolution]* -> GameOver`
//!
//! Every mutation goes through a validated command method returning
//! `Result<_, Rejection>`; the server applies these verbatim, the client
//! may use the same methods for optimistic UI checks.

use serde::{Deserialize, Serialize};

use crate::action::{self, ActionResult, PlannedAction};
use crate::crit::{self, CritEffect};
use crate::board::{Board, Seat};
use crate::combat;
use crate::dice::{AttackFace, DefenseFace};
use crate::data::Content;
use crate::geometry::{Footprint, Pose};
use crate::maneuver::{self, Difficulty, Maneuver};
use crate::rules;
use crate::ship::{PlayerId, ShipClass, ShipClassId, ShipId, ShipState};

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
    pick(&|m| {
        eff(m) == Difficulty::Normal && m.steer == crate::maneuver::Steer::Straight
    })
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

/// An attack whose owner must Declare Target (core rules p.10): more than
/// one enemy is eligible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingAttack {
    pub attacker: ShipId,
    pub owner: PlayerId,
    /// (target, range band, base distance) for every eligible enemy.
    pub candidates: Vec<(ShipId, u8, f64)>,
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
    /// `fleets[0]` deploys South (seat 0), `fleets[1]` North (seat 1).
    /// `tie_roll` is one red die drawn by the server, used only when the
    /// squad totals are equal.
    pub fn new(
        board: Board,
        content: &Content,
        fleets: [&[ShipClassId]; 2],
        tie_roll: crate::dice::AttackFace,
    ) -> Result<Self, String> {
        let mut ships = Vec::new();
        for (seat, fleet) in fleets.iter().enumerate() {
            for &class_id in *fleet {
                let class = content
                    .ships
                    .class(class_id)
                    .ok_or_else(|| format!("unknown ship class {class_id:?}"))?;
                content
                    .dials
                    .set(class.maneuver_set)
                    .ok_or_else(|| format!("{} has no dial", class.name))?;
                ships.push(ShipState {
                    id: ShipId(ships.len() as u32),
                    owner: PlayerId(seat as u32),
                    class: class_id,
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
        let mut squad_totals = [0u32; 2];
        for (seat, fleet) in fleets.iter().enumerate() {
            squad_totals[seat] = fleet
                .iter()
                .map(|id| content.ships.class(*id).expect("checked above").squad_points as u32)
                .sum();
        }
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

    /// Pilot skill after crits: a Damaged Cockpit drops it to 0.
    fn effective_skill(&self, content: &Content, s: &ShipState) -> u8 {
        if s.crits.contains(&CritEffect::DamagedCockpit) {
            0
        } else {
            self.class_of(content, s).pilot_skill
        }
    }

    fn label(&self, content: &Content, i: usize) -> String {
        format!("{} #{}", self.class_of(content, &self.ships[i]).name, self.ships[i].id.0)
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
            .filter_map(|s| {
                s.pose.map(|p| (s.id, p, self.class_of(content, s).footprint))
            })
            .collect();
        rules::placement_legal(&self.board, Self::seat_of(player), pose, fp, &own_placed)
            .map_err(|e| match e {
                rules::PlacementError::OutOfZone => Rejection::OutOfZone,
                rules::PlacementError::OverlapsShip(_) => Rejection::OverlapsShip,
            })?;
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
        let dial =
            &content.dials.set(class.maneuver_set).expect("validated in new()").maneuvers;
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
        if let Some(kind) = planned.kind() {
            let class = self.class_of(content, &self.ships[i]);
            if !class.action_bar.contains(&kind) {
                return Err(Rejection::ActionNotOnBar);
            }
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
                    let choice = self.auto_target(&p).expect("candidates are non-empty");
                    self.declare_target(content, p.owner, choice, roll)?;
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
        let incomplete = self
            .ships
            .iter()
            .any(|s| s.owner == player && !s.destroyed && s.plan.is_none());
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
            let candidates = self.attack_candidates(content, a_idx);
            match candidates.len() {
                0 => continue,
                1 => {
                    let (target, range, _) = candidates[0];
                    let d_idx = self.ships.iter().position(|s| s.id == target).expect("candidate");
                    let mut ev = Vec::new();
                    let rec = self.perform_attack_on(content, a_idx, d_idx, range, roll, &mut ev);
                    let cs = self.combat.as_mut().expect("in combat");
                    cs.events.extend(ev);
                    cs.attacks.push(rec.clone());
                    return Ok(CombatStep::Attack(rec));
                }
                _ => {
                    let p = PendingAttack { attacker, owner, candidates };
                    self.combat.as_mut().expect("in combat").pending = Some(p.clone());
                    return Ok(CombatStep::NeedTarget(p));
                }
            }
        }
    }

    /// The owner's Declare Target choice for the pending attack.
    pub fn declare_target(
        &mut self,
        content: &Content,
        player: PlayerId,
        target: ShipId,
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
        let &(_, range, _) = pending
            .candidates
            .iter()
            .find(|(id, _, _)| *id == target)
            .ok_or(Rejection::BadTarget)?;
        let a_idx = self.ship_index(pending.attacker)?;
        let d_idx = self.ship_index(target)?;
        let mut ev = Vec::new();
        let rec = self.perform_attack_on(content, a_idx, d_idx, range, roll, &mut ev);
        let cs = self.combat.as_mut().expect("checked above");
        cs.pending = None;
        cs.events.extend(ev);
        cs.attacks.push(rec.clone());
        Ok(rec)
    }

    /// Automatic target policy: the locked ship if eligible, else the
    /// nearest candidate.
    pub fn auto_target(&self, p: &PendingAttack) -> Option<ShipId> {
        let lock = self.ships.iter().find(|s| s.id == p.attacker).and_then(|s| s.lock);
        p.candidates
            .iter()
            .find(|(id, _, _)| Some(*id) == lock)
            .or_else(|| {
                p.candidates.iter().min_by(|a, b| {
                    a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal)
                })
            })
            .map(|(id, _, _)| *id)
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
            let fled =
                !rules::within_board(&self.board, &rules::footprint_corners(end, fp));

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
                let died =
                    if self.ships[i].destroyed { " — DESTROYED" } else { "" };
                events.push(format!("{label}: Stunned Pilot — 1 damage from the collision{died}"));
            }
            let destroyed = self.ships[i].destroyed;

            // Perform Action step: one action, right after moving. Stress,
            // bumping, destruction, or damaged sensors all forfeit it.
            let planned =
                self.ships[i].planned_action.take().unwrap_or(PlannedAction::Pass);
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
                                combat::range_band_between(
                                    &my,
                                    &rules::footprint_corners(tp, tfp),
                                )
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
        let dead: Vec<ShipId> =
            self.ships.iter().filter(|s| s.destroyed).map(|s| s.id).collect();
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
        let alive = |p: u32| {
            self.ships.iter().any(|s| s.owner == PlayerId(p) && !s.destroyed)
        };
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
    fn attack_candidates(&self, content: &Content, a_idx: usize) -> Vec<(ShipId, u8, f64)> {
        if self.ships[a_idx]
            .crits
            .iter()
            .any(|c| matches!(c, CritEffect::WeaponsFailure { .. }))
        {
            return Vec::new();
        }
        let Some(a_pose) = self.ships[a_idx].pose else { return Vec::new() };
        let a_fp = self.class_of(content, &self.ships[a_idx]).footprint;
        let a_corners = rules::footprint_corners(a_pose, a_fp);
        let mut candidates = Vec::new();
        for s in &self.ships {
            if s.owner == self.ships[a_idx].owner || s.destroyed {
                continue;
            }
            let Some(pose) = s.pose else { continue };
            let fp = self.class_of(content, s).footprint;
            let corners = rules::footprint_corners(pose, fp);
            let in_arc = corners.iter().any(|&p| combat::in_front_arc(a_pose, a_fp, p))
                || (0..4).any(|i| {
                    let m = crate::geometry::Vec2::new(
                        (corners[i].x + corners[(i + 1) % 4].x) / 2.0,
                        (corners[i].y + corners[(i + 1) % 4].y) / 2.0,
                    );
                    combat::in_front_arc(a_pose, a_fp, m)
                });
            if !in_arc {
                continue;
            }
            let dist = combat::base_distance(&a_corners, &corners);
            if dist <= 0.0 {
                continue; // touching bases cannot be targeted
            }
            let Some(band) = combat::range_band_between(&a_corners, &corners) else {
                continue;
            };
            candidates.push((s.id, band, dist));
        }
        candidates
    }

    /// Resolve one declared attack (dice, token spending, damage, crits).
    /// Token policy: spend the lock to reroll misses, focus when eyes
    /// matter, evade when damage would otherwise land.
    fn perform_attack_on(
        &mut self,
        content: &Content,
        a_idx: usize,
        d_idx: usize,
        range: u8,
        roll: &mut dyn FnMut() -> u8,
        events: &mut Vec<String>,
    ) -> AttackRecord {
        let attacker = self.ships[a_idx].id;
        let defender = self.ships[d_idx].id;
        let a_pose = self.ships[a_idx].pose.expect("attackers are on the board");
        let a_dice = self.class_of(content, &self.ships[a_idx]).attack_dice;

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
            (a_dice + u8::from(range == 1)).saturating_sub(malfunctions)
        };
        let mut attack_faces: Vec<AttackFace> =
            (0..n_atk).map(|_| AttackFace::from_d8(roll())).collect();

        // Modify attack: spend the lock to reroll blanks (and eyes too if
        // no focus token is held), then focus converts eyes to hits.
        let mut lock_spent = false;
        if self.ships[a_idx].lock == Some(defender) {
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
        let mut attacker_focus_spent = false;
        if self.ships[a_idx].focus > 0 && attack_faces.contains(&AttackFace::Focus) {
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
        let n_def = {
            let c = self.class_of(content, &self.ships[d_idx]);
            // Structural Damage: −1 agility per copy.
            let structural = self.ships[d_idx]
                .crits
                .iter()
                .filter(|x| matches!(x, CritEffect::StructuralDamage))
                .count() as u8;
            c.agility.saturating_sub(structural) + u8::from(range == 3)
        };
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
        let mut evades = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
        let incoming = raw_hits + raw_crits;
        let mut defender_focus_spent = false;
        let eyes = defense_faces.iter().filter(|f| **f == DefenseFace::Focus).count() as u8;
        if !defender_in_bullseye && self.ships[d_idx].focus > 0 && eyes > 0 && evades < incoming {
            self.ships[d_idx].focus -= 1;
            defender_focus_spent = true;
            for f in defense_faces.iter_mut() {
                if *f == DefenseFace::Focus {
                    *f = DefenseFace::Evade;
                }
            }
            evades += eyes;
        }
        let mut evade_spent = false;
        if !defender_in_bullseye && self.ships[d_idx].evade > 0 && evades < incoming {
            self.ships[d_idx].evade -= 1;
            evade_spent = true;
            evades += 1;
        }

        // Compare results: evades cancel hits before crits.
        let mut hits = raw_hits;
        let mut crits = raw_crits;
        let canceled_hits = hits.min(evades);
        hits -= canceled_hits;
        crits -= (evades - canceled_hits).min(crits);

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

        AttackRecord {
            attacker,
            defender,
            range,
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
    pub fn snapshot_for(&self, viewer: PlayerId) -> Vec<ShipView> {
        self.ships
            .iter()
            .map(|s| {
                let own = s.owner == viewer;
                ShipView {
                    id: s.id,
                    owner: s.owner,
                    class: s.class,
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
        Content::from_ron(
            &std::fs::read_to_string(format!("{dir}/ships.ron")).unwrap(),
            &std::fs::read_to_string(format!("{dir}/maneuvers.ron")).unwrap(),
        )
        .unwrap()
    }

    fn board() -> Board {
        Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
    }

    fn new_1v1(c: &Content) -> GameState {
        GameState::new(board(), c, [&[TIE], &[XWING]], crate::dice::AttackFace::Hit).unwrap()
    }

    /// Index of a maneuver on a class's dial.
    fn dial_index(c: &Content, class: ShipClassId, m: fn(&Maneuver) -> bool) -> u8 {
        let set = c.ships.class(class).unwrap().maneuver_set;
        c.dials.set(set).unwrap().maneuvers.iter().position(|x| m(x)).unwrap() as u8
    }

    fn straight2(c: &Content, class: ShipClassId) -> u8 {
        dial_index(c, class, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 2
        })
    }

    fn place_both(c: &Content, gs: &mut GameState) {
        gs.place_ship(c, P0, ShipId(0), Pose::new(10.0, 2.0, FRAC_PI_2)).unwrap();
        gs.place_ship(c, P1, ShipId(1), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
    }

    #[test]
    fn initiative_breaks_equal_pilot_skill() {
        // Mirror match: all skill 1; P1 holds initiative.
        let ships = [
            (ShipId(0), 1, P0),
            (ShipId(1), 1, P0),
            (ShipId(2), 1, P1),
            (ShipId(3), 1, P1),
        ];
        assert_eq!(
            movement_order(&ships, P1),
            vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]
        );
        assert_eq!(
            combat_order(&ships, P1),
            vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]
        );
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
        assert!(gs.snapshot_for(P1)[0].pose.is_none());
        assert!(gs.snapshot_for(P0)[0].pose.is_some());
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
        assert_eq!(gs.phase, Phase::Planning);
        // Everything visible once placement ends.
        assert!(gs.snapshot_for(P1)[0].pose.is_some());
    }

    #[test]
    fn full_turn_resolves_in_pilot_skill_order() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        // Opponent never sees the plan.
        assert!(gs.snapshot_for(P1)[0].plan.is_none());
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
        let kturn3 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::KTurn && m.distance == 3
        });
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
        let s3 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 3
        });
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
        let s5_tie = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
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
        let kturn3 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::KTurn && m.distance == 3
        });
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
        gs.plan_action(&c, P0, ShipId(0), PlannedAction::BarrelRoll(action::Side::Left))
            .unwrap();
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
        let s5 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
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
            [&[TIE, TIE], &[XWING]],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 1.15, FRAC_PI_2)).unwrap();
        // Blocker faces east across #0's path (hull y 2.0-3.0).
        gs.place_ship(&c, P0, ShipId(1), Pose::new(10.0, 2.5, 0.0)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(10.0, 18.0, -FRAC_PI_2)).unwrap();
        let s3 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 3
        });
        let s1 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 1
        });
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
        let tie_s5 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
        let tie_s2 = straight2(&c, TIE);
        let xw_s4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        let xw_k4 = dial_index(&c, XWING, |m| {
            m.steer == crate::maneuver::Steer::KTurn && m.distance == 4
        });
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
        let s5 = dial_index(c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
        let s4 = dial_index(c, XWING, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 4
        });
        gs.plan_maneuver(c, P0, ShipId(0), s5).unwrap();
        gs.plan_maneuver(c, P1, ShipId(1), s4).unwrap();
        gs.commit_plans(c, P0, rolls).unwrap();
    }

    #[test]
    fn combat_fires_highest_skill_first_and_strips_shields_before_hull() {
        let c = content();
        let mut gs = new_1v1(&c);
        // X-Wing (skill 2) fires first: 3 dice at R3, all blanks (6).
        // TIE defense: 3 agility + 1 (R3) = 4 dice (blanks). Then TIE
        // fires 2 dice: Hit (0) + Crit (3); X-Wing defense 2+1=3 blanks.
        let mut rolls =
            scripted(vec![6, 6, 6, 7, 7, 7, 7, 0, 3, 7, 7, 7]);
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
        let s5 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
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
        let kturn3 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::KTurn && m.distance == 3
        });
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
        let turn2 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::TurnLeft && m.distance == 2
        });
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
        assert!(gs.ships[1]
            .crits
            .contains(&crate::crit::CritEffect::WeaponsFailure { rounds: 1 }));
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
        let s5_tie = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
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
        let s5 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
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
            [&[TIE], &[TIE]],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        assert_eq!(gs.initiative, P0);
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        let s5 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
        let s1 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 1
        });
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
            [&[TIE], &[XWING, XWING]],
            crate::dice::AttackFace::Hit,
        )
        .unwrap();
        // TIE south; two X-Wings north, both ending inside its arc at R3
        // (#1 a hair nearer than #2).
        gs.place_ship(&c, P0, ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(1), Pose::new(9.0, 17.5, -FRAC_PI_2)).unwrap();
        gs.place_ship(&c, P1, ShipId(2), Pose::new(11.5, 17.5, -FRAC_PI_2)).unwrap();
        let s5 = dial_index(&c, TIE, |m| {
            m.steer == crate::maneuver::Steer::Straight && m.distance == 5
        });
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
        let mut ids: Vec<u32> = p.candidates.iter().map(|c| c.0 .0).collect();
        ids.sort();
        assert_eq!(ids, vec![1, 2]);
        // Stepping again re-issues the prompt; only the owner may answer,
        // and only with an eligible enemy.
        assert!(matches!(gs.combat_step(&c, &mut miss).unwrap(), CombatStep::NeedTarget(_)));
        assert_eq!(
            gs.declare_target(&c, P1, ShipId(2), &mut miss),
            Err(Rejection::NotYourShip)
        );
        assert_eq!(
            gs.declare_target(&c, P0, ShipId(0), &mut miss),
            Err(Rejection::BadTarget)
        );
        // Pick the farther X-Wing — overriding the auto policy's nearest.
        let rec = gs.declare_target(&c, P0, ShipId(2), &mut miss).unwrap();
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
