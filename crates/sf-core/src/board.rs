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
