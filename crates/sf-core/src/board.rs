use serde::{Deserialize, Serialize};

/// The play area. Origin at the bottom-left corner; player 0's edge is
/// y = 0, player 1's edge is y = height.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Board {
    pub width: f64,
    pub height: f64,
    /// Deployment zone extends this far from each player's edge.
    pub deploy_depth: f64,
}

/// Which board edge a player deploys from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Seat {
    /// Deploys along y = 0.
    South,
    /// Deploys along y = height.
    North,
}

impl Board {
    /// The (y_min, y_max) band a seat may deploy ships in.
    pub fn deploy_zone(&self, seat: Seat) -> (f64, f64) {
        match seat {
            Seat::South => (0.0, self.deploy_depth),
            Seat::North => (self.height - self.deploy_depth, self.height),
        }
    }
}
