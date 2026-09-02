//! Parsing of the RON data files (`assets/data/*.ron`). Pure string → struct
//! deserialization; reading files from disk is the caller's job.

use ron::extensions::Extensions;
use serde::{Deserialize, Serialize};

use crate::maneuver::{ManeuverSet, ManeuverSetId};
use crate::pilot::{Pilot, PilotId, Source};
use crate::ship::{ShipClass, ShipClassId};

/// RON parser configured so ID newtypes (`ShipClassId(1)`) can be written
/// as plain values (`id: 1`) in the data files.
fn parser() -> ron::Options {
    ron::Options::default().with_default_extension(Extensions::UNWRAP_NEWTYPES)
}

/// Contents of `assets/data/ships.ron`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShipDb {
    pub classes: Vec<ShipClass>,
}

impl ShipDb {
    pub fn from_ron(s: &str) -> Result<Self, ron::error::SpannedError> {
        parser().from_str(s)
    }

    pub fn class(&self, id: ShipClassId) -> Option<&ShipClass> {
        self.classes.iter().find(|c| c.id == id)
    }
}

/// Everything loaded from the data files — the static content both server
/// and client consult while running a game.
#[derive(Debug, Clone)]
pub struct Content {
    pub ships: ShipDb,
    pub dials: ManeuverDb,
    pub pilots: PilotDb,
}

impl Content {
    pub fn from_ron(
        ships: &str,
        dials: &str,
        pilots: &str,
    ) -> Result<Self, ron::error::SpannedError> {
        Ok(Self {
            ships: ShipDb::from_ron(ships)?,
            dials: ManeuverDb::from_ron(dials)?,
            pilots: PilotDb::from_ron(pilots)?,
        })
    }
}

/// Contents of `assets/data/pilots.ron`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PilotDb {
    pub pilots: Vec<Pilot>,
}

impl PilotDb {
    pub fn from_ron(s: &str) -> Result<Self, ron::error::SpannedError> {
        parser().from_str(s)
    }

    pub fn pilot(&self, id: PilotId) -> Option<&Pilot> {
        self.pilots.iter().find(|p| p.id == id)
    }

    /// The cheapest (basic, non-unique) pilot of a class — what fixed
    /// fleets fly until the squad builder exists.
    pub fn basic_for(&self, class: ShipClassId) -> Option<&Pilot> {
        self.pilots.iter().filter(|p| p.class == class && !p.unique).min_by_key(|p| p.cost)
    }

    /// Relative path of a pilot's card image in an XWS-layout card
    /// directory: `pilots/<faction>/<ship>/<pilot>.png`. The images
    /// themselves are not shipped with the game (a local clone of
    /// voidstate/xwing-card-images, configured in the client).
    pub fn card_image(&self, ships: &ShipDb, id: PilotId) -> Option<String> {
        let p = self.pilot(id)?;
        let c = ships.class(p.class)?;
        Some(format!("pilots/{}/{}/{}.png", c.faction.xws(), c.xws, p.xws))
    }

    /// Pilots of a class, optionally restricted to some source packs.
    pub fn roster(&self, class: ShipClassId, sources: Option<&[Source]>) -> Vec<&Pilot> {
        self.pilots
            .iter()
            .filter(|p| p.class == class)
            .filter(|p| sources.is_none_or(|s| s.contains(&p.source)))
            .collect()
    }
}

/// Contents of `assets/data/maneuvers.ron`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManeuverDb {
    pub sets: Vec<ManeuverSet>,
}

impl ManeuverDb {
    pub fn from_ron(s: &str) -> Result<Self, ron::error::SpannedError> {
        parser().from_str(s)
    }

    pub fn set(&self, id: ManeuverSetId) -> Option<&ManeuverSet> {
        self.sets.iter().find(|s| s.id == id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Pose;
    use crate::maneuver;

    fn read_asset(name: &str) -> String {
        let path = format!("{}/../../assets/data/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
    }

    #[test]
    fn pilots_reference_real_classes_and_core_rosters_exist() {
        let ships = ShipDb::from_ron(&read_asset("ships.ron")).expect("ships.ron");
        let pilots = PilotDb::from_ron(&read_asset("pilots.ron")).expect("pilots.ron");
        let mut ids = std::collections::HashSet::new();
        for p in &pilots.pilots {
            assert!(ids.insert(p.id), "duplicate pilot id {:?}", p.id);
            assert!(ships.class(p.class).is_some(), "{} flies an unknown class", p.name);
            assert!((1..=12).contains(&p.skill), "{} skill out of range", p.name);
            assert!(p.cost > 0);
        }
        for class in &ships.classes {
            let basic = pilots
                .basic_for(class.id)
                .unwrap_or_else(|| panic!("{} has no pilots", class.name));
            assert!(!basic.unique && basic.ability.is_none(), "{} basic pilot", class.name);
        }
        // The Force Awakens core set: 4 T-70 pilots, 6 TIE/fo pilots.
        assert_eq!(pilots.roster(ShipClassId(2), Some(&[Source::CoreSet])).len(), 4);
        assert_eq!(pilots.roster(ShipClassId(3), Some(&[Source::CoreSet])).len(), 6);
        // TIE/fo pilots pay 3 points over the TIE/ln at equal skill (1, 3, 4).
        for (ln, fo) in [(112, 306), (111, 305), (110, 304)] {
            let a = pilots.pilot(PilotId(ln)).unwrap();
            let b = pilots.pilot(PilotId(fo)).unwrap();
            assert_eq!(a.skill, b.skill);
            assert_eq!(a.cost + 3, b.cost);
        }
    }

    /// Every pilot has a card image in the (gitignored) reference clone
    /// when it is present; skipped silently otherwise.
    #[test]
    fn pilot_card_images_exist_in_reference_clone() {
        let root =
            format!("{}/../../reference/xwing-card-images/images", env!("CARGO_MANIFEST_DIR"));
        if !std::path::Path::new(&root).is_dir() {
            eprintln!("reference card images not present; skipping");
            return;
        }
        let ships = ShipDb::from_ron(&read_asset("ships.ron")).expect("ships.ron");
        let pilots = PilotDb::from_ron(&read_asset("pilots.ron")).expect("pilots.ron");
        for p in &pilots.pilots {
            let rel = pilots.card_image(&ships, p.id).unwrap();
            assert!(
                std::path::Path::new(&format!("{root}/{rel}")).is_file(),
                "missing card image {rel} for {}",
                p.name
            );
        }
    }

    #[test]
    fn real_data_files_parse_and_cross_reference() {
        let ships = ShipDb::from_ron(&read_asset("ships.ron")).expect("ships.ron");
        let dials = ManeuverDb::from_ron(&read_asset("maneuvers.ron")).expect("maneuvers.ron");
        assert!(!ships.classes.is_empty());
        for class in &ships.classes {
            // every ship's dial exists and is non-empty
            let set = dials
                .set(class.maneuver_set)
                .unwrap_or_else(|| panic!("{} references missing dial", class.name));
            assert!(!set.maneuvers.is_empty(), "{} has an empty dial", class.name);
            // anchor pixel lies within the sprite
            let (ax, ay) = class.anchor_px;
            let (w, h) = class.sprite_px;
            assert!(ax < w && ay < h, "{} anchor outside sprite", class.name);
            assert!(class.footprint.length > 0.0 && class.footprint.width > 0.0);
        }
        // every maneuver in every dial expands to valid geometry
        for set in &dials.sets {
            for &m in &set.maneuvers {
                maneuver::apply(Pose::new(0.0, 0.0, 0.0), m)
                    .unwrap_or_else(|e| panic!("dial {:?}: {e}", set.id));
            }
        }
    }
}
