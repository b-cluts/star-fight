//! Star Fight server: WebSocket listener, lobby with join codes, and one
//! task per game session owning the authoritative GameState.
//!
//! M3: plaintext WebSocket on localhost. M4 adds TLS with the pinned
//! self-signed certificate and the server password check.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use rand::distributions::Alphanumeric;
use rand::Rng;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_tungstenite::tungstenite::Message;

use sf_core::board::Board;
use sf_core::data::Content;
use sf_core::game::{GameState, Phase};
use sf_core::ship::{PlayerId, ShipClassId};
use sf_proto::codec::{decode, encode};
use sf_proto::messages::{ClientMsg, ServerMsg};
use sf_proto::PROTOCOL_VERSION;

/// Sandbox-era fixed fleets: seat 0 flies two TIEs, seat 1 two X-Wings.
/// (Fleet selection becomes part of the lobby later.)
const FLEET_SOUTH: [ShipClassId; 2] = [ShipClassId(1), ShipClassId(1)];
const FLEET_NORTH: [ShipClassId; 2] = [ShipClassId(2), ShipClassId(2)];

fn default_board() -> Board {
    Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
}

type Lobby = Arc<Mutex<HashMap<String, mpsc::Sender<SessionCmd>>>>;

enum SessionCmd {
    Join {
        name: String,
        resp: oneshot::Sender<Result<(u8, mpsc::Receiver<ServerMsg>), String>>,
    },
    Msg {
        seat: u8,
        msg: ClientMsg,
    },
    Disconnect {
        seat: u8,
    },
}

/// Accept connections forever. Callers bind the listener (tests use an
/// ephemeral port).
pub async fn run(listener: TcpListener, content: Arc<Content>) {
    let lobby: Lobby = Arc::new(Mutex::new(HashMap::new()));
    loop {
        let Ok((stream, addr)) = listener.accept().await else { continue };
        println!("connection from {addr}");
        tokio::spawn(handle_conn(stream, lobby.clone(), content.clone()));
    }
}

fn rand_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(|c| (c as char).to_ascii_uppercase())
        .collect()
}

async fn handle_conn(stream: TcpStream, lobby: Lobby, content: Arc<Content>) {
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else { return };
    let (mut tx, mut rx) = ws.split();

    // 1. Handshake.
    let name = loop {
        match rx.next().await {
            Some(Ok(Message::Text(t))) => match decode::<ClientMsg>(&t) {
                Ok(ClientMsg::Hello { proto_version, name, password: _ }) => {
                    if proto_version != PROTOCOL_VERSION {
                        let _ = tx
                            .send(Message::Text(encode(&ServerMsg::Error {
                                message: format!(
                                    "protocol {proto_version} unsupported (server: {PROTOCOL_VERSION}) — please update"
                                ),
                            })))
                            .await;
                        return;
                    }
                    let _ = tx
                        .send(Message::Text(encode(&ServerMsg::Welcome {
                            reconnect_token: rand_string(16),
                        })))
                        .await;
                    break name;
                }
                Ok(_) => {
                    let _ = tx
                        .send(Message::Text(encode(&ServerMsg::Error {
                            message: "expected Hello first".into(),
                        })))
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(Message::Text(encode(&ServerMsg::Error {
                            message: format!("bad message: {e}"),
                        })))
                        .await;
                }
            },
            Some(Ok(Message::Close(_))) | None => return,
            _ => {}
        }
    };
    // 2. Create or join a game.
    let (session_tx, seat, mut session_rx) = loop {
        match rx.next().await {
            Some(Ok(Message::Text(t))) => match decode::<ClientMsg>(&t) {
                Ok(ClientMsg::CreateGame) => {
                    let code = rand_string(4);
                    let (cmd_tx, cmd_rx) = mpsc::channel(64);
                    tokio::spawn(session(cmd_rx, content.clone(), lobby.clone(), code.clone()));
                    lobby.lock().await.insert(code.clone(), cmd_tx.clone());
                    let (resp_tx, resp_rx) = oneshot::channel();
                    let _ = cmd_tx.send(SessionCmd::Join { name: name.clone(), resp: resp_tx }).await;
                    match resp_rx.await {
                        Ok(Ok((seat, rx_srv))) => {
                            let _ = tx
                                .send(Message::Text(encode(&ServerMsg::GameCreated {
                                    code: code.clone(),
                                })))
                                .await;
                            break (cmd_tx, seat, rx_srv);
                        }
                        _ => {
                            let _ = tx
                                .send(Message::Text(encode(&ServerMsg::Error {
                                    message: "could not create game".into(),
                                })))
                                .await;
                            return;
                        }
                    }
                }
                Ok(ClientMsg::JoinGame { code }) => {
                    let entry = lobby.lock().await.get(&code.to_ascii_uppercase()).cloned();
                    let Some(cmd_tx) = entry else {
                        let _ = tx
                            .send(Message::Text(encode(&ServerMsg::Error {
                                message: format!("no open game with code {code}"),
                            })))
                            .await;
                        continue;
                    };
                    let (resp_tx, resp_rx) = oneshot::channel();
                    let _ = cmd_tx.send(SessionCmd::Join { name: name.clone(), resp: resp_tx }).await;
                    match resp_rx.await {
                        Ok(Ok((seat, rx_srv))) => {
                            // Game now full: no more joins under this code.
                            lobby.lock().await.remove(&code.to_ascii_uppercase());
                            break (cmd_tx, seat, rx_srv);
                        }
                        Ok(Err(e)) => {
                            let _ = tx
                                .send(Message::Text(encode(&ServerMsg::Error { message: e })))
                                .await;
                        }
                        Err(_) => return,
                    }
                }
                Ok(ClientMsg::Ping) => {
                    let _ = tx.send(Message::Text(encode(&ServerMsg::Pong))).await;
                }
                Ok(_) => {
                    let _ = tx
                        .send(Message::Text(encode(&ServerMsg::Error {
                            message: "create or join a game first".into(),
                        })))
                        .await;
                }
                Err(e) => {
                    let _ = tx
                        .send(Message::Text(encode(&ServerMsg::Error {
                            message: format!("bad message: {e}"),
                        })))
                        .await;
                }
            },
            Some(Ok(Message::Close(_))) | None => return,
            _ => {}
        }
    };

    // 3. Relay: socket -> session, session -> socket.
    loop {
        tokio::select! {
            inbound = rx.next() => match inbound {
                Some(Ok(Message::Text(t))) => match decode::<ClientMsg>(&t) {
                    Ok(ClientMsg::Ping) => {
                        let _ = tx.send(Message::Text(encode(&ServerMsg::Pong))).await;
                    }
                    Ok(msg) => {
                        if session_tx.send(SessionCmd::Msg { seat, msg }).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Message::Text(encode(&ServerMsg::Error {
                                message: format!("bad message: {e}"),
                            })))
                            .await;
                    }
                },
                Some(Ok(Message::Close(_))) | None => {
                    let _ = session_tx.send(SessionCmd::Disconnect { seat }).await;
                    return;
                }
                _ => {}
            },
            outbound = session_rx.recv() => match outbound {
                Some(m) => {
                    if tx.send(Message::Text(encode(&m))).await.is_err() {
                        let _ = session_tx.send(SessionCmd::Disconnect { seat }).await;
                        return;
                    }
                }
                None => return, // session ended
            },
        }
    }
}

/// One game: owns the GameState; all commands arrive on one channel, so
/// the rules run free of locks.
async fn session(
    mut cmds: mpsc::Receiver<SessionCmd>,
    content: Arc<Content>,
    lobby: Lobby,
    code: String,
) {
    let mut players: Vec<(String, mpsc::Sender<ServerMsg>)> = Vec::new();
    let mut game: Option<GameState> = None;

    macro_rules! send_to {
        ($seat:expr, $msg:expr) => {
            if let Some((_, tx)) = players.get($seat as usize) {
                let _ = tx.send($msg).await;
            }
        };
    }
    macro_rules! snapshots {
        ($gs:expr) => {
            for s in 0..players.len() as u8 {
                let gs: &GameState = $gs;
                send_to!(
                    s,
                    ServerMsg::Snapshot {
                        phase: gs.phase,
                        turn: gs.turn,
                        ships: gs.snapshot_for(PlayerId(s as u32)),
                        committed: gs.committed,
                        initiative: gs.initiative.0 as u8,
                        squad_totals: gs.squad_totals,
                    }
                );
            }
        };
    }

    while let Some(cmd) = cmds.recv().await {
        match cmd {
            SessionCmd::Join { name, resp } => {
                if players.len() >= 2 {
                    let _ = resp.send(Err("game is full".into()));
                    continue;
                }
                let (tx, rx) = mpsc::channel(64);
                players.push((name, tx));
                let seat = players.len() as u8 - 1;
                let _ = resp.send(Ok((seat, rx)));
                if players.len() == 2 {
                    // One red die, drawn now — only used if squad totals tie.
                    let tie_roll = sf_core::dice::AttackFace::from_d8(rand::random::<u8>());
                    let gs = GameState::new(
                        default_board(),
                        &content,
                        [&FLEET_SOUTH, &FLEET_NORTH],
                        tie_roll,
                    )
                    .expect("valid content");
                    for s in 0..2u8 {
                        let opponent = players[1 - s as usize].0.clone();
                        send_to!(s, ServerMsg::GameStart { seat: s, opponent });
                    }
                    snapshots!(&gs);
                    game = Some(gs);
                }
            }
            SessionCmd::Msg { seat, msg } => {
                let Some(gs) = game.as_mut() else {
                    send_to!(seat, ServerMsg::Error { message: "waiting for opponent".into() });
                    continue;
                };
                let player = PlayerId(seat as u32);
                match msg {
                    ClientMsg::PlaceShip { ship_id, pose } => {
                        match gs.place_ship(&content, player, ship_id, pose) {
                            Ok(()) => snapshots!(&*gs),
                            Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                        }
                    }
                    ClientMsg::PlanManeuver { ship_id, maneuver_index } => {
                        match gs.plan_maneuver(&content, player, ship_id, maneuver_index) {
                            // Plans are secret: only the planner's view changes.
                            Ok(()) => send_to!(
                                seat,
                                ServerMsg::Snapshot {
                                    phase: gs.phase,
                                    turn: gs.turn,
                                    ships: gs.snapshot_for(player),
                                    committed: gs.committed,
                                    initiative: gs.initiative.0 as u8,
                                    squad_totals: gs.squad_totals,
                                }
                            ),
                            Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                        }
                    }
                    ClientMsg::CommitPlans => match gs.commit_plans(&content, player) {
                        Ok(None) => snapshots!(&*gs),
                        Ok(Some(moves)) => {
                            for s in 0..players.len() as u8 {
                                send_to!(s, ServerMsg::TurnResult { moves: moves.clone() });
                            }
                            snapshots!(&*gs);
                            if gs.phase == Phase::GameOver {
                                let winner = gs.winner.map(|p| p.0 as u8);
                                for s in 0..players.len() as u8 {
                                    send_to!(
                                        s,
                                        ServerMsg::GameOver {
                                            winner,
                                            reason: "fleet destroyed".into(),
                                        }
                                    );
                                }
                                break;
                            }
                        }
                        Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                    },
                    ClientMsg::Resign => {
                        let winner = gs.resign(player);
                        for s in 0..players.len() as u8 {
                            send_to!(
                                s,
                                ServerMsg::GameOver {
                                    winner: Some(winner.0 as u8),
                                    reason: "resignation".into(),
                                }
                            );
                        }
                        break;
                    }
                    _ => send_to!(seat, ServerMsg::Error { message: "unexpected message".into() }),
                }
            }
            SessionCmd::Disconnect { seat } => {
                if game.is_some() {
                    let winner = 1 - seat;
                    send_to!(winner, ServerMsg::GameOver {
                        winner: Some(winner),
                        reason: "opponent disconnected".into(),
                    });
                }
                break;
            }
        }
    }
    lobby.lock().await.remove(&code);
}
