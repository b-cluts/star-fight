//! Physical move-template dimensions, converted to game units in one place.
//!
//! Scale anchor: a SMALL ship base is 40 mm square and exactly matches the
//! speed-1 straight template, so **1 game unit = 40 mm**.
//!
//! All templates are 20 mm wide; the ship's front-center anchor travels the
//! template CENTERLINE, so each arc's effective radius is the template's
//! inside radius + 10 mm.
//!
//! | Template          | Inside radius | Centerline | Game units |
//! |-------------------|---------------|------------|------------|
//! | Turn 1 (90°)      | 25 mm         | 35 mm      | 0.875      |
//! | Turn 2 (90°)      | 53 mm         | 63 mm      | 1.575      |
//! | Turn 3 (90°)      | 80 mm         | 90 mm      | 2.25       |
//! | Turn 4 (90°)      | 107 mm *      | 117 mm     | 2.925      |
//! | Bank 1 (45°)      | 70 mm         | 80 mm      | 2.0        |
//! | Bank 2 (45°)      | 120 mm        | 130 mm     | 3.25       |
//! | Bank 3 (45°)      | 170 mm        | 180 mm     | 4.5        |
//! | Straight n (1..5) | —             | 40·n mm    | n          |
//!
//! (*) No speed-4 turn template exists physically; this game's canonical
//! radius extends the 1–3 progression (+27 mm per speed) — adopted as the
//! standard here in preference to the physical game's makeshift rules.

pub const MM_PER_UNIT: f64 = 40.0;
pub const TEMPLATE_WIDTH_MM: f64 = 20.0;

/// Straight template length per speed, in mm (speed 1 = 40 mm … speed 5 = 200 mm).
pub const STRAIGHT_MM_PER_SPEED: f64 = 40.0;

const TURN_INSIDE_MM: [f64; 4] = [25.0, 53.0, 80.0, 107.0];
const BANK_INSIDE_MM: [f64; 3] = [70.0, 120.0, 170.0];

fn centerline_units(inside_mm: f64) -> f64 {
    (inside_mm + TEMPLATE_WIDTH_MM / 2.0) / MM_PER_UNIT
}

/// Centerline radius in game units for a 90° turn, speeds 1..=4
/// (speed 4 extrapolated — see module docs).
pub fn turn_radius(speed: u8) -> Option<f64> {
    TURN_INSIDE_MM.get(speed.checked_sub(1)? as usize).map(|&r| centerline_units(r))
}

/// Centerline radius in game units for a 45° bank, speeds 1..=3.
pub fn bank_radius(speed: u8) -> Option<f64> {
    BANK_INSIDE_MM.get(speed.checked_sub(1)? as usize).map(|&r| centerline_units(r))
}

/// Straight distance in game units, speeds 0..=5 (0 is the stationary
/// "stop" maneuver on the Lambda-class shuttle's dial).
pub fn straight_length(speed: u8) -> Option<f64> {
    (speed <= 5).then(|| speed as f64 * STRAIGHT_MM_PER_SPEED / MM_PER_UNIT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radii_match_physical_templates() {
        assert_eq!(turn_radius(1), Some(0.875));
        assert_eq!(turn_radius(2), Some(1.575));
        assert_eq!(turn_radius(3), Some(2.25));
        assert_eq!(turn_radius(4), Some(2.925));
        assert_eq!(bank_radius(1), Some(2.0));
        assert_eq!(bank_radius(2), Some(3.25));
        assert_eq!(bank_radius(3), Some(4.5));
        assert_eq!(straight_length(1), Some(1.0));
        assert_eq!(straight_length(5), Some(5.0));
    }

    #[test]
    fn out_of_range_speeds_are_rejected() {
        assert_eq!(turn_radius(0), None);
        assert_eq!(turn_radius(5), None);
        assert_eq!(bank_radius(4), None);
        assert_eq!(straight_length(0), Some(0.0));
        assert_eq!(straight_length(6), None);
    }
}
