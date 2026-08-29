//! The authoritative game state and turn phase machine:
//!
//! `Placement -> [Planning -> Resolution]* -> GameOver`
//!
//! Every mutation goes through a validated command method returning
//! `Result<_, Rejection>`; the server applies these verbatim, the client
//! may use the same methods for optimistic UI checks.

use serde::{Deserialize, Serialize};

use crate::board::{Board, Seat};
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

/// Movement phase order: LOWEST pilot skill moves first.
/// Ties break deterministically by ship id.
pub fn movement_order(ships: &[(ShipId, u8)]) -> Vec<ShipId> {
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill)| (skill, id.0));
    v.into_iter().map(|(id, _)| id).collect()
}

/// Combat phase order: HIGHEST pilot skill fires first.
/// Ties break deterministically by ship id.
pub fn combat_order(ships: &[(ShipId, u8)]) -> Vec<ShipId> {
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill)| (std::cmp::Reverse(skill), id.0));
    v.into_iter().map(|(id, _)| id).collect()
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
    pub destroyed: bool,
    pub plan: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameState {
    pub board: Board,
    pub phase: Phase,
    pub turn: u32,
    pub ships: Vec<ShipState>,
    pub committed: [bool; 2],
    pub winner: Option<PlayerId>,
}

impl GameState {
    /// `fleets[0]` deploys South (seat 0), `fleets[1]` North (seat 1).
    pub fn new(
        board: Board,
        content: &Content,
        fleets: [&[ShipClassId]; 2],
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
                    destroyed: false,
                });
            }
        }
        Ok(Self {
            board,
            phase: Phase::Placement,
            turn: 1,
            ships,
            committed: [false, false],
            winner: None,
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

    /// Commit the player's plans. When both players have committed, the
    /// turn resolves immediately and the records are returned.
    pub fn commit_plans(
        &mut self,
        content: &Content,
        player: PlayerId,
    ) -> Result<Option<Vec<MoveRecord>>, Rejection> {
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
            Ok(Some(self.resolve(content)))
        } else {
            Ok(None)
        }
    }

    /// Reveal and fly all plans in movement order (lowest pilot skill
    /// first). Bumps stop a ship at the last clear pose along its path;
    /// ending off the board destroys the ship.
    fn resolve(&mut self, content: &Content) -> Vec<MoveRecord> {
        let order = movement_order(
            &self
                .ships
                .iter()
                .filter(|s| !s.destroyed && s.pose.is_some())
                .map(|s| (s.id, self.class_of(content, s).pilot_skill))
                .collect::<Vec<_>>(),
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
            let stress = ship.stress;
            records.push(MoveRecord {
                ship: id,
                maneuver: man,
                path: path[..=stop].to_vec(),
                end,
                bumped,
                destroyed,
                stress,
            });
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
            (false, false) => self.phase = Phase::GameOver, // mutual loss
        }
        records
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
                    destroyed: s.destroyed,
                    plan: if own { s.plan } else { None },
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
        GameState::new(board(), c, [&[TIE], &[XWING]]).unwrap()
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
        assert_eq!(gs.commit_plans(&c, P0).unwrap(), None);
        let moves = gs.commit_plans(&c, P1).unwrap().unwrap();
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
        gs.commit_plans(&c, P0).unwrap();
        let moves = gs.commit_plans(&c, P1).unwrap().unwrap();
        assert_eq!(moves[0].stress, 1);
        // Stressed: red now forbidden, blue allowed…
        assert_eq!(
            gs.plan_maneuver(&c, P0, ShipId(0), kturn3),
            Err(Rejection::StressedRedForbidden)
        );
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), straight2(&c, XWING)).unwrap();
        gs.commit_plans(&c, P0).unwrap();
        let moves = gs.commit_plans(&c, P1).unwrap().unwrap();
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
        gs.commit_plans(&c, P0).unwrap();
        let moves = gs.commit_plans(&c, P1).unwrap().unwrap();
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
        gs.commit_plans(&c, P0).unwrap();
        gs.commit_plans(&c, P1).unwrap().unwrap();
        // Turn 2: TIE tries 7→12; X-Wing hull occupies y 14..15, so a
        // straight-4 to 10 is clear, but TIE first: 7→12 is clear too.
        // Then X-Wing 14→10 must bump against the TIE hull at 11..12.
        gs.plan_maneuver(&c, P0, ShipId(0), s5_tie).unwrap();
        gs.plan_maneuver(&c, P1, ShipId(1), s4_xw).unwrap();
        gs.commit_plans(&c, P0).unwrap();
        let moves = gs.commit_plans(&c, P1).unwrap().unwrap();
        let xw = moves.iter().find(|m| m.ship == ShipId(1)).unwrap();
        assert!(xw.bumped, "X-Wing should bump into the TIE");
        // Stopped just above the TIE's hull (anchor is its front/south end).
        assert!(xw.end.anchor.y > 12.0 && xw.end.anchor.y < 12.3, "{}", xw.end.anchor.y);
        let tie = moves.iter().find(|m| m.ship == ShipId(0)).unwrap();
        assert!(!tie.bumped);
    }

    #[test]
    fn commit_requires_all_plans_and_resign_ends_game() {
        let c = content();
        let mut gs = new_1v1(&c);
        place_both(&c, &mut gs);
        assert_eq!(gs.commit_plans(&c, P0), Err(Rejection::PlansIncomplete));
        gs.plan_maneuver(&c, P0, ShipId(0), straight2(&c, TIE)).unwrap();
        gs.commit_plans(&c, P0).unwrap();
        assert_eq!(gs.commit_plans(&c, P0), Err(Rejection::AlreadyCommitted));
        assert_eq!(gs.resign(P1), P0);
        assert_eq!(gs.phase, Phase::GameOver);
        assert_eq!(gs.winner, Some(P0));
    }
}
