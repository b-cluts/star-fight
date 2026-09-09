//! Actions (core rules p.8-9): each ship may perform ONE action
//! immediately after executing its maneuver. Which actions a ship may
//! take comes from its class action bar. A stressed ship cannot perform
//! actions; neither can a ship that bumped during its move.

use serde::{Deserialize, Serialize};

use crate::geometry::{Footprint, Pose, Vec2};
use crate::ship::ShipId;
use crate::upgrade::UpgradeId;

/// An entry on a ship's action bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActionKind {
    Focus,
    TargetLock,
    Evade,
    BarrelRoll,
    Boost,
}

/// Boost template choice: straight-1 or bank-1 left/right; "Blue Ace"
/// may also use the turn-1 templates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoostDir {
    Straight,
    BankLeft,
    BankRight,
    TurnLeft,
    TurnRight,
}

/// Why a ship may plan a second action, and what it may be.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecondActionKind {
    /// Push the Limit: a free action from the bar after the first one,
    /// then a stress token.
    FreeBarAction,
    /// Darth Vader: two actions in the Perform Action step.
    TwoActions,
    /// "Snap" Wexley: a free boost after a 2-, 3- or 4-speed maneuver
    /// when not touching a ship — before the Perform Action step.
    BoostAfterMove,
    /// Jake Farrell: a free boost or barrel roll after a focus action.
    RepositionAfterFocus,
    /// BB-8: a free barrel roll when a green maneuver is revealed,
    /// before the ship moves.
    RollOnGreenReveal,
    /// Turr Phennir: a free boost or barrel roll after performing an
    /// attack (resolved in the Combat phase, recorded on the attack).
    RepositionAfterAttack,
}

/// What a ship may plan beyond the basic action bar (own ships only).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActionExtras {
    pub second: Option<SecondActionKind>,
    /// "Blue Ace": boosts may use the turn-1 templates.
    pub turn_boost: bool,
    /// "Zeta Ace": barrel rolls may use the straight-2 template.
    pub far_roll: bool,
    /// Cards whose "Action:" can be taken (Marksmanship, Rage, Expose,
    /// R2-F2) via `PlannedAction::CardAction`.
    pub card_actions: Vec<UpgradeId>,
    /// Lieutenant Lorrir: barrel rolls may use the bank-1 templates for a
    /// stress token (`PlannedAction::BarrelRollBank`).
    pub bank_roll: bool,
    /// Expert Handling: a barrel roll without the icon on the bar (stress
    /// if it is missing), removing one enemy target lock afterwards.
    pub expert_roll: bool,
}

/// The 1-speed maneuver a boost flies. Boosting does NOT count as
/// executing a maneuver (no stress interaction, no dial color).
pub fn boost_maneuver(dir: BoostDir) -> crate::maneuver::Maneuver {
    use crate::maneuver::{Difficulty, Maneuver, Steer};
    let steer = match dir {
        BoostDir::Straight => Steer::Straight,
        BoostDir::BankLeft => Steer::BankLeft,
        BoostDir::BankRight => Steer::BankRight,
        BoostDir::TurnLeft => Steer::TurnLeft,
        BoostDir::TurnRight => Steer::TurnRight,
    };
    Maneuver { steer, distance: 1, difficulty: Difficulty::Normal }
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
    /// Fly a straight-1 or bank-1 template forward (not a maneuver).
    Boost(BoostDir),
    /// Lock a target at range 1-3 (any point to any point, 360°).
    TargetLock(ShipId),
    /// Card action: discard this mine card to drop its token(s) behind
    /// the ship (Proximity Mines, Cluster Mines, Conner Net).
    DropMine(UpgradeId),
    /// "Zeta Ace": a barrel roll with the straight-2 template.
    BarrelRollFar(Side),
    /// A card's "Action:" (Marksmanship, Rage, Expose, R2-F2): its effect
    /// lasts for the round.
    CardAction(UpgradeId),
    /// A card action aimed at an obstacle (Seismic Torpedo): the card and
    /// the obstacle id. Only resolved as the main action of the turn.
    CardActionAt(UpgradeId, u32),
    /// Lieutenant Lorrir: a barrel roll with a bank-1 template to `side`,
    /// bending toward the ship's front (`true`) or rear, for a stress.
    BarrelRollBank(Side, bool),
}

impl PlannedAction {
    /// The bar entry this action requires (None = always available).
    pub fn kind(&self) -> Option<ActionKind> {
        match self {
            PlannedAction::Pass => None,
            PlannedAction::Focus => Some(ActionKind::Focus),
            PlannedAction::Evade => Some(ActionKind::Evade),
            PlannedAction::BarrelRoll(_) => Some(ActionKind::BarrelRoll),
            PlannedAction::Boost(_) => Some(ActionKind::Boost),
            PlannedAction::TargetLock(_) => Some(ActionKind::TargetLock),
            PlannedAction::DropMine(_) => None,
            PlannedAction::BarrelRollFar(_) => Some(ActionKind::BarrelRoll),
            PlannedAction::CardAction(_) => None,
            PlannedAction::CardActionAt(..) => None,
            PlannedAction::BarrelRollBank(..) => Some(ActionKind::BarrelRoll),
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
    /// A Damaged Sensor Array denies all action-bar actions.
    SkippedDamaged,
    /// The action was impossible (barrel roll blocked, lock out of range).
    Failed,
    /// Caught in a Conner Net: the Perform Action step is skipped.
    SkippedNetted,
    /// Overlapped an asteroid while moving: no action this round.
    SkippedObstacle,
}

/// Barrel-roll destination: one end of a straight-1 template against the
/// side of the base, ship placed at the opposite end — so the base shifts
/// laterally by (template length + base width), facing unchanged.
/// (Simplification vs the tabletop: no fore/aft slide along the template.)
pub fn barrel_roll_pose(pose: Pose, fp: Footprint, side: Side) -> Pose {
    barrel_roll_pose_with(pose, fp, side, 1.0)
}

/// Barrel roll with a template of `template` units ("Zeta Ace": 2).
pub fn barrel_roll_pose_with(pose: Pose, fp: Footprint, side: Side, template: f64) -> Pose {
    let shift = template + fp.width;
    let sign = match side {
        Side::Left => 1.0,
        Side::Right => -1.0,
    };
    Pose { anchor: pose.local_to_world(Vec2::new(0.0, sign * shift)), heading: pose.heading }
}

/// Barrel roll with a bank-1 template (Lieutenant Lorrir): the template
/// starts at the middle of the base's side, pointing outward, and bends
/// 45° toward the ship's front (`forward`) or rear. The ship is then set
/// against the template's far end, its side flush with it, which turns
/// its heading 45° — away from the roll side when bending forward,
/// toward it when bending backward.
pub fn barrel_roll_bank_pose(pose: Pose, fp: Footprint, side: Side, forward: bool) -> Pose {
    use std::f64::consts::{FRAC_PI_2, FRAC_PI_4};
    let sign = match side {
        Side::Left => 1.0,
        Side::Right => -1.0,
    };
    let start = Pose {
        anchor: pose.local_to_world(Vec2::new(-fp.length / 2.0, sign * fp.width / 2.0)),
        heading: pose.heading + sign * FRAC_PI_2,
    };
    // Seen from the outward-pointing template, "toward the front" is a
    // turn against the roll side.
    let sweep = if forward { -sign * FRAC_PI_4 } else { sign * FRAC_PI_4 };
    let radius = crate::templates::bank_radius(1).expect("bank 1 exists");
    let end = start.arced(radius, sweep);
    let heading = end.heading - sign * FRAC_PI_2;
    let center = end.advanced(fp.width / 2.0).anchor;
    let anchor = Pose { anchor: center, heading }.advanced(fp.length / 2.0).anchor;
    Pose { anchor, heading }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    #[test]
    fn bank_roll_shifts_sideways_and_turns_45_degrees() {
        use std::f64::consts::FRAC_PI_4;
        let fp = Footprint { length: 1.0, width: 1.0 };
        let start = Pose::new(10.0, 5.0, FRAC_PI_2);
        // Facing +Y, rolling left (-X) with the template bending forward:
        // the nose ends up turned right, further left and ahead.
        let p = barrel_roll_bank_pose(start, fp, Side::Left, true);
        assert!((p.heading - FRAC_PI_4).abs() < 1e-9, "{}", p.heading);
        assert!(p.anchor.x < 9.0 && p.anchor.x > 7.0, "{:?}", p.anchor);
        assert!(p.anchor.y > 5.0, "{:?}", p.anchor);
        // Bending backward: nose turned left, ending behind the start.
        let q = barrel_roll_bank_pose(start, fp, Side::Left, false);
        assert!((q.heading - 3.0 * FRAC_PI_4).abs() < 1e-9, "{}", q.heading);
        assert!(q.anchor.x < 9.0, "{:?}", q.anchor);
        assert!(q.anchor.y < 5.0, "{:?}", q.anchor);
        // Mirror: rolling right bends the other way.
        let r = barrel_roll_bank_pose(start, fp, Side::Right, true);
        assert!((r.heading - 3.0 * FRAC_PI_4).abs() < 1e-9, "{}", r.heading);
        assert!((r.anchor.x - 10.0) + (p.anchor.x - 10.0) < 1e-9, "{:?} {:?}", r, p);
    }

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
        assert_eq!(PlannedAction::TargetLock(ShipId(3)).kind(), Some(ActionKind::TargetLock));
    }
}
