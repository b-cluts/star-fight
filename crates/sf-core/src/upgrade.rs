//! Upgrade cards (First Edition): talents, astromechs, torpedoes, tech,
//! modifications, titles. Loaded from `assets/data/upgrades.ron`. Like
//! pilot abilities they are data first: every card carries its verified
//! text and cost, an `effect` tag, and restrictions the squad builder
//! enforces; rules enforcement is added per effect with tests.

use serde::{Deserialize, Serialize};

use crate::ship::Faction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UpgradeId(pub u16);

/// Upgrade slot icons. `Modification` and `Title` are implicit: every
/// ship has one of each in addition to its printed bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Slot {
    Talent,
    Astromech,
    Torpedo,
    Missile,
    Tech,
    Modification,
    Title,
    Cannon,
    Turret,
    Crew,
    Bomb,
    System,
    Illicit,
    SalvagedAstromech,
}

impl Slot {
    /// XWS directory name for card images.
    pub fn xws(self) -> &'static str {
        match self {
            Slot::Talent => "ept",
            Slot::Astromech => "amd",
            Slot::Torpedo => "torpedo",
            Slot::Missile => "missile",
            Slot::Tech => "tech",
            Slot::Modification => "mod",
            Slot::Title => "title",
            Slot::Cannon => "cannon",
            Slot::Turret => "turret",
            Slot::Crew => "crew",
            Slot::Bomb => "bomb",
            Slot::System => "system",
            Slot::Illicit => "illicit",
            Slot::SalvagedAstromech => "samd",
        }
    }

    /// Slots every ship has without them being printed on its bar.
    pub fn implicit() -> [Slot; 2] {
        [Slot::Modification, Slot::Title]
    }
}

/// Equip restrictions printed on the card, checked by the squad builder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Restriction {
    SmallShipOnly,
    LargeShipOnly,
    /// Substring of the ship's XWS id: "t70xwing" is exact, "xwing" also
    /// matches the T-70, "tie" matches every TIE.
    ShipOnly(String),
    FactionOnly(Faction),
    /// Pilot skill must be strictly above this value.
    SkillAbove(u8),
    /// Pilot skill must be at most this value.
    SkillAtMost(u8),
    /// The ship's action bar must include this action icon.
    RequiresAction(crate::action::ActionKind),
    /// The ship's action bar must NOT include this action icon.
    LacksAction(crate::action::ActionKind),
    /// The upgrade bar must include all these slots.
    RequiresSlots(Vec<Slot>),
    /// The upgrade bar must NOT include this slot.
    LacksSlot(Slot),
    /// Agility must be below this value.
    AgilityBelow(u8),
}

/// What the attack header of a secondary weapon demands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttackRequirement {
    Free,
    TargetLock,
    Focus,
}

/// "Attack:" header of a secondary weapon card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryWeapon {
    pub dice: u8,
    pub range_min: u8,
    pub range_max: u8,
    pub requires: AttackRequirement,
    /// Discarded (or an ordnance token spent) to perform the attack.
    pub discard_to_fire: bool,
}

/// Effect tags — one per distinct card text (see the `text` field in the
/// data file for the verbatim wording). None are enforced yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum UpgradeEffect {
    // stats / bar
    HullPlus1,
    ShieldPlus1,
    AgilityPlus1DiscardWhenHit,
    SkillPlus2,
    SkillPlus1,
    SkillMinus1,
    BarGainsTargetLock,
    BarGainsBoost,
    BarGainsBarrelRoll,
    BarGainsTalent,
    // dice
    FocusToHitSpendFocus,
    BlankToHitSpendFocus,
    BlankToEvadeSpendFocus,
    FocusToCritSpendFocus,
    RerollFocusWhenStressed,
    RerollOneAttackDie,
    RerollBlankIfAlone,
    CancelEvadeDiscard,
    EvadeToFocusIfEvadeToken,
    FocusToCritOthersToHitAction,
    AllFocusToHitIfUnstressed,
    BlankToEvadeAtRange3OrOutsideArc,
    ExtraDefenseDieIfOutgunned,
    OrdnanceDieToHit,
    ExtraAttackDieForStress,
    ExtraAttackDieIfObstructed,
    ReduceAgilityIfNotInDefenderArc,
    ReduceAgilityWhileTouching,
    ForceRerollForStress,
    ForceRerollWithLock,
    ForceRerollLockedAttacker,
    RerollUpTo3ForFocusAnd2Stress,
    ExtraDiceFromFriendlyEvades,
    // tokens / stress
    KeepOneEvade,
    ResolveStressAfterAction,
    StressAllowsRepositionUnder3,
    FocusOrEvadeOnStress,
    TreatRedAsWhiteDiscard,
    StressDefenderIfInArc,
    CancelHitsForStress,
    RemoveStressFriendlyAtCombatStart,
    SwapSkillWithFriendly,
    ShareSkillWithFriendly,
    FreeActionThenStress,
    FreeActionForLowerSkillShip,
    RotateDialSameSpeedRed,
    RotateShip180Discard,
    ExposeAction,
    // movement / actions
    FreeBarrelRollOnGreen,
    Speed1And2AreGreen,
    BanksAreGreen,
    RecoverShieldOnGreen,
    RecoverShieldSpendFocus,
    AgilityPlus1Action,
    LockAfterRed,
    LockAndBoostAction,
    ReLockOnEvadeDie,
    BarrelRollActionDiscardLock,
    RedTurn1Action,
    RemoveEnemyLockAfterReposition,
    // damage cards
    DiscardPilotCritImmediately,
    DiscardAstromechToCancelDamage,
    FlipShipCritFacedown,
    DiscardFacedownOnDefenseDie,
    SufferCritForFriendly,
    IgnoreObstaclesDiscard,
    SplashDamageAfterHit,
    // secondary weapons
    TorpedoFocusToCrit,
    TorpedoBlanksToFocus,
    TorpedoStressIfHullLow,
    TorpedoIonSplash,
    TorpedoStripShield,
    OrdnanceTokens,
    KeepOrdnanceOnMiss,
    LockBecomesFocus,
    ShareLockWithFriendly,
    LocksOnlyAtRange3,
    SnapShotReaction,
    SeismicTorpedoAction,
    // turrets (all fire at ships outside the firing arc)
    TurretIonOneDamage,
    TurretBlasterSpendFocus,
    TurretAutoblasterUncancelable,
    TurretDorsalExtraDieAtRange1,
    TurretTwinLaserTwiceOneDamage,
    // missiles
    MissileBlankToHit,
    MissileAttackTwice,
    MissileDenyEvadeTokens,
    MissileSplashRange1,
    MissileIonOneDamage,
    RocketExtraDiceByAgility,
    MissileFaceupDamage,
    MissileFriendsLockOnHit,
    // titles
    TitleArcOnlyThenTurretAttack,
    // setup
    SetupSkillOverride,
    CancelFocusForEvade,
    SkillOfLockedAttackerDie,
    ScoreToSettle,
    ExtraActionThenStress,
}

impl UpgradeEffect {
    /// Whether the rules engine currently applies this effect.
    pub fn implemented(self) -> bool {
        use UpgradeEffect::*;
        matches!(
            self,
            HullPlus1
                | ShieldPlus1
                | AgilityPlus1DiscardWhenHit
                | SkillPlus2
                | SkillPlus1
                | SkillMinus1
                | BarGainsTargetLock
                | BarGainsBoost
                | BarGainsBarrelRoll
                | BarGainsTalent
        )
    }
}

/// One upgrade card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Upgrade {
    pub id: UpgradeId,
    pub xws: String,
    pub name: String,
    pub slot: Slot,
    pub cost: u8,
    /// Named cards: at most one copy per squad.
    #[serde(default)]
    pub unique: bool,
    /// "Limited.": at most one copy per ship.
    #[serde(default)]
    pub limited: bool,
    #[serde(default)]
    pub restrictions: Vec<Restriction>,
    #[serde(default)]
    pub attack: Option<SecondaryWeapon>,
    #[serde(default)]
    pub effect: Option<UpgradeEffect>,
    /// Verbatim card text, for the builder's text fallback.
    pub text: String,
}
