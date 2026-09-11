//! A simple computer player, so one person can try a game (or a 3-4
//! seat game) without rounding up opponents. It works from the same
//! public snapshot a client sees (`ShipView`s, obstacles, zones, the
//! mission view) and never peeks at hidden plans. The server drives it
//! as a virtual client; nothing here talks to the network.
//!
//! Policy, in one breath: deploy spread out along the zone facing in;
//! every round fly the dial maneuver that ends on the board, off rocks
//! and ships, with the nearest enemy in the front arc and close — or
//! failing that, closest to it; take Focus (else Evade, else nothing);
//! in a mission, run for an escape edge when allowed, chase satellites
//! as the Empire, and protect the shuttle as a Rebel. Shoot the weakest
//! target, with a secondary weapon when one is offered.

use crate::action::{ActionKind, PlannedAction};
use crate::board::{Board, Seat};
use crate::combat::{self, RANGE_BAND_UNITS};
use crate::data::Content;
use crate::game::ShipView;
use crate::geometry::{Footprint, Pose, Vec2};
use crate::maneuver::{self, Difficulty};
use crate::mission::{self, MissionKind, MissionView, Rect};
use crate::obstacle::{self, Obstacle};
use crate::rules;
use crate::ship::{Faction, ShipId};
use crate::squad::{Squad, SquadShip};
use crate::upgrade::UpgradeId;

/// What the bot knows when it decides: a client's view of the game.
pub struct View<'a> {
    pub content: &'a Content,
    pub board: &'a Board,
    pub ships: &'a [ShipView],
    pub obstacles: &'a [Obstacle],
    pub zones: &'a [Rect],
    pub mission: Option<&'a MissionView>,
    pub turn: u32,
    pub seat: u8,
    pub team: u8,
}

impl View<'_> {
    fn footprint(&self, v: &ShipView) -> Footprint {
        self.content
            .ships
            .class(v.class)
            .map(|c| c.footprint)
            .unwrap_or(Footprint { length: 1.0, width: 1.0 })
    }

    fn mine(&self, v: &ShipView) -> bool {
        v.owner.0 == u32::from(self.seat)
    }

    fn enemy(&self, v: &ShipView) -> bool {
        v.team != self.team && !v.destroyed && v.pose.is_some()
    }

    /// Edges this ship may leave the board through alive (mission rules).
    fn escape_edges(&self, v: &ShipView) -> Vec<Seat> {
        let Some(m) = self.mission else { return Vec::new() };
        let rebel = Seat::for_side(m.rebel_side, 2);
        let imperial = Seat::for_side(1 - m.rebel_side, 2);
        match m.kind {
            MissionKind::PoliticalEscort if m.shuttle == Some(v.id) => vec![imperial],
            MissionKind::AsteroidRun
                if m.disabled == Some(v.id) && self.turn >= mission::REPAIR_ROUND =>
            {
                vec![rebel, imperial]
            }
            MissionKind::DarkWhispers
                if v.team != m.rebel_side
                    && v.satellites > 0
                    && m.satellites.iter().all(|s| !s.on_board()) =>
            {
                vec![imperial]
            }
            _ => Vec::new(),
        }
    }
}

/// A squad for a bot seat: the faction's cheapest generic fighter,
/// repeated up to the points (Academy Pilots, or Rookie Pilots).
pub fn squad(content: &Content, faction: Faction, points: u32) -> Squad {
    let pilot = content
        .pilots
        .pilots
        .iter()
        .filter(|p| p.cost > 0 && !p.unique)
        .filter(|p| content.ships.class(p.class).is_some_and(|c| c.faction == faction))
        .min_by_key(|p| p.cost);
    let mut ships = Vec::new();
    if let Some(p) = pilot {
        let n = (points / u32::from(p.cost)).clamp(1, 12);
        for _ in 0..n {
            ships.push(SquadShip { pilot: p.id, upgrades: Vec::new(), callsign: String::new() });
        }
    }
    Squad { name: "Bot squadron".into(), faction, ships }
}

/// Where to put every unplaced own ship: spread along the first zone,
/// facing into the board, skipping spots on obstacles or other ships.
pub fn placements(view: &View) -> Vec<(ShipId, Pose)> {
    let Some(&zone) = view.zones.first() else { return Vec::new() };
    let (x0, y0, x1, y1) = zone;
    let board = view.board;
    // Which edge the zone hugs decides the facing.
    let seat = if y0 <= 0.0 && y1 < board.height {
        Seat::South
    } else if y1 >= board.height && y0 > 0.0 {
        Seat::North
    } else if x1 >= board.width && x0 > 0.0 {
        Seat::East
    } else if x0 <= 0.0 && x1 < board.width {
        Seat::West
    } else {
        Seat::South
    };
    let heading = seat.facing();
    let mut taken: Vec<[Vec2; 4]> = view
        .ships
        .iter()
        .filter(|v| !v.destroyed)
        .filter_map(|v| v.pose.map(|p| rules::footprint_corners(p, view.footprint(v))))
        .collect();
    let mut out = Vec::new();
    for v in view.ships.iter().filter(|v| view.mine(v) && !v.destroyed && v.pose.is_none()) {
        let fp = view.footprint(v);
        let depth_in = fp.length + 0.3;
        let candidates = (0..40).map(|k| {
            // Zig-zag outward from the middle of the zone's edge.
            let step = 1.6 * fp.width.max(1.0);
            let off = (k as f64).div_euclid(2.0) * step * if k % 2 == 0 { 1.0 } else { -1.0 };
            let along = match seat {
                Seat::South | Seat::North => (x0 + x1) / 2.0 + off,
                Seat::East | Seat::West => (y0 + y1) / 2.0 + off,
            };
            let d = match seat {
                Seat::South => y0 + depth_in,
                Seat::North => board.height - y1 + depth_in,
                Seat::East => board.width - x1 + depth_in,
                Seat::West => x0 + depth_in,
            };
            let p = mission::edge_point(board, seat, along, d);
            Pose::new(p.x, p.y, heading)
        });
        for pose in candidates {
            let corners = rules::footprint_corners(pose, fp);
            let clear = rules::placement_legal_in(view.zones, pose, fp, &[]).is_ok()
                && taken.iter().all(|t| !rules::obbs_overlap(&corners, t))
                && view
                    .obstacles
                    .iter()
                    .all(|o| mission::outline_distance(&corners, &o.polygon()) > RANGE_BAND_UNITS);
            if clear {
                taken.push(corners);
                out.push((v.id, pose));
                break;
            }
        }
    }
    out
}

/// The plan for one ship: dial index and action.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub ship: ShipId,
    pub maneuver: u8,
    pub action: PlannedAction,
}

/// Plans for every own ship on the board.
pub fn plans(view: &View) -> Vec<Plan> {
    view.ships
        .iter()
        .filter(|v| view.mine(v) && !v.destroyed && v.pose.is_some())
        .filter_map(|v| plan_ship(view, v))
        .collect()
}

fn plan_ship(view: &View, me: &ShipView) -> Option<Plan> {
    let content = view.content;
    let class = content.ships.class(me.class)?;
    let dial = &content.dials.set(class.maneuver_set)?.maneuvers;
    let pose = me.pose?;
    let fp = class.footprint;
    let m = view.mission;
    let disabled =
        m.is_some_and(|m| m.disabled == Some(me.id)) && view.turn < mission::REPAIR_ROUND;
    let escape = view.escape_edges(me);
    let is_shuttle = m.is_some_and(|m| m.shuttle == Some(me.id));
    // Goals: enemies (base centres), or satellites still on the board for
    // an Imperial ship in mission 3.
    let enemies: Vec<Vec2> = view
        .ships
        .iter()
        .filter(|v| view.enemy(v))
        .map(|v| combat::base_center(v.pose.expect("enemy"), view.footprint(v)))
        .collect();
    let satellites: Vec<Vec2> = m
        .filter(|m| m.kind == MissionKind::DarkWhispers && me.team != m.rebel_side)
        .map(|m| m.satellites.iter().filter(|s| s.on_board()).map(|s| s.pos).collect())
        .unwrap_or_default();
    let others: Vec<[Vec2; 4]> = view
        .ships
        .iter()
        .filter(|v| v.id != me.id && !v.destroyed)
        .filter_map(|v| v.pose.map(|p| rules::footprint_corners(p, view.footprint(v))))
        .collect();
    let rocks: Vec<Vec<Vec2>> = view.obstacles.iter().map(|o| o.polygon()).collect();

    let mut best: Option<(f64, u8)> = None;
    for (idx, man) in dial.iter().enumerate() {
        if me.stress > 0 && man.difficulty == Difficulty::Hard {
            continue;
        }
        if disabled && man.distance > 2 {
            continue;
        }
        let Ok(path) = maneuver::sample_path(pose, *man) else { continue };
        let end = *path.last().expect("path has the start");
        let corners = rules::footprint_corners(end, fp);
        let mut score = 0.0;
        if !rules::within_board(view.board, &corners) {
            let exit = mission::exit_edge(view.board, combat::base_center(end, fp));
            if escape.contains(&exit) {
                score += 1000.0;
            } else {
                continue;
            }
        }
        if others.iter().any(|o| rules::obbs_overlap(&corners, o)) {
            score -= 40.0;
        }
        if path.iter().any(|p| {
            rocks.iter().any(|r| obstacle::convex_overlap(&rules::footprint_corners(*p, fp), r))
        }) {
            score -= 25.0;
        }
        let centre = combat::base_center(end, fp);
        let dist = |p: &Vec2| ((p.x - centre.x).powi(2) + (p.y - centre.y).powi(2)).sqrt();
        if !satellites.is_empty() {
            let sat = satellites.iter().min_by(|a, b| dist(a).total_cmp(&dist(b))).expect("some");
            let d = dist(sat);
            score += if d <= 0.5 { 60.0 } else { 20.0 - d };
        } else if let Some(near) = enemies.iter().min_by(|a, b| dist(a).total_cmp(&dist(b))) {
            let d = dist(near);
            if is_shuttle {
                // Head for the far edge: distance to the escape edge is
                // what counts, nothing to shoot.
                let edge = escape.first().copied().unwrap_or(Seat::North);
                let to_edge = match edge {
                    Seat::North => view.board.height - centre.y,
                    Seat::South => centre.y,
                    Seat::East => view.board.width - centre.x,
                    Seat::West => centre.x,
                };
                score += 30.0 - to_edge;
            } else if combat::in_front_arc(end, fp, *near) && d <= 3.0 * RANGE_BAND_UNITS {
                score += 30.0 - (d - RANGE_BAND_UNITS).abs();
            } else {
                score += 10.0 - d * 0.7;
            }
        } else if !escape.is_empty() {
            let edge = escape[0];
            let to_edge = match edge {
                Seat::North => view.board.height - centre.y,
                Seat::South => centre.y,
                Seat::East => view.board.width - centre.x,
                Seat::West => centre.x,
            };
            score += 30.0 - to_edge;
        }
        // Green sheds stress; red costs it.
        score += match man.difficulty {
            Difficulty::Easy if me.stress > 0 => 5.0,
            Difficulty::Hard => -3.0,
            _ => 0.0,
        };
        if best.is_none_or(|(b, _)| score > b) {
            best = Some((score, idx as u8));
        }
    }
    let (_, maneuver) = best?;
    // Action: Protect when a Rebel fighter will end within Range 1 of the
    // shuttle (mission 1); else Focus, else Evade, else nothing.
    let end = maneuver::sample_path(pose, dial[maneuver as usize])
        .ok()
        .and_then(|p| p.last().copied())
        .unwrap_or(pose);
    let action = if me.stress > 0 {
        PlannedAction::Pass
    } else if let Some(shuttle) = m
        .filter(|m| m.kind == MissionKind::PoliticalEscort && me.team == m.rebel_side)
        .and_then(|m| m.shuttle)
        .filter(|s| *s != me.id)
        .and_then(|s| view.ships.iter().find(|v| v.id == s && !v.destroyed))
        .and_then(|s| s.pose.map(|p| rules::footprint_corners(p, view.footprint(s))))
        .filter(|sc| combat::range_band_between(&rules::footprint_corners(end, fp), sc) == Some(1))
    {
        let _ = shuttle;
        PlannedAction::Protect
    } else if me.actions.contains(&ActionKind::Focus) {
        PlannedAction::Focus
    } else if me.actions.contains(&ActionKind::Evade) {
        PlannedAction::Evade
    } else {
        PlannedAction::Pass
    };
    Some(Plan { ship: me.id, maneuver, action })
}

/// The dial index to fall back on when a plan is refused: the slowest
/// non-red maneuver (a stationary or speed-1 move keeps the ship safe).
pub fn safe_maneuver(content: &Content, v: &ShipView) -> Option<u8> {
    let class = content.ships.class(v.class)?;
    let dial = &content.dials.set(class.maneuver_set)?.maneuvers;
    dial.iter()
        .enumerate()
        .filter(|(_, m)| m.difficulty != Difficulty::Hard)
        .min_by_key(|(_, m)| (m.distance, m.steer != crate::maneuver::Steer::Straight))
        .map(|(i, _)| i as u8)
}

/// Declare Target: a secondary weapon when one is offered (it was worth
/// equipping), otherwise the primary against the weakest target.
pub fn choose_target(
    ships: &[ShipView],
    options: &[(ShipId, Option<UpgradeId>, u8)],
) -> Option<(ShipId, Option<UpgradeId>)> {
    let toughness = |id: ShipId| {
        ships.iter().find(|v| v.id == id).map(|v| u32::from(v.hull) + u32::from(v.shields))
    };
    options
        .iter()
        .min_by_key(|(t, w, r)| (w.is_none(), toughness(*t), *r))
        .map(|(t, w, _)| (*t, *w))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game::GameState;
    use crate::ship::PlayerId;
    use std::f64::consts::FRAC_PI_2;

    fn content() -> Content {
        Content::load_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data")).unwrap()
    }

    fn board() -> Board {
        Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
    }

    #[test]
    fn bot_squads_fill_the_points_with_generic_pilots() {
        let c = content();
        let imp = squad(&c, Faction::Empire, 100);
        assert_eq!(imp.ships.len(), 8, "eight Academy Pilots");
        let reb = squad(&c, Faction::RebelAlliance, 100);
        let scum = squad(&c, Faction::Scum, 100);
        assert_eq!(scum.faction, Faction::Scum);
        assert!(scum.ships.len() >= 4 && scum.cost(&c) <= 100);
        assert!(reb.ships.len() >= 4 && reb.cost(&c) <= 100);
        assert!(crate::squad::validate_squad(&imp, &c, &Default::default()).is_ok());
        assert!(crate::squad::validate_squad(&reb, &c, &Default::default()).is_ok());
    }

    #[test]
    fn bot_places_plans_and_targets_from_a_snapshot() {
        let c = content();
        let imp = squad(&c, Faction::Empire, 40);
        let reb = squad(&c, Faction::RebelAlliance, 40);
        let mut gs = GameState::from_squads(
            board(),
            &c,
            &[&imp, &reb],
            &[0, 1],
            crate::dice::AttackFace::Blank,
        )
        .unwrap();
        gs.place_obstacles(&[crate::obstacle::ObstacleKind::Asteroid; 3], 7);
        // Seat 0 (three TIEs) places through the bot.
        let ships = gs.snapshot_for(&c, PlayerId(0));
        let zones = gs.deploy_zones(PlayerId(0));
        let (board, obstacles) = (gs.board, gs.obstacles.clone());
        let view = View {
            content: &c,
            board: &board,
            ships: &ships,
            obstacles: &obstacles,
            zones: &zones,
            mission: None,
            turn: 1,
            seat: 0,
            team: 0,
        };
        let placed = placements(&view);
        assert_eq!(placed.len(), 3);
        for (id, pose) in &placed {
            gs.place_ship(&c, PlayerId(0), *id, *pose).unwrap();
        }
        // The Rebel places by hand, then the bot plans for its TIEs: every
        // plan is accepted and heads toward the X-Wing.
        let rebel_ids: Vec<ShipId> =
            gs.ships.iter().filter(|s| s.owner == PlayerId(1)).map(|s| s.id).collect();
        for (k, id) in rebel_ids.iter().enumerate() {
            gs.place_ship(&c, PlayerId(1), *id, Pose::new(6.0 + 3.0 * k as f64, 18.5, -FRAC_PI_2))
                .unwrap();
        }
        let ships = gs.snapshot_for(&c, PlayerId(0));
        let view = View { ships: &ships, turn: gs.turn, ..view };
        let plans = plans(&view);
        assert_eq!(plans.len(), 3);
        for p in &plans {
            gs.plan_maneuver(&c, PlayerId(0), p.ship, p.maneuver).unwrap();
            gs.plan_action(&c, PlayerId(0), p.ship, p.action).unwrap();
            assert_eq!(p.action, PlannedAction::Focus);
            let before = gs.ships.iter().find(|s| s.id == p.ship).unwrap().pose.unwrap();
            let man = c.dials.set(crate::maneuver::ManeuverSetId(1)).unwrap().maneuvers
                [p.maneuver as usize];
            let end = *maneuver::sample_path(before, man).unwrap().last().unwrap();
            assert!(end.anchor.y > before.anchor.y, "moves toward the Rebels");
        }
        // Target choice: the weakest target (range breaks ties, closer
        // first), or the secondary weapon when offered.
        let opts = [(ShipId(3), None, 2), (ShipId(4), None, 1)];
        assert_eq!(choose_target(&ships, &opts), Some((ShipId(4), None)));
        let mut hurt = ships.clone();
        hurt.iter_mut().find(|v| v.id == ShipId(3)).unwrap().hull = 1;
        assert_eq!(choose_target(&hurt, &opts), Some((ShipId(3), None)));
        let opts = [(ShipId(3), None, 2), (ShipId(4), Some(UpgradeId(1)), 2)];
        assert_eq!(choose_target(&ships, &opts), Some((ShipId(4), Some(UpgradeId(1)))));
        assert!(safe_maneuver(&c, &ships[0]).is_some());
    }

    #[test]
    fn bot_runs_the_shuttle_for_the_imperial_edge() {
        let c = content();
        let rebel =
            mission::fixed_squad(&c, MissionKind::PoliticalEscort, Faction::RebelAlliance).unwrap();
        let imperial =
            mission::fixed_squad(&c, MissionKind::PoliticalEscort, Faction::Empire).unwrap();
        let mut gs = GameState::from_squads(
            board(),
            &c,
            &[&rebel, &imperial],
            &[0, 1],
            crate::dice::AttackFace::Blank,
        )
        .unwrap();
        gs.start_mission(&c, MissionKind::PoliticalEscort, 31).unwrap();
        let shuttle = gs.mission.as_ref().unwrap().shuttle.unwrap();
        gs.ships.iter_mut().find(|s| s.id == shuttle).unwrap().pose =
            Some(Pose::new(10.0, 18.5, FRAC_PI_2));
        gs.turn = 3;
        let ships = gs.snapshot_for(&c, PlayerId(0));
        let mv = gs.mission_view(PlayerId(0));
        let view = View {
            content: &c,
            board: &gs.board,
            ships: &ships,
            obstacles: &gs.obstacles,
            zones: &[],
            mission: mv.as_ref(),
            turn: 3,
            seat: 0,
            team: 0,
        };
        let plan = plans(&view).into_iter().find(|p| p.ship == shuttle).unwrap();
        let man = c.dials.set(crate::maneuver::ManeuverSetId(12)).unwrap().maneuvers
            [plan.maneuver as usize];
        assert_eq!(man.distance, 2, "straight 2 off the Imperial edge: {man:?}");
    }
}
