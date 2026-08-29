//! Parsing of the RON data files (`assets/data/*.ron`). Pure string → struct
//! deserialization; reading files from disk is the caller's job.

use ron::extensions::Extensions;
use serde::{Deserialize, Serialize};

use crate::maneuver::{ManeuverSet, ManeuverSetId};
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
