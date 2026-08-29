use serde::{Deserialize, Serialize};

use sf_core::game::Phase;
use sf_core::geometry::Pose;
use sf_core::ship::{PlayerId, ShipId};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        proto_version: u32,
        name: String,
        /// Server password, sent inside the established TLS tunnel.
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
        /// Index into the ship's ManeuverSet.
        maneuver_index: u8,
    },
    CommitPlans,
    Resign,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Welcome {
        player_id: PlayerId,
        reconnect_token: String,
    },
    GameCreated {
        code: String,
    },
    PhaseChanged {
        phase: Phase,
    },
    Rejected {
        reason: String,
    },
    Error {
        message: String,
    },
    Pong,
}
