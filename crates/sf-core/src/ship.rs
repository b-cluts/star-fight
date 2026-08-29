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
    /// 40 × 40 mm base — 1 × 1 game units (matches a speed-1 straight).
    Small,
    /// 60 × 60 mm base — 1.5 × 1.5 game units.
    Medium,
    /// 80 × 80 mm base — 2 × 2 game units (matches a speed-2 straight).
    Large,
    /// 80 × 192 mm base — 2 wide × 4.8 long game units.
    Huge,
}

impl SizeClass {
    /// Standard base footprint (length along heading × width) in game units.
    pub fn base_footprint(self) -> Footprint {
        match self {
            SizeClass::Small => Footprint { length: 1.0, width: 1.0 },
            SizeClass::Medium => Footprint { length: 1.5, width: 1.5 },
            SizeClass::Large => Footprint { length: 2.0, width: 2.0 },
            SizeClass::Huge => Footprint { length: 4.8, width: 2.0 },
        }
    }
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
