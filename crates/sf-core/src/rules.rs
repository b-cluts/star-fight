//! Legality checks shared verbatim by client (live feedback) and server
//! (enforcement): footprint collision, placement inside deployment zones,
//! and swept-path checks along maneuvers.

use crate::board::{Board, Seat};
use crate::geometry::{Footprint, Pose, Vec2};
use crate::ship::ShipId;

/// World-space corners of a ship's footprint rectangle. The rectangle hangs
/// BACKWARD from the front-center anchor: half the width to each side, the
/// full length behind.
pub fn footprint_corners(pose: Pose, fp: Footprint) -> [Vec2; 4] {
    let hw = fp.width / 2.0;
    [
        pose.local_to_world(Vec2::new(0.0, -hw)),
        pose.local_to_world(Vec2::new(0.0, hw)),
        pose.local_to_world(Vec2::new(-fp.length, hw)),
        pose.local_to_world(Vec2::new(-fp.length, -hw)),
    ]
}

/// Does a world point lie inside the ship's footprint? (Mouse picking.)
pub fn point_in_footprint(pose: Pose, fp: Footprint, p: Vec2) -> bool {
    let d = p - pose.anchor;
    let (s, c) = pose.heading.sin_cos();
    let local_x = c * d.x + s * d.y;
    let local_y = -s * d.x + c * d.y;
    (-fp.length..=0.0).contains(&local_x) && local_y.abs() <= fp.width / 2.0
}

fn project(corners: &[Vec2; 4], axis: Vec2) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for c in corners {
        let d = c.dot(axis);
        min = min.min(d);
        max = max.max(d);
    }
    (min, max)
}

/// Oriented-rectangle overlap via the separating-axis test.
pub fn obbs_overlap(a: &[Vec2; 4], b: &[Vec2; 4]) -> bool {
    for rect in [a, b] {
        for i in 0..2 {
            let axis = (rect[i + 1] - rect[i]).perp();
            let (amin, amax) = project(a, axis);
            let (bmin, bmax) = project(b, axis);
            if amax < bmin || bmax < amin {
                return false;
            }
        }
    }
    true
}

fn corners_within(corners: &[Vec2; 4], x0: f64, y0: f64, x1: f64, y1: f64) -> bool {
    corners.iter().all(|c| c.x >= x0 && c.x <= x1 && c.y >= y0 && c.y <= y1)
}

pub fn within_board(board: &Board, corners: &[Vec2; 4]) -> bool {
    corners_within(corners, 0.0, 0.0, board.width, board.height)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementError {
    /// Footprint pokes outside the seat's deployment band (or the board).
    OutOfZone,
    OverlapsShip(ShipId),
}

/// Is this a legal ship placement during setup? The whole footprint must sit
/// inside the seat's deployment zone and clear of every already-placed ship.
pub fn placement_legal(
    board: &Board,
    seat: Seat,
    pose: Pose,
    fp: Footprint,
    placed: &[(ShipId, Pose, Footprint)],
) -> Result<(), PlacementError> {
    let corners = footprint_corners(pose, fp);
    let (y0, y1) = board.deploy_zone(seat);
    if !corners_within(&corners, 0.0, y0, board.width, y1) {
        return Err(PlacementError::OutOfZone);
    }
    for &(id, other_pose, other_fp) in placed {
        if obbs_overlap(&corners, &footprint_corners(other_pose, other_fp)) {
            return Err(PlacementError::OverlapsShip(id));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathObstruction {
    /// The ship would leave the board; `at` is the first offending pose.
    OffBoard { at: Pose },
    /// The ship would overlap another ship along the way.
    ShipCollision { ship: ShipId, at: Pose },
}

/// Sweep a footprint along a sampled path (from `maneuver::sample_path`) and
/// report the first obstruction, if any. `others` must not include the
/// moving ship itself.
pub fn check_path(
    board: &Board,
    path: &[Pose],
    fp: Footprint,
    others: &[(ShipId, Pose, Footprint)],
) -> Option<PathObstruction> {
    let other_corners: Vec<(ShipId, [Vec2; 4])> = others
        .iter()
        .map(|&(id, pose, ofp)| (id, footprint_corners(pose, ofp)))
        .collect();
    for &pose in path {
        let corners = footprint_corners(pose, fp);
        if !within_board(board, &corners) {
            return Some(PathObstruction::OffBoard { at: pose });
        }
        for (id, oc) in &other_corners {
            if obbs_overlap(&corners, oc) {
                return Some(PathObstruction::ShipCollision { ship: *id, at: pose });
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::maneuver::{sample_path, Difficulty, Maneuver, Steer};
    use std::f64::consts::{FRAC_PI_2, FRAC_PI_4};

    const FP_SMALL: Footprint = Footprint { length: 1.0, width: 1.0 };

    fn board() -> Board {
        Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
    }

    #[test]
    fn corners_hang_backward_from_anchor() {
        // Facing +X from (5,5): footprint spans x in [4,5], y in [4.5,5.5].
        let c = footprint_corners(Pose::new(5.0, 5.0, 0.0), FP_SMALL);
        for p in c {
            assert!(p.x >= 4.0 - 1e-9 && p.x <= 5.0 + 1e-9);
            assert!(p.y >= 4.5 - 1e-9 && p.y <= 5.5 + 1e-9);
        }
    }

    #[test]
    fn point_picking_respects_rotation() {
        // Ship at (5,5) facing +Y: hull spans y in [4,5], x in [4.5,5.5].
        let pose = Pose::new(5.0, 5.0, FRAC_PI_2);
        assert!(point_in_footprint(pose, FP_SMALL, crate::geometry::Vec2::new(5.0, 4.5)));
        assert!(!point_in_footprint(pose, FP_SMALL, crate::geometry::Vec2::new(5.0, 5.5)));
        assert!(!point_in_footprint(pose, FP_SMALL, crate::geometry::Vec2::new(4.2, 4.5)));
    }

    #[test]
    fn separated_rects_do_not_overlap() {
        let a = footprint_corners(Pose::new(5.0, 5.0, 0.0), FP_SMALL);
        let b = footprint_corners(Pose::new(8.0, 5.0, 0.0), FP_SMALL);
        assert!(!obbs_overlap(&a, &b));
    }

    #[test]
    fn rotated_overlap_detected() {
        // Two footprints crossing at 45° through the same area.
        let a = footprint_corners(Pose::new(5.0, 5.0, FRAC_PI_4), FP_SMALL);
        let b = footprint_corners(Pose::new(5.0, 5.0, -FRAC_PI_4), FP_SMALL);
        assert!(obbs_overlap(&a, &b));
    }

    #[test]
    fn diagonal_neighbors_need_sat_not_aabb() {
        // Bounding boxes of two 45°-rotated rects overlap, but the rects
        // themselves don't — SAT must say "no collision".
        let a = footprint_corners(Pose::new(5.0, 5.0, FRAC_PI_4), FP_SMALL);
        let b = footprint_corners(Pose::new(6.4, 3.6, FRAC_PI_4), FP_SMALL);
        assert!(!obbs_overlap(&a, &b));
    }

    #[test]
    fn placement_inside_south_zone_ok() {
        // Anchor at y=2 facing north: hull spans y in [1,2], inside depth 3.
        let r = placement_legal(&board(), Seat::South, Pose::new(10.0, 2.0, FRAC_PI_2), FP_SMALL, &[]);
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn placement_outside_zone_rejected() {
        let r = placement_legal(&board(), Seat::South, Pose::new(10.0, 5.0, FRAC_PI_2), FP_SMALL, &[]);
        assert_eq!(r, Err(PlacementError::OutOfZone));
    }

    #[test]
    fn placement_on_teammate_rejected() {
        let placed = [(ShipId(7), Pose::new(10.0, 2.0, FRAC_PI_2), FP_SMALL)];
        let r = placement_legal(&board(), Seat::South, Pose::new(10.3, 2.0, FRAC_PI_2), FP_SMALL, &placed);
        assert_eq!(r, Err(PlacementError::OverlapsShip(ShipId(7))));
    }

    #[test]
    fn north_zone_is_far_band() {
        let r = placement_legal(&board(), Seat::North, Pose::new(10.0, 18.5, -FRAC_PI_2), FP_SMALL, &[]);
        assert_eq!(r, Ok(()));
    }

    #[test]
    fn path_off_board_detected() {
        // Straight 5 north from y=17 runs off the top edge.
        let man = Maneuver { steer: Steer::Straight, distance: 5, difficulty: Difficulty::Easy };
        let path = sample_path(Pose::new(10.0, 17.0, FRAC_PI_2), man).unwrap();
        let hit = check_path(&board(), &path, FP_SMALL, &[]);
        assert!(matches!(hit, Some(PathObstruction::OffBoard { .. })));
    }

    #[test]
    fn path_through_ship_detected_even_if_endpoints_clear() {
        // A ship sits mid-path; start and end poses are clear of it, so only
        // the swept check can catch the collision.
        let man = Maneuver { steer: Steer::Straight, distance: 5, difficulty: Difficulty::Easy };
        let start = Pose::new(5.0, 10.0, 0.0);
        let path = sample_path(start, man).unwrap();
        let blocker = (ShipId(3), Pose::new(7.5, 10.0, FRAC_PI_2), FP_SMALL);
        let hit = check_path(&board(), &path, FP_SMALL, &[blocker]);
        assert!(matches!(hit, Some(PathObstruction::ShipCollision { ship: ShipId(3), .. })));
    }

    #[test]
    fn clear_path_reports_nothing() {
        let man = Maneuver { steer: Steer::BankLeft, distance: 2, difficulty: Difficulty::Normal };
        let path = sample_path(Pose::new(10.0, 10.0, 0.0), man).unwrap();
        assert_eq!(check_path(&board(), &path, FP_SMALL, &[]), None);
    }
}
