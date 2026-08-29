use serde::{Deserialize, Serialize};

/// One game unit — the abstract length everything is expressed in
/// (roughly one small-ship length). Tune the visual scale in one place.
pub const UNIT: f64 = 1.0;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f64,
    pub y: f64,
}

impl Vec2 {
    pub const ZERO: Self = Self { x: 0.0, y: 0.0 };

    pub fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

/// A ship's placement on the board.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Pose {
    /// Position of the ship's FRONT-CENTER point — the maneuver anchor.
    pub anchor: Vec2,
    /// Facing in radians; 0 = +X, counter-clockwise positive.
    pub heading: f64,
}

/// Ship footprint: a rectangle extending backward from the front-center
/// anchor — half of `width` to each side, `length` behind.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    pub length: f64,
    pub width: f64,
}
