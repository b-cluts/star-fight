use serde::{Deserialize, Serialize};

use crate::ship::ShipId;

/// The turn phase machine:
/// Setup -> Placement -> [Planning -> Resolution]* -> GameOver
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Setup,
    Placement,
    Planning,
    Resolution,
    GameOver,
}

/// Movement phase order: LOWEST pilot skill moves first.
/// Ties break deterministically by ship id.
pub fn movement_order(ships: &[(ShipId, u8)]) -> Vec<ShipId> {
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill)| (skill, id.0));
    v.into_iter().map(|(id, _)| id).collect()
}

/// Combat phase order: HIGHEST pilot skill fires first.
/// Ties break deterministically by ship id.
pub fn combat_order(ships: &[(ShipId, u8)]) -> Vec<ShipId> {
    let mut v: Vec<_> = ships.to_vec();
    v.sort_by_key(|&(id, skill)| (std::cmp::Reverse(skill), id.0));
    v.into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ties_move_before_xwings_but_fire_after() {
        // Two TIEs (skill 1), two X-Wings (skill 2), interleaved input.
        let ships = [
            (ShipId(3), 2), // X-Wing
            (ShipId(0), 1), // TIE
            (ShipId(2), 2), // X-Wing
            (ShipId(1), 1), // TIE
        ];
        assert_eq!(
            movement_order(&ships),
            vec![ShipId(0), ShipId(1), ShipId(2), ShipId(3)]
        );
        assert_eq!(
            combat_order(&ships),
            vec![ShipId(2), ShipId(3), ShipId(0), ShipId(1)]
        );
    }
}
