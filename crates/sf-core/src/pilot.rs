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
    /// BTL-A4 Y-Wing expansion (wave 1).
    YWingExpansion,
    /// RZ-1 A-Wing expansion (wave 2).
    AWingExpansion,
    /// Rebel Aces (A-Wing and B-Wing repaints, wave 4½).
    RebelAces,
    /// Millennium Falcon expansion (YT-1300, wave 2).
    YT1300Expansion,
    TieBomberExpansion,
    /// Imperial Veterans (TIE Bomber and TIE Defender repaints).
    ImperialVeterans,
    TieAdvancedExpansion,
    /// Imperial Raider (Epic; carries the second TIE Advanced pilot set).
    ImperialRaider,
    TieInterceptorExpansion,
    /// Imperial Aces (TIE Interceptor repaints).
    ImperialAces,
    LambdaShuttleExpansion,
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
    // ---- BTL-A4 Y-Wing ----
    /// Horton Salm: when attacking at Range 2-3, you may reroll any of
    /// your blank results.
    RerollBlanksAtRange2To3,
    /// "Dutch" Vander: after acquiring a target lock, choose another
    /// friendly ship at Range 1-2; that ship may immediately acquire a
    /// target lock.
    FriendlyLockAfterLock,
    // ---- RZ-1 A-Wing ----
    /// Tycho Celchu: you may perform actions even while you have stress
    /// tokens.
    ActionsWhileStressed,
    /// Jake Farrell: after you perform a focus action or are assigned a
    /// focus token, you may perform a free boost or barrel roll action.
    FreeRepositionAfterFocus,
    /// Arvel Crynyd: you may declare an enemy ship inside your firing
    /// arc that you are touching as the target of your attack.
    TargetTouchingShipInArc,
    /// Gemmer Sojan: while you are at Range 1 of at least 1 enemy ship,
    /// increase your agility value by 1.
    AgilityPlus1IfEnemyAtRange1,
    // ---- YT-1300 ----
    /// Han Solo: when attacking, you may reroll all of your dice; if you
    /// do, you must reroll as many of your dice as possible.
    RerollAllDice,
    /// Han Solo (Heroes of the Resistance): when placed during setup,
    /// you can be placed anywhere in the play area beyond Range 3 of
    /// enemy ships.
    SetupAnywhereBeyondRange3,
    /// Lando Calrissian: after you execute a green maneuver, choose 1
    /// other friendly ship at Range 1; it may perform 1 free action
    /// shown in its action bar.
    FriendlyFreeActionAfterGreen,
    /// Chewbacca: when you are dealt a faceup Damage card, immediately
    /// flip it facedown (without resolving its ability).
    FlipCritFacedownImmediately,
    /// Chewbacca (Heroes of the Resistance): after another friendly ship
    /// at Range 1-3 is destroyed (but has not fled the battlefield), you
    /// may perform an attack.
    AttackWhenFriendlyDestroyed,
    /// Rey: when attacking or defending, if the enemy ship is inside
    /// your firing arc, you may reroll up to 2 of your blank results.
    RerollTwoBlanksIfEnemyInArc,
    // ---- TIE Bomber ----
    /// Major Rhymer: when attacking with a secondary weapon, you may
    /// increase or decrease the weapon range by 1 to a limit of Range 1-3.
    SecondaryRangePlusMinus1,
    /// Tomax Bren: once per round, after you discard an Elite Upgrade
    /// card, flip that card faceup.
    FlipTalentFaceupAfterDiscard,
    /// Captain Jonus: when another friendly ship at Range 1 attacks with
    /// a secondary weapon, it may reroll up to 2 attack dice.
    FriendlySecondaryReroll2AtRange1,
    /// "Deathfire": when you reveal your maneuver dial or after you
    /// perform an action, you may perform a Bomb Upgrade card action as
    /// a free action.
    FreeBombActionOnRevealOrAction,
    // ---- TIE Advanced ----
    /// Darth Vader: during your "Perform Action" step, you may perform 2
    /// actions.
    TwoActions,
    /// Juno Eclipse: when you reveal your maneuver, you may increase or
    /// decrease its speed by 1 (to a minimum of 1).
    AdjustManeuverSpeedBy1,
    /// Maarek Stele: when your attack deals a faceup Damage card to the
    /// defender, instead draw 3 Damage cards, choose 1 to deal, and
    /// discard the others.
    ChooseCritFromThree,
    /// Zertik Strom: enemy ships at Range 1 cannot add their range
    /// combat bonus when attacking.
    DenyEnemyRange1Bonus,
    /// Commander Alozen: at the start of the Combat phase, you may
    /// acquire a target lock on an enemy ship at Range 1.
    LockAtRange1AtCombatStart,
    /// Lieutenant Colzet: at the start of the End phase, you may spend a
    /// target lock you have on an enemy ship to flip 1 random facedown
    /// Damage card assigned to it faceup.
    SpendLockToFlipFacedownCrit,
    // ---- TIE Interceptor ----
    /// Soontir Fel: when you receive a stress token, you may assign 1
    /// focus token to your ship.
    FocusOnStress,
    /// Carnor Jax: enemy ships at Range 1 cannot perform focus or evade
    /// actions and cannot spend focus or evade tokens.
    DenyFocusEvadeAtRange1,
    /// Turr Phennir: after you perform an attack, you may perform a free
    /// boost or barrel roll action.
    FreeRepositionAfterAttack,
    /// Tetran Cowall: when you reveal a Koiogran turn, you may treat its
    /// speed as "1", "3", or "5".
    KTurnSpeed1Or3Or5,
    /// Kir Kanos: when attacking at Range 2-3, you may spend 1 evade
    /// token to add 1 hit result to your roll.
    SpendEvadeForHitAtRange2To3,
    /// "Fel's Wrath": when the number of Damage cards assigned to you
    /// equals or exceeds your hull value, you are not destroyed until
    /// the end of the Combat phase.
    SurviveUntilEndOfCombat,
    /// Lieutenant Lorrir: when performing a barrel roll, you may receive
    /// 1 stress token to use the bank 1 templates instead of straight 1.
    BarrelRollWithBank1ForStress,
    // ---- Lambda-class shuttle ----
    /// Captain Kagi: when an enemy ship acquires a target lock, it must
    /// lock onto your ship if able.
    EnemyLocksMustTargetMe,
    /// Colonel Jendon: at the start of the Combat phase, you may assign 1
    /// of your blue target lock tokens to a friendly ship at Range 1 if
    /// it does not have a blue target lock token.
    GiveLockToFriendlyAtCombatStart,
    /// Captain Yorr: when another friendly ship at Range 1-2 would
    /// receive a stress token, if you have 2 or fewer stress tokens, you
    /// may receive that token instead.
    AbsorbFriendlyStressAtRange1To2,
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
    /// Printed stats when the pilot card differs from the chassis
    /// (Outer Rim Smuggler: 2/1/6/4 on a 3/1/8/5 ship).
    #[serde(default)]
    pub stats: Option<crate::ship::StatBlock>,
    /// Has an Elite Pilot Talent slot beyond the chassis upgrade bar.
    #[serde(default)]
    pub talent_slot: bool,
    pub source: Source,
    #[serde(default)]
    pub ability: Option<PilotAbility>,
}
