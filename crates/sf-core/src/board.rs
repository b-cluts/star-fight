use serde::{Deserialize, Serialize};

/// The play area. Origin at the bottom-left corner; the South edge is
/// y = 0, North is y = height, West is x = 0 and East is x = width.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub width: f64,
    pub height: f64,
    /// Deployment zone extends this far from each side's edge.
    pub deploy_depth: f64,
}

/// Which board edge a side deploys from. Two sides face each other
/// across the board; a third takes the East edge, a fourth the West.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Seat {
    /// Deploys along y = 0.
    South,
    /// Deploys along y = height.
    North,
    /// Deploys along x = width.
    East,
    /// Deploys along x = 0.
    West,
}

impl Seat {
    /// The edge for side `side` (0-based) in a game with `sides` sides.
    pub fn for_side(side: u8, sides: u8) -> Seat {
        let _ = sides;
        match side {
            0 => Seat::South,
            1 => Seat::North,
            2 => Seat::East,
            _ => Seat::West,
        }
    }

    /// Heading (radians) pointing from the edge into the board.
    pub fn facing(self) -> f64 {
        use std::f64::consts::{FRAC_PI_2, PI};
        match self {
            Seat::South => FRAC_PI_2,
            Seat::North => -FRAC_PI_2,
            Seat::East => PI,
            Seat::West => 0.0,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Seat::South => "south",
            Seat::North => "north",
            Seat::East => "east",
            Seat::West => "west",
        }
    }
}

impl Board {
    /// The rectangle (x_min, y_min, x_max, y_max) a seat may deploy in.
    pub fn deploy_zone(&self, seat: Seat) -> (f64, f64, f64, f64) {
        let d = self.deploy_depth;
        match seat {
            Seat::South => (0.0, 0.0, self.width, d),
            Seat::North => (0.0, self.height - d, self.width, self.height),
            Seat::East => (self.width - d, 0.0, self.width, self.height),
            Seat::West => (0.0, 0.0, d, self.height),
        }
    }
}
