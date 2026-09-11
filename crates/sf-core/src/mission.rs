//! Missions (core rules p.21-24): a two-sided game with special setup,
//! special rules and objectives. Three come from the rulebook; each is a
//! `MissionKind` with its rules written into the engine (`game.rs`
//! consults `MissionState`), while the scenario file only names it.
//!
//! Conventions: the Rebel side is the side of the seats flying Rebel
//! squads (side 0 when the host creates the game); each side deploys from
//! its own edge as usual. Player choices the rulebook leaves open are
//! automated with a stated policy (see the doc comment on each rule).

use serde::{Deserialize, Serialize};

use crate::board::{Board, Seat};
use crate::combat::RANGE_BAND_UNITS;
use crate::data::Content;
use crate::geometry::{Pose, Vec2};
use crate::ship::{Faction, ShipId};
use crate::squad::{Squad, SquadShip};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MissionKind {
    /// Mission 1: the senator's shuttle must cross the board.
    PoliticalEscort,
    /// Mission 2: a disabled Rebel ship must survive until Round 5 and flee.
    AsteroidRun,
    /// Mission 3: Imperial ships scan satellites and carry the data home.
    DarkWhispers,
}

/// Mission 2: the disabled ship is repaired at the start of this round.
pub const REPAIR_ROUND: u32 = 5;

/// Range 1, 2 and 3 in board units.
pub const R1: f64 = RANGE_BAND_UNITS;
pub const R2: f64 = 2.0 * RANGE_BAND_UNITS;
pub const R3: f64 = 3.0 * RANGE_BAND_UNITS;

/// Side of a satellite token (a small square token, in board units).
pub const SATELLITE_SIZE: f64 = 1.0;

/// (x_min, y_min, x_max, y_max).
pub type Rect = (f64, f64, f64, f64);

/// A pilot (xws) with its upgrades (xws), as printed in "Mission Setup".
pub type FixedShip = (&'static str, &'static [&'static str]);

impl MissionKind {
    pub const ALL: [MissionKind; 3] =
        [MissionKind::PoliticalEscort, MissionKind::AsteroidRun, MissionKind::DarkWhispers];

    pub fn number(self) -> u8 {
        match self {
            MissionKind::PoliticalEscort => 1,
            MissionKind::AsteroidRun => 2,
            MissionKind::DarkWhispers => 3,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            MissionKind::PoliticalEscort => "Political Escort",
            MissionKind::AsteroidRun => "Asteroid Run",
            MissionKind::DarkWhispers => "Dark Whispers",
        }
    }

    /// The special rules, as the HUD and glossary show them.
    pub fn rules_text(self) -> &'static str {
        match self {
            MissionKind::PoliticalEscort => {
                "The senator's shuttle (agility 2, hull 6; shields 6 with 100-point squads) starts at the centre of the Rebel edge and moves first every round with one of: stay, bank 1 left, straight 2, bank 1 right; it cannot act or attack, and critical hits against it count as hits. Rebel ships within Range 1 of the shuttle may take the PROTECT action (key P) to put an evade token on it; the shuttle spends at most one per attack and loses them all in the End phase. In each End phase the Empire places one Academy Pilot within Range 1 of its edge per Imperial ship destroyed that round."
            }
            MissionKind::AsteroidRun => {
                "The Empire deploys within Range 1 of either edge; the Rebels deploy in the middle of the board (beyond Range 3 of both edges, not within Range 1 of an asteroid). The Rebels' first ship is the DISABLED ship: until Round 5 it may only fly speed 1-2 maneuvers. From Round 5 it may flee off the Rebel or Imperial edge without being destroyed. In each End phase the Empire places one Academy Pilot within Range 1 of either edge per Imperial ship destroyed that round."
            }
            MissionKind::DarkWhispers => {
                "Both sides deploy within Range 1 of their edge. Two satellite tokens (four with 100-point squads) sit in the Rebel half. An Imperial ship overlapping a satellite (or touching a Rebel ship that overlaps one) SCANS it instead of attacking: the token moves onto that ship. A scanning ship that is destroyed returns its tokens to the supply. Once every satellite has been scanned, an Imperial ship carrying one may flee off the Imperial edge without being destroyed. In each End phase the Rebels place one Rookie Pilot within Range 1 of their edge per Rebel ship destroyed that round."
            }
        }
    }

    /// What `faction` must do to win.
    pub fn objective(self, faction: Faction) -> &'static str {
        match (self, faction) {
            (MissionKind::PoliticalEscort, Faction::RebelAlliance) => {
                "Get the senator's shuttle off the Imperial edge of the board."
            }
            (MissionKind::PoliticalEscort, Faction::Empire) => "Destroy the senator's shuttle.",
            (MissionKind::AsteroidRun, Faction::RebelAlliance) => {
                "Keep the disabled ship alive; from Round 5, fly it off the Rebel or Imperial edge."
            }
            (MissionKind::AsteroidRun, Faction::Empire) => "Destroy the disabled ship.",
            (MissionKind::DarkWhispers, Faction::RebelAlliance) => {
                "Destroy every Imperial ship, or see every satellite token returned to the supply."
            }
            (MissionKind::DarkWhispers, Faction::Empire) => {
                "Scan every satellite, then fly a ship carrying one off the Imperial edge."
            }
            (_, Faction::Scum) => "Scum and Villainy take no part in the rulebook missions.",
        }
    }

    /// "Mission Setup" forces (used when a player joins without a squad).
    pub fn fixed_force(self, faction: Faction) -> &'static [FixedShip] {
        match (self, faction) {
            (MissionKind::PoliticalEscort, Faction::RebelAlliance) => &[("redsquadronpilot", &[])],
            (MissionKind::PoliticalEscort, Faction::Empire) => {
                &[("academypilot", &[]), ("academypilot", &[])]
            }
            (MissionKind::AsteroidRun, Faction::RebelAlliance) => {
                &[("lukeskywalker", &["determination"])]
            }
            (MissionKind::AsteroidRun, Faction::Empire) => {
                &[("nightbeast", &[]), ("maulermithel", &["marksmanship"])]
            }
            (MissionKind::DarkWhispers, Faction::RebelAlliance) => {
                &[("redsquadronpilot", &["protontorpedoes", "r2f2"])]
            }
            (MissionKind::DarkWhispers, Faction::Empire) => {
                &[("blacksquadronpilot", &["determination"]), ("obsidiansquadronpilot", &[])]
            }
            (_, Faction::Scum) => &[],
        }
    }

    /// Which side gets reinforcements, which pilot arrives, and whether
    /// it may be placed at either edge (else its own).
    pub fn reinforcements(self) -> (Faction, &'static str, bool) {
        match self {
            MissionKind::PoliticalEscort => (Faction::Empire, "academypilot", false),
            MissionKind::AsteroidRun => (Faction::Empire, "academypilot", true),
            MissionKind::DarkWhispers => (Faction::RebelAlliance, "rookiepilot", false),
        }
    }

    /// Number of satellite tokens (mission 3) for the squad-point size.
    pub fn satellite_count(self, points: u32) -> usize {
        match self {
            MissionKind::DarkWhispers if points >= 100 => 4,
            MissionKind::DarkWhispers => 2,
            _ => 0,
        }
    }

    /// Shield value of the senator's shuttle (mission 1).
    pub fn shuttle_shields(self, points: u32) -> u8 {
        if self == MissionKind::PoliticalEscort && points >= 100 { 6 } else { 0 }
    }
}

/// A satellite token (mission 3): on the board, on a scanning ship's
/// card, or back in the supply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Satellite {
    pub id: u32,
    pub pos: Vec2,
    pub holder: Option<ShipId>,
    pub supply: bool,
}

impl Satellite {
    pub fn on_board(&self) -> bool {
        self.holder.is_none() && !self.supply
    }

    /// The token's outline (axis-aligned square).
    pub fn corners(&self) -> [Vec2; 4] {
        let h = SATELLITE_SIZE / 2.0;
        [
            Vec2::new(self.pos.x - h, self.pos.y - h),
            Vec2::new(self.pos.x + h, self.pos.y - h),
            Vec2::new(self.pos.x + h, self.pos.y + h),
            Vec2::new(self.pos.x - h, self.pos.y + h),
        ]
    }
}

/// Mission bookkeeping inside `GameState`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionState {
    pub kind: MissionKind,
    /// Side index of the Rebel and Imperial players.
    pub rebel_side: u8,
    pub imperial_side: u8,
    /// Mission 1: the senator's shuttle.
    pub shuttle: Option<ShipId>,
    /// Mission 2: the disabled ship.
    pub disabled: Option<ShipId>,
    /// Mission 3: the satellite tokens.
    pub satellites: Vec<Satellite>,
    /// Destroyed ships already answered with a reinforcement.
    pub reinforced: Vec<ShipId>,
    /// Reinforcements placed so far (for callsigns).
    pub spawned: u32,
}

impl MissionState {
    pub fn side_of(&self, faction: Faction) -> u8 {
        match faction {
            Faction::RebelAlliance => self.rebel_side,
            Faction::Empire => self.imperial_side,
            // Scum never fly missions; treat them as the Rebel side.
            Faction::Scum => self.rebel_side,
        }
    }

    pub fn faction_of_side(&self, side: u8) -> Faction {
        if side == self.imperial_side { Faction::Empire } else { Faction::RebelAlliance }
    }

    /// Mission 3: every satellite has left the board.
    pub fn all_scanned(&self) -> bool {
        self.satellites.iter().all(|s| !s.on_board())
    }
}

/// What the client sees of the mission (public information).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissionView {
    pub kind: MissionKind,
    /// Side index of the Rebel player(s).
    pub rebel_side: u8,
    /// The viewer's own objective.
    pub objective: String,
    pub satellites: Vec<Satellite>,
    pub shuttle: Option<ShipId>,
    pub disabled: Option<ShipId>,
}

/// The band `depth` deep along `seat`'s edge.
pub fn edge_zone(board: &Board, seat: Seat, depth: f64) -> Rect {
    match seat {
        Seat::South => (0.0, 0.0, board.width, depth),
        Seat::North => (0.0, board.height - depth, board.width, board.height),
        Seat::East => (board.width - depth, 0.0, board.width, board.height),
        Seat::West => (0.0, 0.0, depth, board.height),
    }
}

/// The board with a band `depth` deep cut off along `seat`'s edge.
fn beyond(rect: Rect, seat: Seat, depth: f64) -> Rect {
    let (x0, y0, x1, y1) = rect;
    match seat {
        Seat::South => (x0, y0 + depth, x1, y1),
        Seat::North => (x0, y0, x1, y1 - depth),
        Seat::East => (x0, y0, x1 - depth, y1),
        Seat::West => (x0 + depth, y0, x1, y1),
    }
}

/// Where a player may place ships: at setup, or when placing a
/// reinforcement (`reinforcing`). `own` is the player's edge, `other`
/// the enemy's.
pub fn deploy_zones(
    kind: MissionKind,
    board: &Board,
    faction: Faction,
    own: Seat,
    other: Seat,
    reinforcing: bool,
) -> Vec<Rect> {
    let edge = |seat, depth| edge_zone(board, seat, depth);
    if reinforcing {
        let (_, _, either) = kind.reinforcements();
        return if either { vec![edge(own, R1), edge(other, R1)] } else { vec![edge(own, R1)] };
    }
    match (kind, faction) {
        (MissionKind::PoliticalEscort, _) => vec![edge(own, R2)],
        (MissionKind::AsteroidRun, Faction::Empire) => vec![edge(own, R1), edge(other, R1)],
        (MissionKind::AsteroidRun, Faction::RebelAlliance | Faction::Scum) => {
            let all = (0.0, 0.0, board.width, board.height);
            vec![beyond(beyond(all, own, R3), other, R3)]
        }
        (MissionKind::DarkWhispers, _) => vec![edge(own, R1)],
    }
}

/// A point `along` the edge of `seat` (from its left corner, looking
/// into the board) and `depth` into the board.
pub fn edge_point(board: &Board, seat: Seat, along: f64, depth: f64) -> Vec2 {
    match seat {
        Seat::South => Vec2::new(along, depth),
        Seat::North => Vec2::new(board.width - along, board.height - depth),
        Seat::East => Vec2::new(board.width - depth, along),
        Seat::West => Vec2::new(depth, board.height - along),
    }
}

/// Length of `seat`'s edge.
fn edge_length(board: &Board, seat: Seat) -> f64 {
    match seat {
        Seat::South | Seat::North => board.width,
        Seat::East | Seat::West => board.height,
    }
}

/// Mission 1: the shuttle starts at the exact centre of the Rebel edge,
/// within Range 1 of it, pointing at the Imperial edge. (`length` is the
/// token's base length; the pose anchor is its front centre.)
pub fn shuttle_pose(board: &Board, rebel: Seat, length: f64) -> Pose {
    let p = edge_point(board, rebel, edge_length(board, rebel) / 2.0, length + 0.2);
    Pose::new(p.x, p.y, rebel.facing())
}

/// Mission 3: satellite positions, in place of the Rebel player's
/// choice — one within Range 2 and one within Range 3 of the Rebel edge
/// (two more with 100-point squads), each at least Range 2 from the side
/// edges and at Range 2-3 of another satellite (core rules p.24).
pub fn satellite_positions(board: &Board, rebel: Seat, count: usize) -> Vec<Vec2> {
    let len = edge_length(board, rebel);
    let clamp = |frac: f64| (frac * len).clamp(R2 + 0.5, len - R2 - 0.5);
    let spots = [(0.35, 4.4), (0.65, 6.8), (0.5, 3.0), (0.5, 6.8)];
    spots
        .iter()
        .take(count)
        .map(|&(frac, depth)| edge_point(board, rebel, clamp(frac), depth))
        .collect()
}

/// Which edge a base centre has left the board through (the one it is
/// furthest beyond).
pub fn exit_edge(board: &Board, p: Vec2) -> Seat {
    let over = [
        (Seat::South, -p.y),
        (Seat::North, p.y - board.height),
        (Seat::West, -p.x),
        (Seat::East, p.x - board.width),
    ];
    over.iter()
        .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(s, _)| *s)
        .unwrap_or(Seat::South)
}

/// Shortest distance between two convex outlines (0 when they overlap
/// or touch).
pub fn outline_distance(a: &[Vec2], b: &[Vec2]) -> f64 {
    fn seg_dist(p: Vec2, a: Vec2, b: Vec2) -> f64 {
        let ab = b - a;
        let len2 = ab.dot(ab);
        let t = if len2 == 0.0 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
        let q = Vec2::new(a.x + ab.x * t, a.y + ab.y * t);
        let d = p - q;
        d.dot(d).sqrt()
    }
    if crate::obstacle::convex_overlap(a, b) {
        return 0.0;
    }
    let mut best = f64::INFINITY;
    for (pts, poly) in [(a, b), (b, a)] {
        for &p in pts {
            for k in 0..poly.len() {
                best = best.min(seg_dist(p, poly[k], poly[(k + 1) % poly.len()]));
            }
        }
    }
    best
}

/// The printed force for one side of a mission, as a squad.
pub fn fixed_squad(
    content: &Content,
    kind: MissionKind,
    faction: Faction,
) -> Result<Squad, String> {
    let mut ships = Vec::new();
    for (pilot_xws, upgrades) in kind.fixed_force(faction) {
        let pilot = content
            .pilots
            .pilots
            .iter()
            .find(|p| p.xws == *pilot_xws)
            .ok_or_else(|| format!("mission pilot {pilot_xws} is not in the data"))?;
        let mut ids = Vec::new();
        for u in upgrades.iter() {
            let card = content
                .upgrades
                .upgrades
                .iter()
                .find(|c| c.xws == *u)
                .ok_or_else(|| format!("mission upgrade {u} is not in the data"))?;
            ids.push(card.id);
        }
        ships.push(SquadShip { pilot: pilot.id, upgrades: ids, callsign: String::new() });
    }
    Ok(Squad { name: format!("Mission {}", kind.number()), faction, ships })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Board {
        Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
    }

    #[test]
    fn zones_follow_the_rulebook() {
        let b = board();
        let escort = deploy_zones(
            MissionKind::PoliticalEscort,
            &b,
            Faction::Empire,
            Seat::North,
            Seat::South,
            false,
        );
        assert_eq!(escort, vec![(0.0, 15.0, 20.0, 20.0)]);
        let imp = deploy_zones(
            MissionKind::AsteroidRun,
            &b,
            Faction::Empire,
            Seat::North,
            Seat::South,
            false,
        );
        assert_eq!(imp, vec![(0.0, 17.5, 20.0, 20.0), (0.0, 0.0, 20.0, 2.5)]);
        let reb = deploy_zones(
            MissionKind::AsteroidRun,
            &b,
            Faction::RebelAlliance,
            Seat::South,
            Seat::North,
            false,
        );
        assert_eq!(reb, vec![(0.0, 7.5, 20.0, 12.5)]);
        let reinf = deploy_zones(
            MissionKind::DarkWhispers,
            &b,
            Faction::RebelAlliance,
            Seat::South,
            Seat::North,
            true,
        );
        assert_eq!(reinf, vec![(0.0, 0.0, 20.0, 2.5)]);
    }

    #[test]
    fn satellites_respect_spacing_rules() {
        let b = board();
        let sats = satellite_positions(&b, Seat::South, 4);
        assert_eq!(sats.len(), 4);
        for s in &sats {
            assert!(s.x >= R2 && s.x <= b.width - R2, "Range 2 from the side edges: {s:?}");
            assert!(s.y + SATELLITE_SIZE / 2.0 <= R3, "within Range 3 of the Rebel edge: {s:?}");
        }
        assert!(sats[0].y + SATELLITE_SIZE / 2.0 <= R2 && sats[2].y + 0.5 <= R2);
        for (i, s) in sats.iter().enumerate() {
            let near = sats.iter().enumerate().any(|(j, t)| {
                let d = ((s.x - t.x).powi(2) + (s.y - t.y).powi(2)).sqrt();
                i != j && d > R1 && d <= R3
            });
            assert!(near, "satellite {i} has no neighbour at Range 2-3");
        }
        // The North player's tokens mirror.
        let north = satellite_positions(&b, Seat::North, 2);
        assert!(north.iter().all(|s| s.y >= b.height - R3));
    }

    #[test]
    fn exit_edges_and_outline_distance() {
        let b = board();
        assert_eq!(exit_edge(&b, Vec2::new(10.0, -0.7)), Seat::South);
        assert_eq!(exit_edge(&b, Vec2::new(10.0, 20.4)), Seat::North);
        assert_eq!(exit_edge(&b, Vec2::new(-1.0, 3.0)), Seat::West);
        assert_eq!(exit_edge(&b, Vec2::new(21.0, 19.0)), Seat::East);
        let sq = |x: f64, y: f64| {
            vec![
                Vec2::new(x, y),
                Vec2::new(x + 1.0, y),
                Vec2::new(x + 1.0, y + 1.0),
                Vec2::new(x, y + 1.0),
            ]
        };
        assert!((outline_distance(&sq(0.0, 0.0), &sq(3.0, 0.0)) - 2.0).abs() < 1e-9);
        assert_eq!(outline_distance(&sq(0.0, 0.0), &sq(0.5, 0.5)), 0.0);
    }

    #[test]
    fn fixed_forces_exist_and_validate() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data");
        let c = Content::load_dir(dir).expect("content");
        for kind in MissionKind::ALL {
            for faction in [Faction::RebelAlliance, Faction::Empire] {
                let squad = fixed_squad(&c, kind, faction).expect("fixed force");
                let rules = crate::squad::SquadRules::default();
                assert!(
                    crate::squad::validate_squad(&squad, &c, &rules).is_ok(),
                    "mission {} {faction:?} force is not a legal squad",
                    kind.number()
                );
            }
        }
    }
}
