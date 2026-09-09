//! A bot seat: a virtual client driven by `sf_core::bot`. It receives
//! the same `ServerMsg`s a human's socket would and answers through the
//! session's command channel, so the session never knows the difference.

use std::sync::Arc;

use sf_core::board::Board;
use sf_core::bot;
use sf_core::data::Content;
use sf_core::game::{Phase, ShipView};
use sf_core::ship::ShipId;
use sf_proto::messages::{ClientMsg, ServerMsg};
use tokio::sync::mpsc;

use crate::SessionCmd;

/// Name shown in the lobby for bot number `n` (1-based).
pub(crate) fn name(n: usize) -> String {
    format!("Bot {n}")
}

pub(crate) async fn run(
    mut rx: mpsc::Receiver<ServerMsg>,
    tx: mpsc::Sender<SessionCmd>,
    seat: u8,
    content: Arc<Content>,
) {
    let send = |msg: ClientMsg| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(SessionCmd::Msg { seat, msg }).await;
        }
    };
    let mut team = seat;
    let mut board: Option<Board> = None;
    let mut placed_turn: Option<u32> = None;
    let mut planned_turn: Option<u32> = None;
    let mut fallback_turn: Option<u32> = None;
    let mut last_ships: Vec<ShipView> = Vec::new();
    let mut current_turn = 0;
    while let Some(msg) = rx.recv().await {
        match msg {
            ServerMsg::GameStart { team: t, board: b, .. } => {
                team = t;
                board = Some(b);
            }
            ServerMsg::Snapshot {
                phase,
                turn,
                ships,
                committed,
                obstacles,
                zones,
                mission,
                ..
            } => {
                let Some(board) = board.as_ref() else { continue };
                current_turn = turn;
                let view = bot::View {
                    content: &content,
                    board,
                    ships: &ships,
                    obstacles: &obstacles,
                    zones: &zones,
                    mission: mission.as_ref(),
                    turn,
                    seat,
                    team,
                };
                match phase {
                    Phase::Placement if placed_turn != Some(turn) => {
                        placed_turn = Some(turn);
                        for (ship_id, pose) in bot::placements(&view) {
                            send(ClientMsg::PlaceShip { ship_id, pose }).await;
                        }
                    }
                    Phase::Planning
                        if planned_turn != Some(turn)
                            && !committed.get(seat as usize).copied().unwrap_or(true) =>
                    {
                        planned_turn = Some(turn);
                        for p in bot::plans(&view) {
                            send(ClientMsg::PlanManeuver {
                                ship_id: p.ship,
                                maneuver_index: p.maneuver,
                            })
                            .await;
                            send(ClientMsg::PlanAction { ship_id: p.ship, action: p.action }).await;
                        }
                        send(ClientMsg::CommitPlans).await;
                    }
                    _ => {}
                }
                last_ships = ships;
            }
            ServerMsg::ChooseTarget { options, .. } => {
                let opts: Vec<(ShipId, Option<_>, u8)> =
                    options.iter().map(|o| (o.target, o.weapon, o.range)).collect();
                if let Some((target, weapon)) = bot::choose_target(&last_ships, &opts) {
                    send(ClientMsg::DeclareTarget { target, weapon }).await;
                }
            }
            // A refused plan (a rule the bot did not model): fall back to
            // the slowest safe maneuver for every ship, once per turn.
            ServerMsg::Rejected { .. } if fallback_turn != Some(current_turn) => {
                fallback_turn = Some(current_turn);
                for v in last_ships.iter().filter(|v| v.owner.0 == u32::from(seat) && !v.destroyed)
                {
                    if let Some(idx) = bot::safe_maneuver(&content, v) {
                        send(ClientMsg::PlanManeuver { ship_id: v.id, maneuver_index: idx }).await;
                    }
                }
                send(ClientMsg::CommitPlans).await;
            }
            ServerMsg::GameOver { .. } => break,
            _ => {}
        }
    }
}
