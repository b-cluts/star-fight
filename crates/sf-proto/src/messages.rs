use serde::{Deserialize, Serialize};

use sf_core::action::PlannedAction;
use sf_core::board::Board;
use sf_core::game::{AttackRecord, MoveRecord, Phase, ShipView};
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
    /// Secretly assign the ship's one post-move action.
    PlanAction {
        ship_id: ShipId,
        action: PlannedAction,
    },
    CommitPlans,
    /// Answer to ChooseTarget: which eligible enemy to attack.
    DeclareTarget {
        target: ShipId,
    },
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
        board: Board,
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
    /// Both sides committed: the Activation phase (moves + actions) has
    /// resolved. Combat follows as a stream of AttackResult / ChooseTarget
    /// / OpponentChoosing messages, closed by TurnEnd.
    MovementResult {
        moves: Vec<MoveRecord>,
        events: Vec<String>,
    },
    /// One attack resolved in the Combat phase.
    AttackResult {
        attack: AttackRecord,
        /// Narrated side effects since the previous message.
        events: Vec<String>,
    },
    /// Your ship has several eligible targets: answer with DeclareTarget.
    ChooseTarget {
        attacker: ShipId,
        /// (enemy ship, range band)
        candidates: Vec<(ShipId, u8)>,
    },
    /// The opponent is declaring a target for one of their ships.
    OpponentChoosing {
        attacker: ShipId,
    },
    /// Combat and the End phase are done; a Snapshot follows.
    TurnEnd {
        events: Vec<String>,
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
