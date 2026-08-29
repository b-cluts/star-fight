use serde::{Deserialize, Serialize};
use std::ops::{Add, Sub};

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

    pub fn dot(self, other: Self) -> f64 {
        self.x * other.x + self.y * other.y
    }

    /// Counter-clockwise perpendicular.
    pub fn perp(self) -> Self {
        Self::new(-self.y, self.x)
    }
}

impl Add for Vec2 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for Vec2 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y)
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

impl Pose {
    pub fn new(x: f64, y: f64, heading: f64) -> Self {
        Self { anchor: Vec2::new(x, y), heading }
    }

    /// Map a point from the ship's local frame (origin at the anchor,
    /// +X ahead, +Y to port/left) into world coordinates.
    pub fn local_to_world(&self, local: Vec2) -> Vec2 {
        let (s, c) = self.heading.sin_cos();
        Vec2::new(
            self.anchor.x + c * local.x - s * local.y,
            self.anchor.y + s * local.x + c * local.y,
        )
    }

    /// Advance straight ahead by `dist` units.
    pub fn advanced(&self, dist: f64) -> Self {
        Self { anchor: self.local_to_world(Vec2::new(dist, 0.0)), heading: self.heading }
    }

    /// Rotate in place around the anchor; positive = left (CCW).
    pub fn rotated(&self, angle: f64) -> Self {
        Self { anchor: self.anchor, heading: self.heading + angle }
    }

    /// Follow a circular arc of the given centerline radius. `sweep` is the
    /// signed heading change in radians: positive = left, negative = right.
    pub fn arced(&self, radius: f64, sweep: f64) -> Self {
        let a = sweep.abs();
        let side = if sweep >= 0.0 { 1.0 } else { -1.0 };
        let local = Vec2::new(radius * a.sin(), side * radius * (1.0 - a.cos()));
        Self { anchor: self.local_to_world(local), heading: self.heading + sweep }
    }
}

/// Ship footprint: a rectangle extending backward from the front-center
/// anchor — half of `width` to each side, `length` behind.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Footprint {
    pub length: f64,
    pub width: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::FRAC_PI_2;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn local_to_world_rotates_and_translates() {
        // Facing +Y (heading 90°): local "ahead" is world +Y, local left is world -X.
        let p = Pose::new(10.0, 5.0, FRAC_PI_2);
        let w = p.local_to_world(Vec2::new(2.0, 1.0));
        assert!(approx(w.x, 9.0), "{w:?}");
        assert!(approx(w.y, 7.0), "{w:?}");
    }

    #[test]
    fn arc_quarter_circle_left() {
        // 90° left from origin: end at (r, r), facing +Y.
        let p = Pose::new(0.0, 0.0, 0.0).arced(2.0, FRAC_PI_2);
        assert!(approx(p.anchor.x, 2.0));
        assert!(approx(p.anchor.y, 2.0));
        assert!(approx(p.heading, FRAC_PI_2));
    }

    #[test]
    fn arc_right_mirrors_left() {
        let l = Pose::new(0.0, 0.0, 0.0).arced(3.0, 0.7);
        let r = Pose::new(0.0, 0.0, 0.0).arced(3.0, -0.7);
        assert!(approx(l.anchor.x, r.anchor.x));
        assert!(approx(l.anchor.y, -r.anchor.y));
        assert!(approx(l.heading, -r.heading));
    }
}
