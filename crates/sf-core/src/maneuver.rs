use serde::{Deserialize, Serialize};
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};

use crate::geometry::Pose;
use crate::templates;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ManeuverSetId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Steer {
    /// No heading change. Speeds 1..=5.
    Straight,
    /// 45° arc on the bank templates. Speeds 1..=3.
    BankLeft,
    BankRight,
    /// 90° arc on the turn templates. Speeds 1..=4 (4 extrapolated).
    TurnLeft,
    TurnRight,
    /// Tallon roll: fly a 90° turn template, then rotate a further 90° in
    /// the same direction — ends facing 180° from start, displaced to the
    /// flank. Speeds as per turn templates.
    TallonLeft,
    TallonRight,
    /// Koiogran turn: fly a straight template at `distance`, then flip
    /// 180° in place — ends facing backward. Speeds 1..=5 geometrically;
    /// which speeds a ship may fly comes from its dial.
    KTurn,
    /// Segnor's loop: fly a bank template, then flip 180° in place — ends
    /// facing back the way it came, offset to the flank. Speeds as banks.
    SegnorLeft,
    SegnorRight,
}

/// Maneuver difficulty, color-coded as on the physical dials. Stress rules
/// (enforced in `game.rs` once tokens exist): flying a Blue/Easy maneuver
/// removes one stress token; flying a Red/Hard maneuver adds one; a
/// stressed ship may not select Red maneuvers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Difficulty {
    /// Blue on the dial.
    Easy,
    /// White on the dial.
    Normal,
    /// Red on the dial.
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maneuver {
    pub steer: Steer,
    /// Speed: 1..=5 for Straight and KTurn, 1..=3 for banks,
    /// 1..=4 for turns and Tallon rolls.
    pub distance: u8,
    pub difficulty: Difficulty,
}

/// The maneuvers available to one agility tier of ship,
/// loaded from `assets/data/maneuvers.ron`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManeuverSet {
    pub id: ManeuverSetId,
    pub maneuvers: Vec<Maneuver>,
}

/// One piece of a maneuver's path, in the ship's local frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Segment {
    /// Advance along the current heading.
    Line(f64),
    /// Circular arc; `sweep` signed radians, positive = left.
    Arc { radius: f64, sweep: f64 },
    /// Turn in place about the anchor.
    Rotate(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BadManeuver {
    pub steer: Steer,
    pub distance: u8,
}

impl std::fmt::Display for BadManeuver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid maneuver: {:?} at speed {}", self.steer, self.distance)
    }
}

impl std::error::Error for BadManeuver {}

/// Expand a maneuver into its path segments (the "template").
pub fn segments(m: Maneuver) -> Result<Vec<Segment>, BadManeuver> {
    let bad = BadManeuver { steer: m.steer, distance: m.distance };
    Ok(match m.steer {
        Steer::Straight => vec![Segment::Line(templates::straight_length(m.distance).ok_or(bad)?)],
        Steer::BankLeft | Steer::BankRight => {
            let radius = templates::bank_radius(m.distance).ok_or(bad)?;
            let sign = if m.steer == Steer::BankLeft { 1.0 } else { -1.0 };
            vec![Segment::Arc { radius, sweep: sign * FRAC_PI_4 }]
        }
        Steer::TurnLeft | Steer::TurnRight => {
            let radius = templates::turn_radius(m.distance).ok_or(bad)?;
            let sign = if m.steer == Steer::TurnLeft { 1.0 } else { -1.0 };
            vec![Segment::Arc { radius, sweep: sign * FRAC_PI_2 }]
        }
        Steer::TallonLeft | Steer::TallonRight => {
            let radius = templates::turn_radius(m.distance).ok_or(bad)?;
            let sign = if m.steer == Steer::TallonLeft { 1.0 } else { -1.0 };
            vec![
                Segment::Arc { radius, sweep: sign * FRAC_PI_2 },
                Segment::Rotate(sign * FRAC_PI_2),
            ]
        }
        Steer::KTurn => {
            let ahead = templates::straight_length(m.distance).ok_or(bad)?;
            vec![Segment::Line(ahead), Segment::Rotate(PI)]
        }
        Steer::SegnorLeft | Steer::SegnorRight => {
            let radius = templates::bank_radius(m.distance).ok_or(bad)?;
            let sign = if m.steer == Steer::SegnorLeft { 1.0 } else { -1.0 };
            vec![Segment::Arc { radius, sweep: sign * FRAC_PI_4 }, Segment::Rotate(PI)]
        }
    })
}

pub fn apply_segment(pose: Pose, seg: Segment) -> Pose {
    match seg {
        Segment::Line(len) => pose.advanced(len),
        Segment::Arc { radius, sweep } => pose.arced(radius, sweep),
        Segment::Rotate(angle) => pose.rotated(angle),
    }
}

/// Final pose after flying `m` from `start`. Pure and deterministic — the
/// client uses it for ghost-ship previews, the server to resolve the turn.
pub fn apply(start: Pose, m: Maneuver) -> Result<Pose, BadManeuver> {
    Ok(segments(m)?.into_iter().fold(start, apply_segment))
}

/// Arc-length step between path samples, in game units.
pub const SAMPLE_STEP: f64 = 0.1;
/// Heading step between samples while rotating in place.
pub const SAMPLE_STEP_ANGLE: f64 = PI / 12.0;

/// Poses along the maneuver's path, starting at `start` and ending exactly
/// at the final pose. Used for collision sweeps and client animation.
pub fn sample_path(start: Pose, m: Maneuver) -> Result<Vec<Pose>, BadManeuver> {
    let mut path = vec![start];
    let mut pose = start;
    for seg in segments(m)? {
        let steps = match seg {
            Segment::Line(len) => (len / SAMPLE_STEP).ceil() as u32,
            Segment::Arc { radius, sweep } => (radius * sweep.abs() / SAMPLE_STEP).ceil() as u32,
            Segment::Rotate(angle) => (angle.abs() / SAMPLE_STEP_ANGLE).ceil() as u32,
        }
        .max(1);
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let partial = match seg {
                Segment::Line(len) => Segment::Line(len * t),
                Segment::Arc { radius, sweep } => Segment::Arc { radius, sweep: sweep * t },
                Segment::Rotate(angle) => Segment::Rotate(angle * t),
            };
            path.push(apply_segment(pose, partial));
        }
        pose = apply_segment(pose, seg);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    fn m(steer: Steer, distance: u8) -> Maneuver {
        Maneuver { steer, distance, difficulty: Difficulty::Normal }
    }

    #[test]
    fn straight_moves_ahead() {
        let p = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::Straight, 3)).unwrap();
        assert!(approx(p.anchor.x, 3.0));
        assert!(approx(p.anchor.y, 0.0));
        assert!(approx(p.heading, 0.0));
    }

    #[test]
    fn turn_1_left_matches_template() {
        // Centerline radius 0.875: end at (0.875, 0.875), facing left.
        let p = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::TurnLeft, 1)).unwrap();
        assert!(approx(p.anchor.x, 0.875));
        assert!(approx(p.anchor.y, 0.875));
        assert!(approx(p.heading, FRAC_PI_2));
    }

    #[test]
    fn bank_2_right_matches_template() {
        // Radius 3.25, 45° right.
        let p = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::BankRight, 2)).unwrap();
        let r = 3.25f64;
        assert!(approx(p.anchor.x, r * FRAC_PI_4.sin()));
        assert!(approx(p.anchor.y, -(r * (1.0 - FRAC_PI_4.cos()))));
        assert!(approx(p.heading, -FRAC_PI_4));
    }

    #[test]
    fn segnor_loop_is_bank_then_flip() {
        let bank = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::BankLeft, 3)).unwrap();
        let p = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::SegnorLeft, 3)).unwrap();
        assert!(approx(p.anchor.x, bank.anchor.x));
        assert!(approx(p.anchor.y, bank.anchor.y));
        // Bank left = +45°, then a half turn: heading ends at 45° - 180°.
        let want = (FRAC_PI_4 + PI).rem_euclid(2.0 * PI);
        assert!(approx(p.heading.rem_euclid(2.0 * PI), want), "heading {}", p.heading);
    }

    #[test]
    fn left_and_right_are_mirrors() {
        for (l, r, max) in [
            (Steer::BankLeft, Steer::BankRight, 3),
            (Steer::TurnLeft, Steer::TurnRight, 4),
            (Steer::TallonLeft, Steer::TallonRight, 4),
            (Steer::SegnorLeft, Steer::SegnorRight, 3),
        ] {
            for speed in 1..=max {
                let pl = apply(Pose::new(0.0, 0.0, 0.0), m(l, speed)).unwrap();
                let pr = apply(Pose::new(0.0, 0.0, 0.0), m(r, speed)).unwrap();
                assert!(approx(pl.anchor.x, pr.anchor.x));
                assert!(approx(pl.anchor.y, -pr.anchor.y));
                assert!(
                    approx(pl.heading.rem_euclid(2.0 * PI), (-pr.heading).rem_euclid(2.0 * PI)),
                    "left {l:?} speed {speed}: pl.heading={}, pr.heading={}",
                    pl.heading,
                    pr.heading
                );
            }
        }
    }

    #[test]
    fn kturn_flies_straight_then_flips() {
        // Koiogran 3 from origin facing +X: anchor ends 3 ahead, heading
        // reversed; the hull (which trails the anchor) flips to sit ahead
        // of the anchor, facing back the way it came.
        let p = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::KTurn, 3)).unwrap();
        assert!(approx(p.anchor.x, 3.0));
        assert!(approx(p.anchor.y, 0.0));
        assert!(approx(p.heading, PI));
    }

    #[test]
    fn tallon_roll_ends_flipped_on_the_flank() {
        // Tallon left 3: 90° left arc (r = 2.25) to (2.25, 2.25) facing
        // +Y, then a further 90° left in place — net heading 180°.
        let p = apply(Pose::new(0.0, 0.0, 0.0), m(Steer::TallonLeft, 3)).unwrap();
        assert!(approx(p.anchor.x, 2.25));
        assert!(approx(p.anchor.y, 2.25));
        assert!(approx(p.heading, PI));
    }

    #[test]
    fn bad_speeds_rejected() {
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::Straight, 6)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::TurnLeft, 5)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::TallonRight, 5)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::BankRight, 0)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::KTurn, 6)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::SegnorLeft, 4)).is_err());
    }

    #[test]
    fn sampled_path_ends_at_final_pose() {
        for steer in [Steer::Straight, Steer::BankLeft, Steer::TurnRight, Steer::KTurn] {
            let man = m(steer, 2);
            let start = Pose::new(1.0, -2.0, 0.4);
            let path = sample_path(start, man).unwrap();
            let end = apply(start, man).unwrap();
            let last = *path.last().unwrap();
            assert_eq!(path[0], start);
            assert!(approx(last.anchor.x, end.anchor.x));
            assert!(approx(last.anchor.y, end.anchor.y));
            assert!(approx(last.heading, end.heading));
            assert!(path.len() > 2);
        }
    }
}
