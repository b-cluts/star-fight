//! Obstacles (core rules p.20): asteroid and debris tokens on the board.
//! Convex polygon tokens; a ship whose base or template overlaps one
//! suffers the token's effect (see `GameState::resolve_movement`), and an
//! attack whose line of sight crosses one is obstructed.

use serde::{Deserialize, Serialize};

use crate::board::Board;
use crate::combat::RANGE_BAND_UNITS;
use crate::geometry::Vec2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObstacleKind {
    /// Overlap: skip the action, roll one attack die (hit: 1 damage,
    /// crit: faceup card); a ship ending on it cannot attack this round.
    Asteroid,
    /// Overlap: receive a stress token, roll one attack die (crit:
    /// faceup card). Attacks and (stress permitting) actions go on.
    Debris,
    /// A black core (`CORE_RADIUS`) that swallows any ship whose base
    /// touches it. The gravity pull (every ship within Range 5 dragged
    /// one unit toward the core after all moves) is not implemented yet.
    BlackHole,
}

/// Radius of a black hole's core, in units (1-unit diameter).
pub const CORE_RADIUS: f64 = 0.5;

impl ObstacleKind {
    pub fn name(self) -> &'static str {
        match self {
            ObstacleKind::Asteroid => "asteroid",
            ObstacleKind::Debris => "debris cloud",
            ObstacleKind::BlackHole => "black hole",
        }
    }
}

/// Token outlines in local units (convex, roughly 1.2-2 units across).
pub const SHAPES: [&[(f64, f64)]; 4] = [
    &[(0.8, 0.0), (0.4, 0.7), (-0.4, 0.75), (-0.85, 0.1), (-0.5, -0.6), (0.3, -0.7)],
    &[(1.0, 0.1), (0.5, 0.55), (-0.3, 0.6), (-0.95, 0.2), (-0.8, -0.4), (0.1, -0.65), (0.7, -0.45)],
    &[(0.6, 0.0), (0.3, 0.5), (-0.35, 0.55), (-0.65, 0.0), (-0.3, -0.5), (0.35, -0.5)],
    &[(0.9, -0.2), (0.7, 0.45), (0.0, 0.75), (-0.7, 0.5), (-0.9, -0.1), (-0.4, -0.7), (0.4, -0.7)],
];

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Obstacle {
    pub id: u32,
    pub kind: ObstacleKind,
    pub center: Vec2,
    pub heading: f64,
    /// Index into `SHAPES`.
    pub shape: u8,
}

impl Obstacle {
    /// World-space outline (a black hole's is its core, as a 12-gon).
    pub fn polygon(&self) -> Vec<Vec2> {
        if self.kind == ObstacleKind::BlackHole {
            return (0..12)
                .map(|k| {
                    let a = k as f64 * std::f64::consts::TAU / 12.0;
                    Vec2::new(
                        self.center.x + CORE_RADIUS * a.cos(),
                        self.center.y + CORE_RADIUS * a.sin(),
                    )
                })
                .collect();
        }
        let (s, c) = self.heading.sin_cos();
        SHAPES[self.shape as usize % SHAPES.len()]
            .iter()
            .map(|&(x, y)| Vec2::new(self.center.x + c * x - s * y, self.center.y + s * x + c * y))
            .collect()
    }
}

fn project(poly: &[Vec2], axis: Vec2) -> (f64, f64) {
    poly.iter()
        .map(|p| p.dot(axis))
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)))
}

/// Separating-axis overlap test for two convex polygons.
pub fn convex_overlap(a: &[Vec2], b: &[Vec2]) -> bool {
    for poly in [a, b] {
        for i in 0..poly.len() {
            let axis = (poly[(i + 1) % poly.len()] - poly[i]).perp();
            let (amin, amax) = project(a, axis);
            let (bmin, bmax) = project(b, axis);
            if amax < bmin || bmax < amin {
                return false;
            }
        }
    }
    true
}

fn cross(o: Vec2, a: Vec2, b: Vec2) -> f64 {
    (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
}

fn segments_cross(p1: Vec2, p2: Vec2, q1: Vec2, q2: Vec2) -> bool {
    let d1 = cross(q1, q2, p1);
    let d2 = cross(q1, q2, p2);
    let d3 = cross(p1, p2, q1);
    let d4 = cross(p1, p2, q2);
    ((d1 > 0.0) != (d2 > 0.0)) && ((d3 > 0.0) != (d4 > 0.0))
}

/// Is `p` inside the convex polygon (counter-clockwise or clockwise)?
pub fn point_in_convex(p: Vec2, poly: &[Vec2]) -> bool {
    let mut sign = 0.0;
    for i in 0..poly.len() {
        let c = cross(poly[i], poly[(i + 1) % poly.len()], p);
        if c != 0.0 {
            if sign == 0.0 {
                sign = c.signum();
            } else if c.signum() != sign {
                return false;
            }
        }
    }
    true
}

/// Does the segment from `p` to `q` touch the polygon (cross an edge or
/// start/end inside it)?
pub fn segment_hits_polygon(p: Vec2, q: Vec2, poly: &[Vec2]) -> bool {
    if point_in_convex(p, poly) || point_in_convex(q, poly) {
        return true;
    }
    (0..poly.len()).any(|i| segments_cross(p, q, poly[i], poly[(i + 1) % poly.len()]))
}

fn point_seg_distance(p: Vec2, a: Vec2, b: Vec2) -> f64 {
    let ab = b - a;
    let len2 = ab.dot(ab);
    let t = if len2 == 0.0 { 0.0 } else { ((p - a).dot(ab) / len2).clamp(0.0, 1.0) };
    let c = Vec2::new(a.x + ab.x * t, a.y + ab.y * t);
    let d = p - c;
    (d.x * d.x + d.y * d.y).sqrt()
}

/// Closest distance between two convex polygons (0 when they overlap).
pub fn polygon_distance(a: &[Vec2], b: &[Vec2]) -> f64 {
    if convex_overlap(a, b) {
        return 0.0;
    }
    let mut min = f64::INFINITY;
    for i in 0..a.len() {
        for j in 0..b.len() {
            let (a1, a2) = (a[i], a[(i + 1) % a.len()]);
            let (b1, b2) = (b[j], b[(j + 1) % b.len()]);
            min = min.min(point_seg_distance(a1, b1, b2)).min(point_seg_distance(b1, a1, a2));
        }
    }
    min
}

/// Scatter `kinds.len()` tokens on the board: every vertex beyond Range 1
/// of the edges, every token beyond Range 1 of the others (the core
/// rules' placement limits, drawn at random instead of by the players).
/// Deterministic for a given `seed`.
pub fn scatter(board: &Board, kinds: &[ObstacleKind], seed: u64) -> Vec<Obstacle> {
    let mut state = seed ^ 0x9E37_79B9_7F4A_7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    let margin = RANGE_BAND_UNITS;
    let mut out: Vec<Obstacle> = Vec::new();
    for (k, &kind) in kinds.iter().enumerate() {
        for _ in 0..500 {
            let cand = Obstacle {
                id: k as u32,
                kind,
                center: Vec2::new(
                    margin + 1.0 + next() * (board.width - 2.0 * (margin + 1.0)),
                    margin + 1.0 + next() * (board.height - 2.0 * (margin + 1.0)),
                ),
                heading: next() * std::f64::consts::TAU,
                shape: (next() * SHAPES.len() as f64) as u8,
            };
            let poly = cand.polygon();
            let inside = poly.iter().all(|p| {
                p.x > margin
                    && p.x < board.width - margin
                    && p.y > margin
                    && p.y < board.height - margin
            });
            let apart = out.iter().all(|o| polygon_distance(&poly, &o.polygon()) > margin);
            if inside && apart {
                out.push(cand);
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(cx: f64, cy: f64, half: f64) -> Vec<Vec2> {
        vec![
            Vec2::new(cx - half, cy - half),
            Vec2::new(cx + half, cy - half),
            Vec2::new(cx + half, cy + half),
            Vec2::new(cx - half, cy + half),
        ]
    }

    #[test]
    fn overlap_and_distance() {
        let a = square(0.0, 0.0, 1.0);
        assert!(convex_overlap(&a, &square(1.5, 0.0, 1.0)));
        assert!(!convex_overlap(&a, &square(3.0, 0.0, 1.0)));
        assert!((polygon_distance(&a, &square(3.0, 0.0, 1.0)) - 1.0).abs() < 1e-9);
        assert_eq!(polygon_distance(&a, &square(0.5, 0.5, 1.0)), 0.0);
    }

    #[test]
    fn segments_and_points() {
        let a = square(0.0, 0.0, 1.0);
        assert!(segment_hits_polygon(Vec2::new(-3.0, 0.0), Vec2::new(3.0, 0.0), &a));
        assert!(segment_hits_polygon(Vec2::new(0.0, 0.0), Vec2::new(5.0, 5.0), &a));
        assert!(!segment_hits_polygon(Vec2::new(-3.0, 2.0), Vec2::new(3.0, 2.0), &a));
        assert!(point_in_convex(Vec2::new(0.2, -0.3), &a));
        assert!(!point_in_convex(Vec2::new(1.2, 0.0), &a));
    }

    #[test]
    fn scatter_keeps_the_spacing_rules() {
        let board = Board { width: 20.0, height: 20.0, deploy_depth: 3.0 };
        let kinds = [ObstacleKind::Asteroid; 6];
        let obs = scatter(&board, &kinds, 42);
        assert_eq!(obs.len(), 6);
        for (i, o) in obs.iter().enumerate() {
            for p in o.polygon() {
                assert!(p.x > 2.5 && p.x < 17.5 && p.y > 2.5 && p.y < 17.5, "{p:?}");
            }
            for other in &obs[i + 1..] {
                assert!(polygon_distance(&o.polygon(), &other.polygon()) > 2.5);
            }
        }
        assert_eq!(scatter(&board, &kinds, 42), obs, "deterministic");
    }
}
