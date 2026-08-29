//! Custom d8 combat dice.
//!
//! Red ATTACK die (50% natural damage):  Hit x3, Crit x1, Focus x2, Blank x2.
//! Green DEFENSE die (37.5% natural dodge): Evade x3, Focus x2, Blank x3.
//!
//! Focus tokens convert every Focus face to a Hit (attacking) or Evade
//! (defending). A Target Lock lets the attacker reroll blanks.
//!
//! This module is deterministic: callers supply the randomness as raw d8
//! values (the server draws them from its seeded RNG), so every resolution
//! is replayable and unit-testable.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttackFace {
    Hit,
    Crit,
    Focus,
    Blank,
}

impl AttackFace {
    /// Face shown for a d8 value (0..=7 — larger values wrap).
    pub fn from_d8(v: u8) -> Self {
        match v % 8 {
            0 | 1 | 2 => AttackFace::Hit,
            3 => AttackFace::Crit,
            4 | 5 => AttackFace::Focus,
            _ => AttackFace::Blank,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DefenseFace {
    Evade,
    Focus,
    Blank,
}

impl DefenseFace {
    pub fn from_d8(v: u8) -> Self {
        match v % 8 {
            0 | 1 | 2 => DefenseFace::Evade,
            3 | 4 => DefenseFace::Focus,
            _ => DefenseFace::Blank,
        }
    }
}

/// Tally of one attack roll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttackRoll {
    pub hits: u8,
    pub crits: u8,
    pub focuses: u8,
    pub blanks: u8,
}

impl AttackRoll {
    pub fn from_faces(faces: impl IntoIterator<Item = AttackFace>) -> Self {
        let mut r = Self::default();
        for f in faces {
            match f {
                AttackFace::Hit => r.hits += 1,
                AttackFace::Crit => r.crits += 1,
                AttackFace::Focus => r.focuses += 1,
                AttackFace::Blank => r.blanks += 1,
            }
        }
        r
    }

    /// Spend a Focus token: every Focus face becomes a Hit.
    pub fn spend_focus(mut self) -> Self {
        self.hits += self.focuses;
        self.focuses = 0;
        self
    }
}

/// Tally of one defense roll.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DefenseRoll {
    pub evades: u8,
    pub focuses: u8,
    pub blanks: u8,
}

impl DefenseRoll {
    pub fn from_faces(faces: impl IntoIterator<Item = DefenseFace>) -> Self {
        let mut r = Self::default();
        for f in faces {
            match f {
                DefenseFace::Evade => r.evades += 1,
                DefenseFace::Focus => r.focuses += 1,
                DefenseFace::Blank => r.blanks += 1,
            }
        }
        r
    }

    /// Spend a Focus token: every Focus face becomes an Evade.
    pub fn spend_focus(mut self) -> Self {
        self.evades += self.focuses;
        self.focuses = 0;
        self
    }
}

/// Damage that gets through after evades cancel. Evades cancel Hits first,
/// then Crits (so crits — the nastier result — survive longest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttackOutcome {
    pub hits: u8,
    pub crits: u8,
}

pub fn cancel(attack: AttackRoll, defense: DefenseRoll) -> AttackOutcome {
    let mut evades = defense.evades;
    let hits = attack.hits.saturating_sub(evades);
    evades = evades.saturating_sub(attack.hits);
    let crits = attack.crits.saturating_sub(evades);
    AttackOutcome { hits, crits }
}

/// Result of applying an outcome to a ship's shields and hull.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DamageResult {
    pub shields_remaining: u8,
    /// Face-down damage (plain hull damage).
    pub hull_damage: u8,
    /// Face-up critical damage cards — only crits that REACH THE HULL;
    /// crits absorbed by shields are just shield loss.
    pub hull_crits: u8,
}

/// Hits resolve before crits; shields absorb each point before the hull.
pub fn apply_damage(shields: u8, outcome: AttackOutcome) -> DamageResult {
    let mut s = shields;
    let mut hull_damage = 0;
    let mut hull_crits = 0;
    for _ in 0..outcome.hits {
        if s > 0 { s -= 1 } else { hull_damage += 1 }
    }
    for _ in 0..outcome.crits {
        if s > 0 { s -= 1 } else { hull_crits += 1 }
    }
    DamageResult { shields_remaining: s, hull_damage, hull_crits }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn face_layouts_match_spec() {
        let attack: Vec<_> = (0..8).map(AttackFace::from_d8).collect();
        let a = AttackRoll::from_faces(attack);
        assert_eq!(a, AttackRoll { hits: 3, crits: 1, focuses: 2, blanks: 2 });
        // 50% of attack faces damage naturally; 37.5% of defense faces dodge.
        assert_eq!(a.hits + a.crits, 4);

        let defense: Vec<_> = (0..8).map(DefenseFace::from_d8).collect();
        let d = DefenseRoll::from_faces(defense);
        assert_eq!(d, DefenseRoll { evades: 3, focuses: 2, blanks: 3 });
    }

    #[test]
    fn focus_token_converts_eyes() {
        let a = AttackRoll { hits: 1, crits: 0, focuses: 2, blanks: 1 }.spend_focus();
        assert_eq!(a.hits, 3);
        let d = DefenseRoll { evades: 0, focuses: 2, blanks: 1 }.spend_focus();
        assert_eq!(d.evades, 2);
    }

    #[test]
    fn evades_cancel_hits_before_crits() {
        // 2 hits + 1 crit vs 2 evades: both hits gone, crit survives.
        let out = cancel(
            AttackRoll { hits: 2, crits: 1, focuses: 0, blanks: 0 },
            DefenseRoll { evades: 2, focuses: 0, blanks: 0 },
        );
        assert_eq!(out, AttackOutcome { hits: 0, crits: 1 });
        // 3 evades kill everything.
        let out = cancel(
            AttackRoll { hits: 2, crits: 1, focuses: 0, blanks: 0 },
            DefenseRoll { evades: 3, focuses: 0, blanks: 0 },
        );
        assert_eq!(out, AttackOutcome { hits: 0, crits: 0 });
    }

    #[test]
    fn shields_absorb_crits_without_face_up_cards() {
        // T-70 with 3 shields: 1 hit + 1 crit → 2 shields gone, no crit card.
        let r = apply_damage(3, AttackOutcome { hits: 1, crits: 1 });
        assert_eq!(r, DamageResult { shields_remaining: 1, hull_damage: 0, hull_crits: 0 });
    }

    #[test]
    fn unshielded_tie_takes_crits_on_the_hull() {
        // TIE (0 shields): 1 hit + 1 crit → face-down + face-up damage.
        let r = apply_damage(0, AttackOutcome { hits: 1, crits: 1 });
        assert_eq!(r, DamageResult { shields_remaining: 0, hull_damage: 1, hull_crits: 1 });
    }

    #[test]
    fn hits_drain_shields_before_crits_resolve() {
        // 1 shield, 1 hit + 1 crit: hit eats the shield, crit reaches hull.
        let r = apply_damage(1, AttackOutcome { hits: 1, crits: 1 });
        assert_eq!(r, DamageResult { shields_remaining: 0, hull_damage: 0, hull_crits: 1 });
    }
}
