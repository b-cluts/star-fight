//! Bombs and mines (First Edition expansion rules). Dial-reveal bombs
//! (Proton Bombs, Seismic Charges, Ion Bombs, Thermal Detonators) are
//! dropped BEFORE the ship moves and detonate at the end of the
//! Activation phase against every ship within Range 1 of the token.
//! Mines (Proximity Mines, Cluster Mines, Conner Net) are dropped as an
//! action after moving and detonate when a ship's base or maneuver
//! template overlaps them, hurting only that ship.
//!
//! Drop placement (both kinds): the straight-1 template is placed against
//! the ship's rear guides and the token touches the far end of it.

use serde::{Deserialize, Serialize};

use crate::geometry::{Footprint, Pose, Vec2};
use crate::rules;
use crate::ship::{PlayerId, ShipId};
use crate::templates;
use crate::upgrade::{UpgradeEffect, UpgradeId};

/// Bomb token footprint: modelled as a small-base-sized square.
pub const TOKEN_FOOTPRINT: Footprint = Footprint { length: 1.0, width: 1.0 };

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BombKind {
    /// Each ship at Range 1 is dealt 1 faceup Damage card.
    Proton,
    /// Each ship at Range 1 suffers 1 damage.
    Seismic,
    /// Each ship at Range 1 receives 2 ion tokens.
    Ion,
    /// Each ship at Range 1 suffers 1 damage and receives 1 stress token.
    Thermal,
    /// The crossing ship rolls 3 attack dice and suffers all damage rolled.
    ProximityMine,
    /// Three tokens side by side; each rolls 2 attack dice on the ship.
    ClusterMine,
    /// The crossing ship suffers 1 damage, receives 2 ion tokens and
    /// skips its Perform Action step.
    ConnerNet,
    /// A Seismic Torpedo going off on an obstacle: each ship at Range 1
    /// rolls 1 attack die and suffers any damage or critical rolled. Never
    /// a token on the board; only appears in a `Detonation`.
    SeismicTorpedo,
}

impl BombKind {
    pub fn from_effect(effect: UpgradeEffect) -> Option<BombKind> {
        Some(match effect {
            UpgradeEffect::BombOnRevealProton => BombKind::Proton,
            UpgradeEffect::BombOnRevealSeismic => BombKind::Seismic,
            UpgradeEffect::BombOnRevealIon => BombKind::Ion,
            UpgradeEffect::BombOnRevealThermal => BombKind::Thermal,
            UpgradeEffect::BombActionProximityMines => BombKind::ProximityMine,
            UpgradeEffect::BombActionClusterMines => BombKind::ClusterMine,
            UpgradeEffect::BombActionConnerNet => BombKind::ConnerNet,
            _ => return None,
        })
    }

    /// Mines are dropped as an action and wait for a ship to cross them;
    /// everything else drops on dial reveal and blows at end of Activation.
    pub fn is_mine(self) -> bool {
        matches!(self, BombKind::ProximityMine | BombKind::ClusterMine | BombKind::ConnerNet)
    }

    pub fn name(self) -> &'static str {
        match self {
            BombKind::Proton => "Proton Bomb",
            BombKind::Seismic => "Seismic Charge",
            BombKind::Ion => "Ion Bomb",
            BombKind::Thermal => "Thermal Detonator",
            BombKind::ProximityMine => "Proximity Mine",
            BombKind::ClusterMine => "Cluster Mine",
            BombKind::ConnerNet => "Conner Net",
            BombKind::SeismicTorpedo => "Seismic Torpedo",
        }
    }
}

/// A bomb or mine token on the board. `pose` is the token's front-center
/// like a ship pose (facing the way the dropping ship faced).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BombToken {
    pub id: u32,
    pub kind: BombKind,
    /// The card it came from (for names in the client).
    pub card: UpgradeId,
    pub pose: Pose,
    pub owner: PlayerId,
}

impl BombToken {
    pub fn corners(&self) -> [Vec2; 4] {
        rules::footprint_corners(self.pose, TOKEN_FOOTPRINT)
    }

    pub fn center(&self) -> Vec2 {
        self.pose.local_to_world(Vec2::new(-TOKEN_FOOTPRINT.length / 2.0, 0.0))
    }
}

/// What one ship suffered when a token detonated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BombHit {
    pub ship: ShipId,
    /// Damage points suffered (shields first), critical ones included.
    pub damage: u8,
    /// Faceup Damage cards dealt / criticals that reached the hull.
    pub crits: u8,
    pub ion: u8,
    pub stress: u8,
    pub destroyed: bool,
}

/// One token going off and everything it did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Detonation {
    pub token: BombToken,
    pub hits: Vec<BombHit>,
}

/// Where a token dropped by a ship at `ship` (base `fp`) lands: one
/// straight-1 template behind the rear edge, facing the same way.
pub fn drop_pose(ship: Pose, fp: Footprint) -> Pose {
    let back = fp.length + templates::straight_length(1).unwrap_or(1.0);
    Pose { anchor: ship.local_to_world(Vec2::new(-back, 0.0)), heading: ship.heading }
}

/// Token poses for a drop: one token, or the three-wide Cluster Mine set.
pub fn drop_poses(kind: BombKind, ship: Pose, fp: Footprint) -> Vec<Pose> {
    let center = drop_pose(ship, fp);
    if kind == BombKind::ClusterMine {
        let w = TOKEN_FOOTPRINT.width;
        [w, 0.0, -w]
            .iter()
            .map(|&dy| Pose {
                anchor: center.local_to_world(Vec2::new(0.0, dy)),
                heading: center.heading,
            })
            .collect()
    } else {
        vec![center]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    #[test]
    fn token_lands_one_template_behind_the_base() {
        let fp = Footprint { length: 1.0, width: 1.0 };
        let p = drop_pose(Pose::new(10.0, 5.0, FRAC_PI_2), fp);
        // Facing +Y from a front-center at y=5: rear edge y=4, template
        // to y=3, token front-center there.
        assert!((p.anchor.x - 10.0).abs() < 1e-9);
        assert!((p.anchor.y - 3.0).abs() < 1e-9, "{}", p.anchor.y);
        assert_eq!(drop_poses(BombKind::ClusterMine, Pose::new(10.0, 5.0, FRAC_PI_2), fp).len(), 3);
        assert_eq!(drop_poses(BombKind::Proton, Pose::new(10.0, 5.0, FRAC_PI_2), fp).len(), 1);
    }
}
