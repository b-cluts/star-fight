use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ManeuverSetId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Steer {
    /// No heading change.
    Straight,
    /// Gentle bank (total heading change tunable, e.g. 22.5°).
    SlightLeft,
    SlightRight,
    /// 45° total heading change.
    HardLeft,
    HardRight,
    /// 90° total heading change.
    TurnLeft,
    TurnRight,
    /// Ahead 1, rotate 180°, ahead 1. Ignores `distance`.
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
    /// 1..=3 game units (UTurn ignores this).
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
