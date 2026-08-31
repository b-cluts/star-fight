//! Front firing arc and range bands.
//!
//! Both starter ships use the standard Front Firing Arc: a 90° forward
//! cone (±45° of heading) originating at the center of the ship's square
//! base. Range is measured in three bands of exactly 100 mm each; combat
//! modifiers: +1 attack die at Range 1 (point-blank), +1 defense die for
//! the defender at Range 3 (long range).

use std::f64::consts::{FRAC_PI_4, PI, TAU};

use crate::geometry::{Footprint, Pose, Vec2};
use crate::templates::MM_PER_UNIT;

/// One range band is 100 mm — 2.5 game units.
pub const RANGE_BAND_UNITS: f64 = 100.0 / MM_PER_UNIT;

/// Weapons reach three bands (300 mm / 7.5 units).
pub const MAX_RANGE_BAND: u8 = 3;

/// Bullseye lane: 300 mm long × 15 mm wide (range-ruler dimensions),
/// centered straight ahead from the front edge of the base.
pub const BULLSEYE_LENGTH_UNITS: f64 = 300.0 / MM_PER_UNIT;
pub const BULLSEYE_WIDTH_UNITS: f64 = 15.0 / MM_PER_UNIT;

/// World-space corners of the ship's bullseye lane.
pub fn bullseye_corners(pose: Pose) -> [Vec2; 4] {
    let hw = BULLSEYE_WIDTH_UNITS / 2.0;
    [
        pose.local_to_world(Vec2::new(0.0, -hw)),
        pose.local_to_world(Vec2::new(0.0, hw)),
        pose.local_to_world(Vec2::new(BULLSEYE_LENGTH_UNITS, hw)),
        pose.local_to_world(Vec2::new(BULLSEYE_LENGTH_UNITS, -hw)),
    ]
}

/// Is any part of the defender's base inside the attacker's bullseye?
/// (Defenders in the bullseye cannot spend focus or evade tokens.)
pub fn in_bullseye(attacker: Pose, defender_corners: &[Vec2; 4]) -> bool {
    crate::rules::obbs_overlap(&bullseye_corners(attacker), defender_corners)
}

/// Center of the ship's base (the firing arc's origin).
pub fn base_center(pose: Pose, fp: Footprint) -> Vec2 {
    pose.local_to_world(Vec2::new(-fp.length / 2.0, 0.0))
}

/// Is `target` inside the 90° forward cone from the base center?
pub fn in_front_arc(pose: Pose, fp: Footprint, target: Vec2) -> bool {
    let d = target - base_center(pose, fp);
    if d.x == 0.0 && d.y == 0.0 {
        return true;
    }
    let rel = (d.y.atan2(d.x) - pose.heading + PI).rem_euclid(TAU) - PI;
    rel.abs() <= FRAC_PI_4 + 1e-12
}

fn point_seg_distance(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    let t = if len2 == 0.0 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
    let c = Vec2::new(a.x + ab.x * t, a.y + ab.y * t);
    let d = p - c;
    (d.x * d.x + d.y * d.y).sqrt()
}

/// Closest distance between two base rectangles (0 if they overlap).
/// This is the "any point to any point" measurement of the range ruler.
pub fn base_distance(a: &[Vec2; 4], b: &[Vec2; 4]) -> f64 {
    if crate::rules::obbs_overlap(a, b) {
        return 0.0;
    }
    // Non-overlapping convex quads: the minimum is between a vertex of one
    // and an edge of the other.
    let mut min = f64::INFINITY;
    for i in 0..4 {
        for j in 0..4 {
            let (a1, a2) = (a[i], a[(i + 1) % 4]);
            let (b1, b2) = (b[j], b[(j + 1) % 4]);
            min = min
                .min(point_seg_distance(a[i], b1, b2))
                .min(point_seg_distance(b[j], a1, a2))
                .min(point_seg_distance(a2, b1, b2))
                .min(point_seg_distance(b2, a1, a2));
        }
    }
    min
}

/// Range band (1..=3) between two bases, or None beyond range 3.
/// Touching bases are range 1.
pub fn range_band_between(a: &[Vec2; 4], b: &[Vec2; 4]) -> Option<u8> {
    let d = base_distance(a, b);
    (1..=MAX_RANGE_BAND).find(|&band| d <= band as f64 * RANGE_BAND_UNITS)
}

/// Attack dice thrown at a given range band: +1 at point-blank Range 1.
pub fn attack_dice(base_dice: u8, band: u8) -> u8 {
    if band == 1 { base_dice + 1 } else { base_dice }
}

/// Extra defense dice granted to the defender: +1 at long Range 3.
pub fn defense_bonus(band: u8) -> u8 {
    if band >= MAX_RANGE_BAND { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    const FP: Footprint = Footprint { length: 1.0, width: 1.0 };

    #[test]
    fn band_is_2_5_units() {
        assert_eq!(RANGE_BAND_UNITS, 2.5);
    }

    #[test]
    fn arc_covers_forward_cone_only() {
        // Ship at origin facing +X; base center at (-0.5, 0).
        let pose = Pose::new(0.0, 0.0, 0.0);
        assert!(in_front_arc(pose, FP, Vec2::new(3.0, 0.0))); // dead ahead
        assert!(in_front_arc(pose, FP, Vec2::new(2.5, 2.9))); // just under 45°
        assert!(!in_front_arc(pose, FP, Vec2::new(2.5, 3.1))); // just over 45°
        assert!(!in_front_arc(pose, FP, Vec2::new(-4.0, 0.0))); // behind
        assert!(!in_front_arc(pose, FP, Vec2::new(-0.5, 2.0))); // abeam
    }

    #[test]
    fn arc_rotates_with_heading() {
        let pose = Pose::new(5.0, 5.0, FRAC_PI_2); // facing +Y
        assert!(in_front_arc(pose, FP, Vec2::new(5.0, 9.0)));
        assert!(!in_front_arc(pose, FP, Vec2::new(9.0, 5.0)));
    }

    #[test]
    fn range_bands_between_bases() {
        use crate::rules::footprint_corners;
        let a = footprint_corners(Pose::new(5.0, 5.0, 0.0), FP);
        // Hulls: a spans x 4..5 at y 4.5..5.5.
        let touching = footprint_corners(Pose::new(6.0, 5.0, 0.0), FP); // x 5..6
        let r1 = footprint_corners(Pose::new(8.0, 5.0, 0.0), FP); // gap 2.0
        let r2 = footprint_corners(Pose::new(9.0, 5.0, 0.0), FP); // gap 3.0
        let r3 = footprint_corners(Pose::new(12.0, 5.0, 0.0), FP); // gap 6.0
        let out = footprint_corners(Pose::new(14.0, 5.0, 0.0), FP); // gap 8.0
        assert_eq!(range_band_between(&a, &touching), Some(1));
        assert_eq!(range_band_between(&a, &r1), Some(1));
        assert_eq!(range_band_between(&a, &r2), Some(2));
        assert_eq!(range_band_between(&a, &r3), Some(3));
        assert_eq!(range_band_between(&a, &out), None);
        assert_eq!(range_band_between(&a, &a), Some(1)); // overlap = 0
    }

    #[test]
    fn bullseye_is_a_narrow_centered_lane() {
        use crate::rules::footprint_corners;
        let attacker = Pose::new(0.0, 0.0, 0.0); // facing +X
        let dead_ahead = footprint_corners(Pose::new(5.0, 0.0, 0.0), FP);
        let offset = footprint_corners(Pose::new(5.0, 1.2, 0.0), FP);
        let beyond = footprint_corners(Pose::new(8.6, 0.0, 0.0), FP);
        assert!(in_bullseye(attacker, &dead_ahead));
        assert!(!in_bullseye(attacker, &offset), "off-axis is outside the lane");
        assert!(!in_bullseye(attacker, &beyond), "past 7.5 units is outside");
    }

    #[test]
    fn range_modifiers() {
        // TIE (2 dice): 3 at range 1, 2 otherwise. X-Wing (3): 4 at range 1.
        assert_eq!(attack_dice(2, 1), 3);
        assert_eq!(attack_dice(2, 2), 2);
        assert_eq!(attack_dice(3, 1), 4);
        assert_eq!(attack_dice(3, 3), 3);
        assert_eq!(defense_bonus(1), 0);
        assert_eq!(defense_bonus(2), 0);
        assert_eq!(defense_bonus(3), 1);
    }
}
