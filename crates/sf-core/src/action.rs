//! Actions (core rules p.8-9): each ship may perform ONE action
//! immediately after executing its maneuver. Which actions a ship may
//! take comes from its class action bar. A stressed ship cannot perform
//! actions; neither can a ship that bumped during its move.

use serde::{Deserialize, Serialize};

use crate::geometry::{Footprint, Pose, Vec2};
use crate::ship::ShipId;

/// An entry on a ship's action bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    Focus,
    TargetLock,
    Evade,
    BarrelRoll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Left,
    Right,
}

/// A concrete action choice, planned during the Planning phase and
/// executed right after the ship's maneuver.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlannedAction {
    /// Choosing not to act is always allowed.
    Pass,
    /// Gain a focus token (spend in combat: eyes → hits or evades).
    Focus,
    /// Gain an evade token (spend in combat: cancel one damage).
    Evade,
    /// Shift laterally by a straight-1 template, heading unchanged.
    BarrelRoll(Side),
    /// Lock a target at range 1-3 (any point to any point, 360°).
    TargetLock(ShipId),
}

impl PlannedAction {
    /// The bar entry this action requires (None = always available).
    pub fn kind(&self) -> Option<ActionKind> {
        match self {
            PlannedAction::Pass => None,
            PlannedAction::Focus => Some(ActionKind::Focus),
            PlannedAction::Evade => Some(ActionKind::Evade),
            PlannedAction::BarrelRoll(_) => Some(ActionKind::BarrelRoll),
            PlannedAction::TargetLock(_) => Some(ActionKind::TargetLock),
        }
    }
}

/// What became of a ship's planned action during resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionResult {
    Performed,
    /// Stressed ships cannot perform actions.
    SkippedStressed,
    /// A ship that bumped loses its action.
    SkippedBumped,
    /// The action was impossible (barrel roll blocked, lock out of range).
    Failed,
}

/// Barrel-roll destination: one end of a straight-1 template against the
/// side of the base, ship placed at the opposite end — so the base shifts
/// laterally by (template length + base width), facing unchanged.
/// (Simplification vs the tabletop: no fore/aft slide along the template.)
pub fn barrel_roll_pose(pose: Pose, fp: Footprint, side: Side) -> Pose {
    let shift = 1.0 + fp.width;
    let sign = match side {
        Side::Left => 1.0,
        Side::Right => -1.0,
    };
    Pose {
        anchor: pose.local_to_world(Vec2::new(0.0, sign * shift)),
        heading: pose.heading,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    #[test]
    fn barrel_roll_shifts_one_template_plus_base_width() {
        let fp = Footprint { length: 1.0, width: 1.0 };
        // Facing +Y: "left" is -X.
        let p = barrel_roll_pose(Pose::new(10.0, 5.0, FRAC_PI_2), fp, Side::Left);
        assert!((p.anchor.x - 8.0).abs() < 1e-9, "{}", p.anchor.x);
        assert!((p.anchor.y - 5.0).abs() < 1e-9);
        assert!((p.heading - FRAC_PI_2).abs() < 1e-9);
        let r = barrel_roll_pose(Pose::new(10.0, 5.0, FRAC_PI_2), fp, Side::Right);
        assert!((r.anchor.x - 12.0).abs() < 1e-9);
    }

    #[test]
    fn pass_needs_no_bar_entry() {
        assert_eq!(PlannedAction::Pass.kind(), None);
        assert_eq!(PlannedAction::Focus.kind(), Some(ActionKind::Focus));
        assert_eq!(
            PlannedAction::TargetLock(ShipId(3)).kind(),
            Some(ActionKind::TargetLock)
        );
    }
}
