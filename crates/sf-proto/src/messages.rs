use serde::{Deserialize, Serialize};

use sf_core::action::PlannedAction;
use sf_core::board::Board;
use sf_core::bombs::{BombToken, Detonation};
use sf_core::game::{AttackRecord, MoveRecord, Phase, ShipView};
use sf_core::geometry::Pose;
use sf_core::obstacle::{Obstacle, Pull};
use sf_core::scenario::GameSetup;
use sf_core::ship::ShipId;
use sf_core::squad::Squad;
use sf_core::upgrade::UpgradeId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello {
        proto_version: u32,
        name: String,
        /// Server password, sent inside the (M4: TLS) tunnel.
        password: String,
    },
    /// `squad`: the fleet to fly; None = the server's basic fixed fleet
    /// for your seat. Validated server-side (SquadRules).
    CreateGame {
        squad: Option<Squad>,
        /// Board, obstacles, squad points and player count chosen by the
        /// host; None = the server's defaults.
        #[serde(default)]
        setup: Option<GameSetup>,
    },
    JoinGame {
        code: String,
        squad: Option<Squad>,
    },
    PlaceShip {
        ship_id: ShipId,
        pose: Pose,
    },
    /// Placement phase only: give an own ship its squad callsign.
    Rename {
        ship_id: ShipId,
        callsign: String,
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
    /// Secretly choose a bomb card to drop when the dial is revealed
    /// (None = keep it).
    PlanBomb {
        ship_id: ShipId,
        bomb: Option<UpgradeId>,
    },
    /// Secretly plan a second action where an ability grants one
    /// (ShipView.extras.second); None clears it.
    PlanSecondAction {
        ship_id: ShipId,
        action: Option<PlannedAction>,
    },
    CommitPlans,
    /// Answer to ChooseTarget: which eligible enemy to attack, and with
    /// which weapon (None = primary weapon).
    DeclareTarget {
        target: ShipId,
        #[serde(default)]
        weapon: Option<UpgradeId>,
    },
    Resign,
    Ping,
}

/// One selectable attack in a ChooseTarget prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttackChoice {
    /// None = primary weapon; Some = an equipped secondary weapon card.
    pub weapon: Option<UpgradeId>,
    pub target: ShipId,
    pub range: u8,
    /// The range line crosses an obstacle (+1 defense die).
    #[serde(default)]
    pub obstructed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    /// Handshake accepted.
    Welcome {
        reconnect_token: String,
    },
    /// Your game exists; share the code with the other players.
    GameCreated {
        code: String,
    },
    /// Who is in so far (seat order, names) and how many seats the game
    /// has; sent to everyone in the lobby whenever it changes.
    Lobby {
        code: String,
        players: Vec<String>,
        capacity: u8,
    },
    /// Every seat is taken — the match begins. You are `seat` on side
    /// `team`; `players` lists every seat's name in seat order. Sides
    /// deploy South, North, East, West in team order.
    GameStart {
        seat: u8,
        #[serde(default)]
        team: u8,
        #[serde(default)]
        players: Vec<String>,
        board: Board,
        /// The host's setup (scenario name and numbers).
        #[serde(default)]
        setup: Option<GameSetup>,
    },
    /// Your current view of the game (already filtered for you).
    Snapshot {
        phase: Phase,
        turn: u32,
        ships: Vec<ShipView>,
        /// One entry per seat.
        committed: Vec<bool>,
        /// Seat holding the initiative token (breaks pilot-skill ties:
        /// moves first AND fires first at equal skill).
        initiative: u8,
        /// Squad points per seat.
        squad_totals: Vec<u32>,
        /// Side (team) of each seat.
        #[serde(default)]
        teams: Vec<u8>,
        /// Bomb and mine tokens on the board (public).
        #[serde(default)]
        bombs: Vec<BombToken>,
        /// Asteroid and debris tokens (fixed for the game).
        #[serde(default)]
        obstacles: Vec<Obstacle>,
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
        /// Ships dragged by black holes after all moves.
        #[serde(default)]
        pulls: Vec<Pull>,
        /// Bombs that went off at the end of the Activation phase.
        #[serde(default)]
        detonations: Vec<Detonation>,
        events: Vec<String>,
    },
    /// One attack resolved in the Combat phase.
    AttackResult {
        attack: AttackRecord,
        /// Narrated side effects since the previous message.
        events: Vec<String>,
    },
    /// Your ship has several eligible (weapon, target) combinations:
    /// answer with DeclareTarget.
    ChooseTarget {
        attacker: ShipId,
        options: Vec<AttackChoice>,
        /// Equipped weapons that cannot fire, as (name, reason).
        #[serde(default)]
        unavailable: Vec<(String, String)>,
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
        /// Winning side (team index); None when you resigned.
        winner: Option<u8>,
        reason: String,
    },
    Error {
        message: String,
    },
    Pong,
}
