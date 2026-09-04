//! Pilots: the card that gives a ship its skill, squad cost, upgrade slots
//! and (for named pilots) a special ability. Loaded from
//! `assets/data/pilots.ron`; a fleet is a list of pilots, each implying
//! its ship class.

use serde::{Deserialize, Serialize};

use crate::ship::ShipClassId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PilotId(pub u16);

/// Which product a pilot card came from, so a scenario can restrict the
/// roster (e.g. "core set only").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Source {
    /// The Force Awakens core set (T-70 X-Wing, TIE/fo).
    CoreSet,
    /// The original core set (TIE/ln).
    OriginalCoreSet,
    TieFighterExpansion,
    T70Expansion,
    HeroesOfTheResistance,
    ImperialAssaultCarrier,
    TieFoExpansion,
}

/// Pilot abilities, as data tags. Each variant documents the card text;
/// rules enforcement lands per variant in `game.rs` (a variant listed
/// here is NOT necessarily implemented yet — see `implemented()`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PilotAbility {
    // ---- T-70 X-Wing ----
    /// Poe Dameron: while attacking or defending, if you have a focus
    /// token, you may change 1 focus result to a hit or evade result.
    FocusToResult,
    /// Ello Asty: while you are not stressed, treat your Tallon Rolls as
    /// white maneuvers.
    TallonWhiteWhileUnstressed,
    /// Nien Nunb: when you receive a stress token, if there is an enemy
    /// ship inside your firing arc at Range 1, you may discard it.
    DiscardStressIfEnemyInArcRange1,
    /// "Snap" Wexley: after you execute a 2-, 3-, or 4-speed maneuver, if
    /// you are not touching a ship, you may perform a free boost action.
    FreeBoostAfterSpeed2To4,
    /// "Red Ace": the first time you remove a shield token each round,
    /// assign 1 evade token to your ship.
    EvadeOnFirstShieldLoss,
    /// "Blue Ace": when performing a boost action, you may use the left
    /// or right turn 1 template.
    BoostWithTurnTemplate,
    /// Jess Pava: when attacking or defending, you may reroll 1 die for
    /// each other friendly ship at Range 1.
    RerollPerFriendlyRange1,
    // ---- TIE/ln ----
    /// "Howlrunner": when another friendly ship at Range 1 attacks with
    /// its primary weapon, it may reroll 1 attack die.
    FriendlyRerollAttackRange1,
    /// "Mauler Mithel": when attacking at Range 1, roll 1 additional
    /// attack die.
    ExtraAttackDieAtRange1,
    /// "Backstabber": when attacking from outside the defender's firing
    /// arc, roll 1 additional attack die.
    ExtraAttackDieOutsideDefenderArc,
    /// "Dark Curse": when defending, ships attacking you cannot spend
    /// focus tokens or reroll attack dice.
    DefenderDeniesFocusAndRerolls,
    /// Scourge: when attacking a defender that has 1 or more Damage
    /// cards, roll 1 additional attack die.
    ExtraAttackDieVsDamaged,
    /// "Night Beast": after executing a green maneuver, you may perform a
    /// free focus action.
    FreeFocusAfterGreen,
    /// "Youngster": friendly TIE fighters at Range 1-3 may perform the
    /// action on your equipped Elite Pilot Talent card as their action.
    ShareTalentAction,
    /// "Wampa": when attacking, you may cancel all dice results; if you
    /// cancel a critical result, deal 1 facedown Damage card to the
    /// defender.
    CancelAllForFacedownDamage,
    /// "Chaser": when another friendly ship at Range 1 spends a focus
    /// token, assign a focus token to your ship.
    FocusWhenFriendlySpendsFocusRange1,
    /// "Winged Gundark": when attacking at Range 1, you may change 1 of
    /// your hit results to a critical hit result.
    HitToCritAtRange1,
    // ---- TIE/fo ----
    /// "Omega Ace": when attacking, you may spend a target lock and a
    /// focus token to change all of your dice results to critical hits.
    SpendLockAndFocusForAllCrits,
    /// "Epsilon Leader": at the start of the Combat phase, remove 1 stress
    /// token from each friendly ship at Range 1.
    RemoveStressFriendlyRange1AtCombatStart,
    /// "Zeta Ace": when performing a barrel roll, you may use the
    /// straight 2 template instead of the straight 1 template.
    BarrelRollWithStraight2,
    /// "Omega Leader": enemy ships you have locked cannot modify any dice
    /// when attacking you or defending against your attacks.
    LockedEnemiesCannotModifyDice,
    /// "Zeta Leader": when attacking, if you are not stressed, you may
    /// receive 1 stress token to roll 1 additional attack die.
    StressForExtraAttackDie,
    /// "Epsilon Ace": while you have no Damage cards, treat your pilot
    /// skill as 12.
    SkillTwelveWhileUndamaged,
}

impl PilotAbility {
    /// Whether the rules engine currently applies this ability. Abilities
    /// are data first; enforcement is added one at a time with tests.
    pub fn implemented(self) -> bool {
        matches!(
            self,
            PilotAbility::FocusToResult
                | PilotAbility::ExtraAttackDieAtRange1
                | PilotAbility::ExtraAttackDieOutsideDefenderArc
                | PilotAbility::ExtraAttackDieVsDamaged
                | PilotAbility::StressForExtraAttackDie
                | PilotAbility::HitToCritAtRange1
                | PilotAbility::SpendLockAndFocusForAllCrits
                | PilotAbility::DefenderDeniesFocusAndRerolls
                | PilotAbility::LockedEnemiesCannotModifyDice
                | PilotAbility::FriendlyRerollAttackRange1
                | PilotAbility::RerollPerFriendlyRange1
        )
    }
}

/// One pilot card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pilot {
    pub id: PilotId,
    pub class: ShipClassId,
    pub name: String,
    /// XWS identifier (application-neutral card id, e.g. "howlrunner",
    /// "poedameron-swx57"); also the card image file stem.
    pub xws: String,
    /// Named pilots: at most one copy per squad.
    pub unique: bool,
    /// Pilot skill: lower MOVES first, higher FIRES first.
    pub skill: u8,
    /// Squad-point cost of ship + pilot (upgrades add their own).
    pub cost: u16,
    /// Has an Elite Pilot Talent slot beyond the chassis upgrade bar.
    #[serde(default)]
    pub talent_slot: bool,
    pub source: Source,
    #[serde(default)]
    pub ability: Option<PilotAbility>,
}
