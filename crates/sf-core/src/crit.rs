//! Critical damage effects (original core set, 14 types) — implemented as
//! modifier tags per the no-card-UI design: a crit that reaches the hull
//! draws one effect; immediate ones resolve at once, persistent ones stay
//! attached to the ship and are public information.

use serde::{Deserialize, Serialize};

use crate::maneuver::{Difficulty, Maneuver, Steer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CritEffect {
    /// Next attack rolls 0 attack dice, then this clears.
    BlindedPilot,
    /// Each combat phase: roll one attack die, take 1 damage on a Hit.
    ConsoleFire,
    /// Pilot skill becomes 0 from the next round on.
    DamagedCockpit,
    /// Turn (90°) maneuvers are treated as red.
    DamagedEngine,
    /// Cannot perform actions from the action bar (Pass still allowed).
    DamagedSensorArray,
    /// Immediate: counts as 2 damage against the hull.
    DirectHit,
    /// Ignore pilot ability / elite talents (no-op until those exist).
    InjuredPilot,
    /// Immediate: roll one attack die, take 1 more damage on a Hit.
    MinorExplosion,
    /// Immediate: one existing faceup effect flips facedown (removed).
    MinorHullBreach,
    /// Suffer 1 damage whenever this ship bumps another ship.
    StunnedPilot,
    /// Bank (45°) maneuvers are treated as red.
    ThrustControlFire,
    /// Roll 1 fewer attack die with the primary weapon.
    WeaponMalfunction,
    /// Cannot attack this round or the next.
    WeaponsFailure { rounds: u8 },
    /// Agility reduced by 1.
    StructuralDamage,
}

/// Draw one effect from the table with a raw random byte.
pub fn draw(raw: u8) -> CritEffect {
    match raw % 14 {
        0 => CritEffect::BlindedPilot,
        1 => CritEffect::ConsoleFire,
        2 => CritEffect::DamagedCockpit,
        3 => CritEffect::DamagedEngine,
        4 => CritEffect::DamagedSensorArray,
        5 => CritEffect::DirectHit,
        6 => CritEffect::InjuredPilot,
        7 => CritEffect::MinorExplosion,
        8 => CritEffect::MinorHullBreach,
        9 => CritEffect::StunnedPilot,
        10 => CritEffect::ThrustControlFire,
        11 => CritEffect::WeaponMalfunction,
        12 => CritEffect::WeaponsFailure { rounds: 2 },
        _ => CritEffect::StructuralDamage,
    }
}

impl CritEffect {
    pub fn name(&self) -> &'static str {
        match self {
            CritEffect::BlindedPilot => "Blinded Pilot",
            CritEffect::ConsoleFire => "Console Fire",
            CritEffect::DamagedCockpit => "Damaged Cockpit",
            CritEffect::DamagedEngine => "Damaged Engine",
            CritEffect::DamagedSensorArray => "Damaged Sensor Array",
            CritEffect::DirectHit => "Direct Hit!",
            CritEffect::InjuredPilot => "Injured Pilot",
            CritEffect::MinorExplosion => "Minor Explosion",
            CritEffect::MinorHullBreach => "Minor Hull Breach",
            CritEffect::StunnedPilot => "Stunned Pilot",
            CritEffect::ThrustControlFire => "Thrust Control Fire",
            CritEffect::WeaponMalfunction => "Weapon Malfunction",
            CritEffect::WeaponsFailure { .. } => "Weapons Failure",
            CritEffect::StructuralDamage => "Structural Damage",
        }
    }
}

/// Difficulty after crit modifiers: Damaged Engine makes 90° turns red,
/// Thrust Control Fire makes banks red.
pub fn effective_difficulty(crits: &[CritEffect], m: &Maneuver) -> Difficulty {
    if m.difficulty == Difficulty::Hard {
        return Difficulty::Hard;
    }
    let engine = crits.contains(&CritEffect::DamagedEngine);
    let thrust = crits.contains(&CritEffect::ThrustControlFire);
    match m.steer {
        Steer::TurnLeft | Steer::TurnRight if engine => Difficulty::Hard,
        Steer::BankLeft | Steer::BankRight if thrust => Difficulty::Hard,
        _ => m.difficulty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draw_covers_all_fourteen() {
        assert_eq!(draw(5), CritEffect::DirectHit);
        assert_eq!(draw(12), CritEffect::WeaponsFailure { rounds: 2 });
        assert_eq!(draw(13), CritEffect::StructuralDamage);
        assert_eq!(draw(14), CritEffect::BlindedPilot); // wraps
    }

    #[test]
    fn engine_and_thrust_make_maneuvers_red() {
        let turn = Maneuver { steer: Steer::TurnLeft, distance: 2, difficulty: Difficulty::Normal };
        let bank = Maneuver { steer: Steer::BankRight, distance: 1, difficulty: Difficulty::Easy };
        assert_eq!(effective_difficulty(&[], &turn), Difficulty::Normal);
        assert_eq!(effective_difficulty(&[CritEffect::DamagedEngine], &turn), Difficulty::Hard);
        assert_eq!(
            effective_difficulty(&[CritEffect::DamagedEngine], &bank),
            Difficulty::Easy,
            "engine damage leaves banks alone"
        );
        assert_eq!(effective_difficulty(&[CritEffect::ThrustControlFire], &bank), Difficulty::Hard);
    }
}
