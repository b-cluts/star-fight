use serde::{Deserialize, Serialize};

use sf_core::game::{MoveRecord, Phase, ShipView};
use sf_core::geometry::Pose;
use sf_core::ship::ShipId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        proto_version: u32,
        name: String,
        /// Server password, sent inside the (M4: TLS) tunnel.
        password: String,
    },
    CreateGame,
    JoinGame {
        code: String,
    },
    PlaceShip {
        ship_id: ShipId,
        pose: Pose,
    },
    PlanManeuver {
        ship_id: ShipId,
        /// Index into the ship's dial.
        maneuver_index: u8,
    },
    CommitPlans,
    Resign,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Handshake accepted.
    Welcome {
        reconnect_token: String,
    },
    /// Your game exists; share the code with your opponent.
    GameCreated {
        code: String,
    },
    /// Both players present — the match begins. You are `seat`
    /// (0 deploys South, 1 North).
    GameStart {
        seat: u8,
        opponent: String,
    },
    /// Your current view of the game (already filtered for you).
    Snapshot {
        phase: Phase,
        turn: u32,
        ships: Vec<ShipView>,
        committed: [bool; 2],
        /// Seat holding the initiative token (breaks pilot-skill ties:
        /// moves first AND fires first at equal skill).
        initiative: u8,
        squad_totals: [u32; 2],
    },
    /// A command of yours was refused.
    Rejected {
        reason: String,
    },
    /// Both sides committed: the revealed, resolved movement.
    TurnResult {
        moves: Vec<MoveRecord>,
    },
    GameOver {
        /// Winning seat; None on mutual destruction.
        winner: Option<u8>,
        reason: String,
    },
    Error {
        message: String,
    },
    Pong,
}
