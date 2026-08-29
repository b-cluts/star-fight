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
