use serde::{Deserialize, Serialize};

use crate::geometry::{Footprint, Pose};
use crate::maneuver::ManeuverSetId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShipClassId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ShipId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PlayerId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SizeClass {
    Small,
    Medium,
    Large,
}

/// A kind of ship, loaded from `assets/data/ships.ron`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipClass {
    pub id: ShipClassId,
    pub name: String,
    pub size: SizeClass,
    pub footprint: Footprint,
    pub maneuver_set: ManeuverSetId,
    /// Sprite asset path, e.g. "ships/scout.png".
    pub sprite: String,
    /// Pixel coordinates of the front-center point in the sprite.
    pub anchor_px: (u32, u32),
}

/// One ship in play.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipState {
    pub id: ShipId,
    pub owner: PlayerId,
    pub class: ShipClassId,
    pub pose: Pose,
}
