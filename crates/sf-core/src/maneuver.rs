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
    /// 90° arc on the turn templates. Speeds 1..=3.
    TurnLeft,
    TurnRight,
    /// Ahead 1, rotate 180° about the anchor, ahead 1. Ignores `distance`.
    UTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Difficulty {
    Easy,
    Normal,
    Hard,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maneuver {
    pub steer: Steer,
    /// Speed: 1..=5 for Straight, 1..=3 for banks/turns; ignored by UTurn.
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
        Steer::UTurn => {
            let ahead = templates::straight_length(1).expect("speed 1 is always valid");
            vec![Segment::Line(ahead), Segment::Rotate(PI), Segment::Line(ahead)]
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
    fn left_and_right_are_mirrors() {
        for (l, r, max) in [
            (Steer::BankLeft, Steer::BankRight, 3),
            (Steer::TurnLeft, Steer::TurnRight, 3),
        ] {
            for speed in 1..=max {
                let pl = apply(Pose::new(0.0, 0.0, 0.0), m(l, speed)).unwrap();
                let pr = apply(Pose::new(0.0, 0.0, 0.0), m(r, speed)).unwrap();
                assert!(approx(pl.anchor.x, pr.anchor.x));
                assert!(approx(pl.anchor.y, -pr.anchor.y));
                assert!(approx(pl.heading, -pr.heading));
            }
        }
    }

    #[test]
    fn uturn_returns_anchor_flipped() {
        // Ahead 1, flip, ahead 1: the anchor comes back to the start with
        // heading reversed — the hull (which trails the anchor) ends one
        // ship-length AHEAD of where it began, now facing backward.
        let p = apply(Pose::new(2.0, 3.0, 1.0), m(Steer::UTurn, 1)).unwrap();
        assert!(approx(p.anchor.x, 2.0));
        assert!(approx(p.anchor.y, 3.0));
        assert!(approx(p.heading, 1.0 + PI));
    }

    #[test]
    fn bad_speeds_rejected() {
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::Straight, 6)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::TurnLeft, 4)).is_err());
        assert!(apply(Pose::new(0.0, 0.0, 0.0), m(Steer::BankRight, 0)).is_err());
    }

    #[test]
    fn sampled_path_ends_at_final_pose() {
        for steer in [Steer::Straight, Steer::BankLeft, Steer::TurnRight, Steer::UTurn] {
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
