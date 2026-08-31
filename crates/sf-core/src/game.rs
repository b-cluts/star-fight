//! The authoritative game state and turn phase machine:
//!
//! `Placement -> [Planning -> Resolution]* -> GameOver`
//!
//! Every mutation goes through a validated command method returning
//! `Result<_, Rejection>`; the server applies these verbatim, the client
//! may use the same methods for optimistic UI checks.

use serde::{Deserialize, Serialize};

use crate::action::{self, ActionResult, PlannedAction};
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
        })
    }

    fn seat_of(player: PlayerId) -> Seat {
        if player.0 == 0 { Seat::South } else { Seat::North }
    }

    fn class_of<'a>(&self, content: &'a Content, ship: &ShipState) -> &'a ShipClass {
        content.ships.class(ship.class).expect("classes validated in new()")
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
        if self.ships[i].stress > 0 && man.difficulty == Difficulty::Hard {
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
    /// turn resolves immediately and the records are returned. `roll`
    /// supplies raw d8 values for combat dice (the server's RNG; tests
    /// script it for deterministic outcomes).
    pub fn commit_plans(
        &mut self,
        content: &Content,
        player: PlayerId,
        roll: &mut dyn FnMut() -> u8,
    ) -> Result<Option<TurnRecords>, Rejection> {
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
        if self.committed == [true, true] {
            Ok(Some(self.resolve(content, roll)))
        } else {
            Ok(None)
        }
    }

    /// Reveal and fly all plans in movement order (lowest pilot skill
    /// first), perform actions, then fight the Combat phase (highest
    /// skill first) and the End phase.
    fn resolve(&mut self, content: &Content, roll: &mut dyn FnMut() -> u8) -> TurnRecords {
        let order = movement_order(
            &self
                .ships
                .iter()
                .filter(|s| !s.destroyed && s.pose.is_some())
                .map(|s| (s.id, self.class_of(content, s).pilot_skill, s.owner))
                .collect::<Vec<_>>(),
            self.initiative,
        );
        let mut records = Vec::new();
        for id in order {
            let i = self.ship_index(id).expect("ordered ids exist");
            let (fp, dial_id) = {
                let class = self.class_of(content, &self.ships[i]);
                (class.footprint, class.maneuver_set)
            };
            let dial = &content.dials.set(dial_id).expect("validated").maneuvers;
            let man = dial[self.ships[i].plan.expect("commit checked plans") as usize];
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

            // Walk forward; stop just before the first overlapping sample.
            let mut stop = path.len() - 1;
            'walk: for (k, p) in path.iter().enumerate().skip(1) {
                let corners = rules::footprint_corners(*p, fp);
                for oc in &obstacles {
                    if rules::obbs_overlap(&corners, oc) {
                        stop = k - 1;
                        break 'walk;
                    }
                }
            }
            let end = path[stop];
            let bumped = stop + 1 != path.len();
            let destroyed =
                !rules::within_board(&self.board, &rules::footprint_corners(end, fp));

            {
                let ship = &mut self.ships[i];
                ship.pose = Some(end);
                ship.plan = None;
                match man.difficulty {
                    Difficulty::Hard => ship.stress += 1,
                    Difficulty::Easy => ship.stress = ship.stress.saturating_sub(1),
                    Difficulty::Normal => {}
                }
                if destroyed {
                    ship.destroyed = true;
                }
            }

            // Perform Action step: one action, right after moving. Stress,
            // bumping, or having flown off the board all forfeit it.
            let planned =
                self.ships[i].planned_action.take().unwrap_or(PlannedAction::Pass);
            let action_result = if destroyed {
                ActionResult::Failed
            } else if self.ships[i].stress > 0 {
                ActionResult::SkippedStressed
            } else if bumped {
                ActionResult::SkippedBumped
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
                path: path[..=stop].to_vec(),
                end,
                bumped,
                destroyed,
                stress,
                action: planned,
                action_result,
            });
        }

        // Combat phase: highest pilot skill fires first (initiative breaks
        // ties). Ships of equal skill fire "simultaneously": everyone alive
        // when their skill group starts still gets their shot, even if
        // destroyed within the group.
        let mut attacks = Vec::new();
        let combatants: Vec<(ShipId, u8, PlayerId)> = self
            .ships
            .iter()
            .filter(|s| !s.destroyed && s.pose.is_some())
            .map(|s| (s.id, self.class_of(content, s).pilot_skill, s.owner))
            .collect();
        let order = combat_order(&combatants, self.initiative);
        let skill_of = |id: ShipId| {
            combatants.iter().find(|(s, _, _)| *s == id).map(|(_, k, _)| *k).unwrap_or(0)
        };
        let mut g = 0;
        while g < order.len() {
            let group_skill = skill_of(order[g]);
            let group_end = order[g..]
                .iter()
                .position(|&id| skill_of(id) != group_skill)
                .map(|p| g + p)
                .unwrap_or(order.len());
            // Alive at group start = allowed to attack this group.
            let allowed: Vec<ShipId> = order[g..group_end]
                .iter()
                .copied()
                .filter(|&id| {
                    let i = self.ship_index(id).expect("ordered ids exist");
                    !self.ships[i].destroyed
                })
                .collect();
            for id in allowed {
                if let Some(rec) = self.perform_attack(content, id, roll) {
                    attacks.push(rec);
                }
            }
            g = group_end;
        }

        // End phase: unspent focus and evade tokens are removed from all
        // ships; target locks persist, except locks on ships that are now
        // destroyed.
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
        TurnRecords { moves: records, attacks }
    }

    /// One attack, auto-resolved: target = locked ship if eligible, else
    /// the nearest enemy with any part of its base in the firing arc at
    /// range 1-3 (touching bases cannot be targeted). Token policy: spend
    /// the lock to reroll misses, focus when eyes matter, evade when
    /// damage would otherwise land.
    fn perform_attack(
        &mut self,
        content: &Content,
        attacker: ShipId,
        roll: &mut dyn FnMut() -> u8,
    ) -> Option<AttackRecord> {
        let a_idx = self.ship_index(attacker).ok()?;
        let a_pose = self.ships[a_idx].pose?;
        let (a_fp, a_dice) = {
            let c = self.class_of(content, &self.ships[a_idx]);
            (c.footprint, c.attack_dice)
        };
        let a_corners = rules::footprint_corners(a_pose, a_fp);

        // Eligible targets: enemy, alive, any base point in arc, range 1-3,
        // not touching.
        let mut candidates: Vec<(ShipId, u8, f64)> = Vec::new();
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
        let locked = self.ships[a_idx].lock;
        let &(defender, range, _) = candidates
            .iter()
            .find(|(id, _, _)| Some(*id) == locked)
            .or_else(|| {
                candidates
                    .iter()
                    .min_by(|a, b| a.2.partial_cmp(&b.2).unwrap_or(std::cmp::Ordering::Equal))
            })?;
        let d_idx = self.ship_index(defender).ok()?;

        // Roll attack dice (+1 at range 1).
        let n_atk = a_dice + u8::from(range == 1);
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
            c.agility + u8::from(range == 3)
        };
        let mut defense_faces: Vec<DefenseFace> =
            (0..n_def).map(|_| DefenseFace::from_d8(roll())).collect();

        // Modify defense: focus converts eyes when it helps, evade token
        // adds one evade result if damage would still land.
        let mut evades = defense_faces.iter().filter(|f| **f == DefenseFace::Evade).count() as u8;
        let incoming = raw_hits + raw_crits;
        let mut defender_focus_spent = false;
        let eyes = defense_faces.iter().filter(|f| **f == DefenseFace::Focus).count() as u8;
        if self.ships[d_idx].focus > 0 && eyes > 0 && evades < incoming {
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
        if self.ships[d_idx].evade > 0 && evades < incoming {
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

        // Deal damage: hits before crits; shields absorb first; only crits
        // reaching the hull are critical.
        let mut shields_lost = 0;
        let mut hull_lost = 0;
        let mut crits_to_hull = 0;
        {
            let d = &mut self.ships[d_idx];
            for _ in 0..hits {
                if d.shields > 0 {
                    d.shields -= 1;
                    shields_lost += 1;
                } else if d.hull > 0 {
                    d.hull -= 1;
                    hull_lost += 1;
                }
            }
            for _ in 0..crits {
                if d.shields > 0 {
                    d.shields -= 1;
                    shields_lost += 1;
                } else if d.hull > 0 {
                    d.hull -= 1;
                    hull_lost += 1;
                    crits_to_hull += 1;
                }
            }
            if d.hull == 0 {
                d.destroyed = true;
            }
        }

        Some(AttackRecord {
            attacker,
            defender,
            range,
            attack_faces,
            defense_faces,
            lock_spent,
            attacker_focus_spent,
            defender_focus_spent,
            evade_spent,
            hits,
            crits,
            shields_lost,
            hull_lost,
            crits_to_hull,
            defender_destroyed: self.ships[d_idx].destroyed,
        })
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
        gs.plan_action(&c, P1, ShipId(1), PlannedAction::Focus).unwrap();
        // X-Wing attack: eye, eye, blank → focus turns 2 eyes into hits.
        // TIE defense: 4 blanks, then spends its evade token → 1 evade,
        // so 1 hit lands on the shieldless TIE's hull.
        // TIE attack: 2 blanks; X-Wing defense: 3 blanks.
        let mut rolls = scripted(vec![4, 4, 6, 7, 7, 7, 7, 6, 6, 7, 7, 7]);
        gs.commit_plans(&c, P0, &mut rolls).unwrap();
        let rec = gs.commit_plans(&c, P1, &mut rolls).unwrap().unwrap();
        let xw_shot = &rec.attacks[0];
        assert!(xw_shot.attacker_focus_spent);
        assert!(xw_shot.evade_spent);
        assert_eq!((xw_shot.hits, xw_shot.crits), (1, 0));
        assert_eq!(xw_shot.hull_lost, 1);
        assert_eq!(gs.ships[0].hull, 2);
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
