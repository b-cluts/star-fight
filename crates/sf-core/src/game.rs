use serde::{Deserialize, Serialize};

/// The turn phase machine:
/// Setup -> Placement -> [Planning -> Resolution]* -> GameOver
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Setup,
    Placement,
    Planning,
    Resolution,
    GameOver,
}
