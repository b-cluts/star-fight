//! Upgrade cards (First Edition): talents, astromechs, torpedoes, tech,
//! modifications, titles. Loaded from `assets/data/upgrades.ron`. Like
//! pilot abilities they are data first: every card carries its verified
//! text and cost, an `effect` tag, and restrictions the squad builder
//! enforces; rules enforcement is added per effect with tests.

use serde::{Deserialize, Serialize};

use crate::ship::Faction;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    /// The required token is spent to fire ("Spend your target lock…");
    /// false for cards that only need the token present (Homing
    /// Missiles, Proton Rockets…).
    #[serde(default = "default_true")]
    pub spend: bool,
}

fn default_true() -> bool {
    true
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
    /// Bomb Loadout: a Bomb slot.
    BarGainsBomb,
    /// Smuggling Compartment: an Illicit slot plus a second Modification
    /// slot whose card must cost 3 or fewer points.
    BarGainsIllicitAndCheapModification,
    // large-ship modifications
    /// Anti-Pursuit Lasers: an enemy whose maneuver overlaps this ship
    /// rolls one attack die and suffers 1 damage on a hit or crit.
    BumpedEnemyDamage,
    /// Ion Projector: as above, but an ion token instead of damage.
    BumpedEnemyIon,
    /// Countermeasures: switched on for a round (key U), discarded at the
    /// start of the Combat phase for +1 agility until the End phase and
    /// the removal of one enemy target lock.
    CountermeasuresDiscard,
    /// Tactical Jammer: this ship obstructs enemy attacks on friends.
    ObstructsEnemyAttacks,
    // Scum and Villainy (Most Wanted)
    /// Inertial Dampeners: switched on for a round (key U), the revealed
    /// maneuver becomes a white stationary one, the card is discarded and
    /// the ship takes a stress token.
    StationaryOnRevealDiscard,
    /// Dead Man's Switch: when destroyed, every ship at Range 1 suffers 1
    /// damage.
    DamageNeighboursWhenDestroyed,
    /// Feedback Array: switched on for a round (key U), the ship skips its
    /// attacks, takes an ion token and 1 damage, and deals 1 damage to an
    /// enemy at Range 1.
    FeedbackArray,
    /// Unhinged Astromech: 3-speed maneuvers are green.
    Speed3Green,
    /// Salvaged Astromech: a Ship-trait damage card is discarded along
    /// with this card.
    DiscardSelfToCancelShipDamage,
    /// R4 Agromech: spending a focus token while attacking acquires a lock
    /// on the defender.
    LockAfterSpendingFocus,
    /// R4-B11: spend a lock on the defender to make it reroll its evades.
    SpendLockToRerollDefenseDice,
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
    BarGainsEvade,
    TitleRotate180AfterBank3ForStress,
    BarGainsSystemCheaper,
    TwoDifferentModifications,
    // bombs (dropped when the dial is revealed, or as an action)
    BombOnRevealProton,
    BombOnRevealSeismic,
    BombActionProximityMines,
    BombOnRevealIon,
    BombActionClusterMines,
    BombActionConnerNet,
    BombOnRevealThermal,
    // cannons
    CannonCritsToHits,
    CannonIonOneDamage,
    CannonUncancelableHits,
    CannonOneDamageAndStress,
    CannonHitToCrit,
    CannonTractorToken,
    // systems
    SystemLockAfterAttack,
    SystemFreeActionBeforeReveal,
    SystemAttackerHitToFocus,
    SystemCancelAllAddTwoHits,
    SystemSkillZeroInActivation,
    SystemDamageToDiscardToken,
    SystemOverlapObstaclesOnReposition,
    SystemRecoverShieldAfter3Damage,
    SystemAddCritWithLock,
    // Imperial crew
    CrewSufferTwoForCrit,
    CrewChosenEnemyFocusToHitOrEvade,
    CrewDiscardToFlipCritFacedown,
    CrewFleetOfficerAction,
    CrewFriendlyLockAfterGreen,
    CrewStressFirstAttacker,
    CrewFreeEvadeIfNoShieldsDamaged,
    CrewFocusAfterFriendlyMiss,
    CrewStressEnemiesAtRange1EndOfCombat,
    TitleLockAnywhere,
    // crew
    CrewDiscardDamageRecoverShield,
    CrewSecondAttackFocusToHit,
    CrewLockAllFocusToHit,
    CrewRedAsWhiteForAll,
    CrewGuessEvades,
    CrewRecoverShieldEndPhase,
    CrewStraightsAreGreen,
    CrewSecondAttackOnMiss,
    CrewHitToCritAtRange3,
    CrewRotateDialSameBearing,
    CrewTwoLocks,
    CrewExtraFocusOnFocusAction,
    CrewStressTargetAtRange2InArc,
    CrewPeekEnemyDial,
    CrewFocusAfterStressRemoved,
    CrewEvadeInsteadOfFocusForFriendly,
    CrewRollDefenseForTokensAction,
    CrewStoreFocusTokens,
    CrewAddBlankIfEnemyInArc,
    CrewRerollDefenseDie,
    CrewSaboteurAction,
    // setup
    SetupSkillOverride,
    CancelFocusForEvade,
    SkillOfLockedAttackerDie,
    ScoreToSettle,
    ExtraActionThenStress,
}

impl UpgradeEffect {
    /// Whether the rules engine currently applies this effect.
    /// Upgrade slots an equipped card adds to the ship's bar (R2-D6's
    /// talent slot, the TIE/x1 System slot, the Royal Guard TIE's second
    /// Modification, Bomb Loadout, Smuggling Compartment).
    pub fn granted_slots(self) -> &'static [Slot] {
        use UpgradeEffect::*;
        match self {
            BarGainsTalent => &[Slot::Talent],
            BarGainsSystemCheaper => &[Slot::System],
            TwoDifferentModifications => &[Slot::Modification],
            BarGainsBomb => &[Slot::Bomb],
            BarGainsIllicitAndCheapModification => &[Slot::Illicit, Slot::Modification],
            _ => &[],
        }
    }

    pub fn implemented(self) -> bool {
        use UpgradeEffect::*;
        matches!(
            self,
            HullPlus1
                | BarGainsBomb
                | BarGainsIllicitAndCheapModification
                | BumpedEnemyDamage
                | BumpedEnemyIon
                | CountermeasuresDiscard
                | ObstructsEnemyAttacks
                | StationaryOnRevealDiscard
                | DamageNeighboursWhenDestroyed
                | FeedbackArray
                | Speed3Green
                | DiscardSelfToCancelShipDamage
                | LockAfterSpendingFocus
                | SpendLockToRerollDefenseDice
                | ShieldPlus1
                | AgilityPlus1DiscardWhenHit
                | SkillPlus2
                | SkillPlus1
                | SkillMinus1
                | BarGainsTargetLock
                | BarGainsBoost
                | BarGainsBarrelRoll
                | BarGainsTalent
                | BarGainsEvade
                | TorpedoFocusToCrit
                | TorpedoBlanksToFocus
                | MissileBlankToHit
                | CannonHitToCrit
                | CannonCritsToHits
                | TurretDorsalExtraDieAtRange1
                | RocketExtraDiceByAgility
                | MissileDenyEvadeTokens
                | TurretAutoblasterUncancelable
                | CannonUncancelableHits
                | TurretIonOneDamage
                | CannonIonOneDamage
                | MissileIonOneDamage
                | CannonOneDamageAndStress
                | MissileFaceupDamage
                | TurretTwinLaserTwiceOneDamage
                | MissileAttackTwice
                | TorpedoStressIfHullLow
                | TorpedoStripShield
                | SeismicTorpedoAction
                | BarrelRollActionDiscardLock
                | TwoDifferentModifications
                | BarGainsSystemCheaper
                | BlankToEvadeAtRange3OrOutsideArc
                | ShareLockWithFriendly
                | StressDefenderIfInArc
                | ScoreToSettle
                | CannonTractorToken
                | TitleArcOnlyThenTurretAttack
                | CrewStoreFocusTokens
                | CrewRedAsWhiteForAll
                | CrewRotateDialSameBearing
                | RotateDialSameSpeedRed
                | CrewFleetOfficerAction
                | FreeActionForLowerSkillShip
                | LockAndBoostAction
                | CrewRollDefenseForTokensAction
                | CrewSaboteurAction
                | DiscardFacedownOnDefenseDie
                | SetupSkillOverride
                | ExtraActionThenStress
                | SystemDamageToDiscardToken
                | CrewPeekEnemyDial
                | CrewEvadeInsteadOfFocusForFriendly
                | SwapSkillWithFriendly
                | SnapShotReaction
                | RedTurn1Action
                | RotateShip180Discard
                | CrewTwoLocks
                | OrdnanceTokens
                | ResolveStressAfterAction
                | StressAllowsRepositionUnder3
                | IgnoreObstaclesDiscard
                | LockAfterRed
                | LocksOnlyAtRange3
                | RemoveEnemyLockAfterReposition
                | TitleLockAnywhere
                | SystemFreeActionBeforeReveal
                | SystemSkillZeroInActivation
                | SystemOverlapObstaclesOnReposition
                | CrewSecondAttackFocusToHit
                | CrewSecondAttackOnMiss
                | CrewFriendlyLockAfterGreen
                | ShareSkillWithFriendly
                | LockBecomesFocus
                | BlankToHitSpendFocus
                | KeepOneEvade
                | CancelFocusForEvade
                | CancelHitsForStress
                | ReLockOnEvadeDie
                | ForceRerollLockedAttacker
                | OrdnanceDieToHit
                | ExtraDefenseDieIfOutgunned
                | KeepOrdnanceOnMiss
                | SystemLockAfterAttack
                | SystemAttackerHitToFocus
                | SystemCancelAllAddTwoHits
                | SystemRecoverShieldAfter3Damage
                | SystemAddCritWithLock
                | CrewLockAllFocusToHit
                | CrewGuessEvades
                | CrewHitToCritAtRange3
                | CrewExtraFocusOnFocusAction
                | CrewStressTargetAtRange2InArc
                | CrewAddBlankIfEnemyInArc
                | CrewRerollDefenseDie
                | CrewSufferTwoForCrit
                | CrewChosenEnemyFocusToHitOrEvade
                | CrewStressFirstAttacker
                | CrewFreeEvadeIfNoShieldsDamaged
                | CrewFocusAfterFriendlyMiss
                | CrewStressEnemiesAtRange1EndOfCombat
                | ReduceAgilityWhileTouching
                | SplashDamageAfterHit
                | ExtraDiceFromFriendlyEvades
                | BombOnRevealProton
                | BombOnRevealSeismic
                | BombOnRevealIon
                | BombOnRevealThermal
                | BombActionProximityMines
                | BombActionClusterMines
                | BombActionConnerNet
                | TorpedoIonSplash
                | MissileSplashRange1
                | MissileFriendsLockOnHit
                | TurretBlasterSpendFocus
                | RerollFocusWhenStressed
                | RerollOneAttackDie
                | RerollBlankIfAlone
                | CancelEvadeDiscard
                | EvadeToFocusIfEvadeToken
                | AllFocusToHitIfUnstressed
                | FocusToCritSpendFocus
                | ExtraAttackDieForStress
                | ReduceAgilityIfNotInDefenderArc
                | BlankToEvadeSpendFocus
                | Speed1And2AreGreen
                | BanksAreGreen
                | CrewStraightsAreGreen
                | TreatRedAsWhiteDiscard
                | RemoveStressFriendlyAtCombatStart
                | FocusOrEvadeOnStress
                | DiscardPilotCritImmediately
                | CrewFocusAfterStressRemoved
                | ForceRerollForStress
                | ForceRerollWithLock
                | SufferCritForFriendly
                | CrewRecoverShieldEndPhase
                | CrewDiscardDamageRecoverShield
                | CrewDiscardToFlipCritFacedown
                | DiscardAstromechToCancelDamage
                | FlipShipCritFacedown
                | RecoverShieldOnGreen
                | RecoverShieldSpendFocus
                | FreeActionThenStress
                | FreeBarrelRollOnGreen
                | FocusToCritOthersToHitAction
                | RerollUpTo3ForFocusAnd2Stress
                | ExposeAction
                | AgilityPlus1Action
                | ExtraAttackDieIfObstructed
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
    /// Squad points; Chardaan Refit is the one negative card.
    pub cost: i8,
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
