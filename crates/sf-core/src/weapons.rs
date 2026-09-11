//! Weapon readiness summary computed from a player's snapshot, so the
//! client HUD can tell a pilot what each weapon could do right now
//! (mirrors `GameState::attack_options`, which stays authoritative).

use crate::combat;
use crate::crit::CritEffect;
use crate::data::Content;
use crate::game::ShipView;
use crate::obstacle::{self, Obstacle};
use crate::rules;
use crate::ship::ShipId;
use crate::upgrade::{AttackRequirement, Slot, UpgradeEffect, UpgradeId};

/// What one weapon could do from the current positions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WeaponState {
    /// A legal shot exists; the nearest target, its range band, and
    /// whether the range line crosses an obstacle.
    Ready { target: ShipId, range: u8, obstructed: bool },
    /// A ship is in range and arc but the weapon needs a lock on it.
    NeedsLock { target: ShipId },
    /// A ship is in range and arc but the weapon needs a focus token.
    NeedsFocus { target: ShipId },
    /// No enemy in this weapon's range and arc.
    NoTarget,
    /// Weapons Failure critical: nothing can fire.
    Offline,
    /// Sitting on an asteroid: no attack this round.
    Grounded,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeaponStatus {
    /// None = primary weapon.
    pub weapon: Option<UpgradeId>,
    pub name: String,
    pub state: WeaponState,
}

/// Readiness of the primary weapon and every equipped secondary weapon
/// of `me`, against the living enemy ships in `ships`.
pub fn weapon_status(
    content: &Content,
    ships: &[ShipView],
    obstacles: &[Obstacle],
    me: &ShipView,
) -> Vec<WeaponStatus> {
    let Some(class) = content.ships.class(me.class) else { return Vec::new() };
    // (weapon, name, min range, max range, needs arc, requirement)
    let mut weapons: Vec<(Option<UpgradeId>, String, u8, u8, bool, AttackRequirement)> =
        vec![(None, "Primary".to_string(), 1, 3, !class.turret_primary, AttackRequirement::Free)];
    for &u in &me.upgrade_ids {
        if let Some(card) = content.upgrades.upgrade(u)
            && let Some(sw) = card.attack
            && matches!(card.slot, Slot::Torpedo | Slot::Missile | Slot::Cannon | Slot::Turret)
        {
            weapons.push((
                Some(u),
                card.name.clone(),
                sw.range_min,
                sw.range_max,
                card.slot != Slot::Turret,
                sw.requires,
            ));
        }
    }
    let offline = me.crits.iter().any(|c| matches!(c, CritEffect::WeaponsFailure { .. }));
    let grounded = me.on_asteroid;
    let Some(a_pose) = me.pose else {
        return weapons
            .into_iter()
            .map(|(weapon, name, ..)| WeaponStatus { weapon, name, state: WeaponState::NoTarget })
            .collect();
    };
    let a_corners = rules::footprint_corners(a_pose, class.footprint);
    // (target, band, in arc, distance, obstructed) for every targetable enemy
    let enemies: Vec<(ShipId, u8, bool, f64, bool)> = ships
        .iter()
        .filter(|s| s.team != me.team && !s.destroyed)
        .filter_map(|s| {
            let pose = s.pose?;
            let fp = content.ships.class(s.class)?.footprint;
            let corners = rules::footprint_corners(pose, fp);
            let dist = combat::base_distance(&a_corners, &corners);
            if dist <= 0.0 {
                return None;
            }
            let band = combat::range_band_between(&a_corners, &corners)?;
            let in_arc = corners.iter().any(|&p| combat::in_front_arc(a_pose, class.footprint, p))
                || (0..4).any(|i| {
                    let m = crate::geometry::Vec2::new(
                        (corners[i].x + corners[(i + 1) % 4].x) / 2.0,
                        (corners[i].y + corners[(i + 1) % 4].y) / 2.0,
                    );
                    combat::in_front_arc(a_pose, class.footprint, m)
                });
            let (p, q) = combat::closest_points(&a_corners, &corners);
            let obstructed =
                obstacles.iter().any(|o| obstacle::segment_hits_polygon(p, q, &o.polygon()));
            Some((s.id, band, in_arc, dist, obstructed))
        })
        .collect();

    weapons
        .into_iter()
        .map(|(weapon, name, lo, hi, needs_arc, req)| {
            let state = if grounded {
                WeaponState::Grounded
            } else if offline {
                WeaponState::Offline
            } else {
                let mut candidates: Vec<&(ShipId, u8, bool, f64, bool)> = enemies
                    .iter()
                    .filter(|(_, band, in_arc, _, _)| {
                        *band >= lo && *band <= hi && (!needs_arc || *in_arc)
                    })
                    .collect();
                candidates.sort_by(|a, b| a.3.total_cmp(&b.3));
                match (candidates.first(), req) {
                    (None, _) => WeaponState::NoTarget,
                    (Some(&&(t, band, _, _, obstructed)), AttackRequirement::Free) => {
                        WeaponState::Ready { target: t, range: band, obstructed }
                    }
                    (Some(&&(t, band, _, _, obstructed)), AttackRequirement::TargetLock) => {
                        // Deadeye: a focus token stands in for the lock.
                        let deadeye = me.focus > 0
                            && me.upgrade_ids.iter().any(|u| {
                                content.upgrades.upgrade(*u).and_then(|c| c.effect)
                                    == Some(UpgradeEffect::LockBecomesFocus)
                            });
                        match candidates
                            .iter()
                            .find(|c| me.lock == Some(c.0) || me.lock2 == Some(c.0))
                        {
                            Some(&&(locked, band, _, _, obstructed)) => {
                                WeaponState::Ready { target: locked, range: band, obstructed }
                            }
                            None if deadeye => {
                                WeaponState::Ready { target: t, range: band, obstructed }
                            }
                            None => WeaponState::NeedsLock { target: t },
                        }
                    }
                    (Some(&&(t, band, _, _, obstructed)), AttackRequirement::Focus) => {
                        if me.focus > 0 {
                            WeaponState::Ready { target: t, range: band, obstructed }
                        } else {
                            WeaponState::NeedsFocus { target: t }
                        }
                    }
                }
            };
            WeaponStatus { weapon, name, state }
        })
        .collect()
}

/// The weapons that cannot fire right now, as (weapon name, reason)
/// pairs for the Declare Target prompt.
pub fn unavailable_reasons(
    content: &Content,
    ships: &[ShipView],
    obstacles: &[Obstacle],
    me: &ShipView,
) -> Vec<(String, String)> {
    let callsign = |id: ShipId| {
        ships.iter().find(|v| v.id == id).map(|v| v.callsign.clone()).unwrap_or_default()
    };
    weapon_status(content, ships, obstacles, me)
        .into_iter()
        .filter_map(|w| {
            let why = match w.state {
                WeaponState::Ready { .. } => return None,
                WeaponState::NeedsLock { target } => {
                    format!("needs a target lock on {}", callsign(target))
                }
                WeaponState::NeedsFocus { .. } => "needs a focus token".to_string(),
                WeaponState::NoTarget => "no target in range or arc".to_string(),
                WeaponState::Offline => "weapons failure".to_string(),
                WeaponState::Grounded => "sitting on an asteroid".to_string(),
            };
            Some((w.name, why))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::board::Board;
    use crate::game::GameState;
    use crate::geometry::Pose;
    use crate::ship::PlayerId;
    use crate::squad::Squad;
    use std::f64::consts::FRAC_PI_2;

    fn content() -> Content {
        Content::load_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data")).unwrap()
    }

    fn state_for(c: &Content) -> Vec<WeaponStatus> {
        let torps = UpgradeId(1); // Proton Torpedoes: R2-3, needs a lock
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let a = Squad::basic(c, "i", &[pilot("academypilot")]);
        let b = Squad::basic(c, "r", &[pilot("bluesquadronnovice")]);
        let board = Board { width: 20.0, height: 20.0, deploy_depth: 3.0 };
        let mut gs =
            GameState::from_squads(board, c, &[&a, &b], &[0, 1], crate::dice::AttackFace::Hit)
                .unwrap();
        gs.ships[1].upgrades.push(torps);
        gs.place_ship(c, PlayerId(0), ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(c, PlayerId(1), ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        // X-Wing 2.5 units north of the TIE, nose-to-nose: Range 2.
        gs.ships[1].pose = Some(Pose::new(10.0, 6.0, -FRAC_PI_2));
        let ships = gs.snapshot_for(c, PlayerId(1));
        weapon_status(c, &ships, &[], &ships[1])
    }

    #[test]
    fn torpedoes_need_a_lock_on_the_target_in_arc() {
        let c = content();
        let st = state_for(&c);
        assert_eq!(st.len(), 2);
        assert_eq!(st[0].name, "Primary");
        assert_eq!(
            st[0].state,
            WeaponState::Ready { target: ShipId(0), range: 2, obstructed: false }
        );
        assert_eq!(st[1].name, "Proton Torpedoes");
        assert_eq!(st[1].state, WeaponState::NeedsLock { target: ShipId(0) });
    }

    #[test]
    fn unavailable_reasons_name_the_weapon_and_the_missing_piece() {
        let c = content();
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let a = Squad::basic(&c, "i", &[pilot("academypilot")]);
        let b = Squad::basic(&c, "r", &[pilot("bluesquadronnovice")]);
        let board = Board { width: 20.0, height: 20.0, deploy_depth: 3.0 };
        let mut gs =
            GameState::from_squads(board, &c, &[&a, &b], &[0, 1], crate::dice::AttackFace::Hit)
                .unwrap();
        gs.ships[1].upgrades.push(UpgradeId(1));
        gs.place_ship(&c, PlayerId(0), ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, PlayerId(1), ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        gs.ships[1].pose = Some(Pose::new(10.0, 6.0, -FRAC_PI_2));
        let ships = gs.snapshot_for(&c, PlayerId(1));
        let why = unavailable_reasons(&c, &ships, &[], &ships[1]);
        assert_eq!(why.len(), 1, "{why:?}");
        assert_eq!(why[0].0, "Proton Torpedoes");
        assert!(why[0].1.contains("target lock on"), "{}", why[0].1);
        assert!(why[0].1.contains(&ships[0].callsign), "{}", why[0].1);
    }

    #[test]
    fn locked_torpedoes_are_ready_and_offline_after_weapons_failure() {
        let c = content();
        let pilot = |x: &str| c.pilots.pilots.iter().find(|p| p.xws == x).unwrap().id;
        let a = Squad::basic(&c, "i", &[pilot("academypilot")]);
        let b = Squad::basic(&c, "r", &[pilot("bluesquadronnovice")]);
        let board = Board { width: 20.0, height: 20.0, deploy_depth: 3.0 };
        let mut gs =
            GameState::from_squads(board, &c, &[&a, &b], &[0, 1], crate::dice::AttackFace::Hit)
                .unwrap();
        gs.ships[1].upgrades.push(UpgradeId(1));
        gs.place_ship(&c, PlayerId(0), ShipId(0), Pose::new(10.0, 2.5, FRAC_PI_2)).unwrap();
        gs.place_ship(&c, PlayerId(1), ShipId(1), Pose::new(10.0, 17.5, -FRAC_PI_2)).unwrap();
        gs.ships[1].pose = Some(Pose::new(10.0, 6.0, -FRAC_PI_2));
        gs.ships[1].lock = Some(ShipId(0));
        let ships = gs.snapshot_for(&c, PlayerId(1));
        let st = weapon_status(&c, &ships, &[], &ships[1]);
        assert_eq!(
            st[1].state,
            WeaponState::Ready { target: ShipId(0), range: 2, obstructed: false }
        );

        // Facing away: nothing in arc, so no target for either weapon.
        gs.ships[1].pose = Some(Pose::new(10.0, 6.0, FRAC_PI_2));
        let ships = gs.snapshot_for(&c, PlayerId(1));
        let st = weapon_status(&c, &ships, &[], &ships[1]);
        assert_eq!(st[0].state, WeaponState::NoTarget);
        assert_eq!(st[1].state, WeaponState::NoTarget);

        gs.ships[1].crits.push(CritEffect::WeaponsFailure { rounds: 2 });
        let ships = gs.snapshot_for(&c, PlayerId(1));
        let st = weapon_status(&c, &ships, &[], &ships[1]);
        assert!(st.iter().all(|w| w.state == WeaponState::Offline));
    }
}
