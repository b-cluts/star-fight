//! Star Fight server: WebSocket listener, lobby with join codes, and one
//! task per game session owning the authoritative GameState.
//!
//! M4: TLS with a pinned self-signed certificate (see `tls`), a server
//! password checked in constant time, and rate-limited join attempts.
//! Plaintext is still available for tests and local development
//! (`ServerOpts::insecure`).

pub mod tls;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use rand::Rng;
use rand::distributions::Alphanumeric;
use subtle::ConstantTimeEq;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::Message;

use sf_core::board::Board;
use sf_core::data::Content;
use sf_core::game::{CombatStep, GameState, Phase};
use sf_core::pilot::PilotId;
use sf_core::ship::{PlayerId, ShipClassId};
use sf_core::squad::{Squad, SquadRules, validate_squad};
use sf_proto::PROTOCOL_VERSION;
use sf_proto::codec::{decode, encode};
use sf_proto::messages::{ClientMsg, ServerMsg};

/// Sandbox-era fixed fleets: seat 0 flies two TIE/ln, seat 1 two T-70s,
/// each with the class's basic pilot. (Squad selection becomes part of
/// the lobby once the squad builder exists.)
const FLEET_SOUTH: [ShipClassId; 2] = [ShipClassId(1), ShipClassId(1)];
const FLEET_NORTH: [ShipClassId; 2] = [ShipClassId(2), ShipClassId(2)];

fn basic_fleet(content: &Content, classes: &[ShipClassId]) -> Vec<PilotId> {
    classes.iter().map(|k| content.pilots.basic_for(*k).expect("basic pilot").id).collect()
}

fn default_board() -> Board {
    Board { width: 20.0, height: 20.0, deploy_depth: 3.0 }
}

type Lobby = Arc<Mutex<HashMap<String, mpsc::Sender<SessionCmd>>>>;

/// Failed password attempts allowed per client address within
/// [`RATE_WINDOW`] before further Hellos are refused outright.
pub const RATE_LIMIT: u32 = 5;
pub const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Transport and admission settings.
#[derive(Clone)]
pub struct ServerOpts {
    /// TLS identity; `None` accepts plaintext `ws://` (tests / dev only).
    pub tls: Option<Arc<rustls::ServerConfig>>,
    /// Required in every Hello; `None` disables the check.
    pub password: Option<String>,
}

impl ServerOpts {
    /// Plaintext, no password — for tests and local development.
    pub fn insecure() -> Self {
        Self { tls: None, password: None }
    }
}

/// Shared admission state: failed-password counters per address.
#[derive(Default)]
struct Admission {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl Admission {
    async fn blocked(&self, ip: IpAddr) -> bool {
        let mut map = self.failures.lock().await;
        match map.get(&ip) {
            Some((n, since)) if since.elapsed() < RATE_WINDOW => *n >= RATE_LIMIT,
            Some(_) => {
                map.remove(&ip);
                false
            }
            None => false,
        }
    }

    async fn failed(&self, ip: IpAddr) {
        let mut map = self.failures.lock().await;
        let e = map.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= RATE_WINDOW {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }

    async fn succeeded(&self, ip: IpAddr) {
        self.failures.lock().await.remove(&ip);
    }
}

/// Constant-time password check (no early exit on the first differing byte).
fn password_ok(expected: &str, given: &str) -> bool {
    expected.len() == given.len() && bool::from(expected.as_bytes().ct_eq(given.as_bytes()))
}

enum SessionCmd {
    Join {
        name: String,
        squad: Option<Squad>,
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
pub async fn run(listener: TcpListener, content: Arc<Content>, opts: ServerOpts) {
    let lobby: Lobby = Arc::new(Mutex::new(HashMap::new()));
    let admission = Arc::new(Admission::default());
    let acceptor = opts.tls.clone().map(TlsAcceptor::from);
    loop {
        let Ok((stream, addr)) = listener.accept().await else {
            continue;
        };
        println!("connection from {addr}");
        let ctx = Conn {
            lobby: lobby.clone(),
            content: content.clone(),
            admission: admission.clone(),
            password: opts.password.clone(),
            ip: addr.ip(),
        };
        match &acceptor {
            Some(acceptor) => {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    match acceptor.accept(stream).await {
                        Ok(tls) => handle_conn(tls, ctx).await,
                        Err(e) => println!("TLS handshake with {addr} failed: {e}"),
                    }
                });
            }
            None => {
                tokio::spawn(handle_conn(stream, ctx));
            }
        }
    }
}

/// Per-connection context handed to `handle_conn`.
struct Conn {
    lobby: Lobby,
    content: Arc<Content>,
    admission: Arc<Admission>,
    password: Option<String>,
    ip: IpAddr,
}

fn rand_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(|c| (c as char).to_ascii_uppercase())
        .collect()
}

async fn handle_conn<S>(stream: S, ctx: Conn)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let Conn { lobby, content, admission, password, ip } = ctx;
    let Ok(ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    let (mut tx, mut rx) = ws.split();

    // 1. Handshake.
    let name = loop {
        match rx.next().await {
            Some(Ok(Message::Text(t))) => match decode::<ClientMsg>(&t) {
                Ok(ClientMsg::Hello { proto_version, name, password: given }) => {
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
                    if let Some(expected) = &password {
                        if admission.blocked(ip).await {
                            let _ = tx
                                .send(Message::Text(encode(&ServerMsg::Error {
                                    message: "too many failed password attempts — try again later"
                                        .into(),
                                })))
                                .await;
                            return;
                        }
                        if !password_ok(expected, &given) {
                            admission.failed(ip).await;
                            let _ = tx
                                .send(Message::Text(encode(&ServerMsg::Error {
                                    message: "wrong server password".into(),
                                })))
                                .await;
                            return;
                        }
                        admission.succeeded(ip).await;
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
                Ok(ClientMsg::CreateGame { squad }) => {
                    let code = rand_string(4);
                    let (cmd_tx, cmd_rx) = mpsc::channel(64);
                    tokio::spawn(session(cmd_rx, content.clone(), lobby.clone(), code.clone()));
                    lobby.lock().await.insert(code.clone(), cmd_tx.clone());
                    let (resp_tx, resp_rx) = oneshot::channel();
                    let _ = cmd_tx
                        .send(SessionCmd::Join { name: name.clone(), squad, resp: resp_tx })
                        .await;
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
                Ok(ClientMsg::JoinGame { code, squad }) => {
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
                    let _ = cmd_tx
                        .send(SessionCmd::Join { name: name.clone(), squad, resp: resp_tx })
                        .await;
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
    let mut players: Vec<(String, mpsc::Sender<ServerMsg>, Squad)> = Vec::new();
    let rules = SquadRules::default();
    let mut game: Option<GameState> = None;

    // Combat streaming: how many narrated events have gone out this turn,
    // and whether the game just ended (macros can't `break` the loop).
    let mut streamed = 0usize;
    let mut game_over = false;
    macro_rules! send_to {
        ($seat:expr, $msg:expr) => {
            if let Some((_, tx, _)) = players.get($seat as usize) {
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
                        ships: gs.snapshot_for(&content, PlayerId(s as u32)),
                        committed: gs.committed,
                        initiative: gs.initiative.0 as u8,
                        squad_totals: gs.squad_totals,
                    }
                );
            }
        };
    }

    /// Advance combat, streaming each attack; stop at a Declare Target
    /// prompt (sent to its owner) or at the end of the turn.
    macro_rules! drive_combat {
        ($gs:expr) => {
            loop {
                match $gs.combat_step(&content, &mut || rand::random::<u8>()) {
                    Ok(CombatStep::Attack(rec)) => {
                        let all = $gs.combat_events();
                        let events = all[streamed.min(all.len())..].to_vec();
                        streamed = all.len();
                        for s in 0..players.len() as u8 {
                            send_to!(
                                s,
                                ServerMsg::AttackResult {
                                    attack: rec.clone(),
                                    events: events.clone()
                                }
                            );
                        }
                    }
                    Ok(CombatStep::NeedTarget(p)) => {
                        let owner = p.owner.0 as u8;
                        let candidates: Vec<(sf_core::ship::ShipId, u8)> =
                            p.candidates.iter().map(|(id, r, _)| (*id, *r)).collect();
                        send_to!(
                            owner,
                            ServerMsg::ChooseTarget { attacker: p.attacker, candidates }
                        );
                        send_to!(1 - owner, ServerMsg::OpponentChoosing { attacker: p.attacker });
                        break;
                    }
                    Ok(CombatStep::Done(rec)) => {
                        let events = rec.events[streamed.min(rec.events.len())..].to_vec();
                        streamed = 0;
                        for s in 0..players.len() as u8 {
                            send_to!(s, ServerMsg::TurnEnd { events: events.clone() });
                        }
                        snapshots!(&*$gs);
                        if $gs.phase == Phase::GameOver {
                            let winner = $gs.winner.map(|p| p.0 as u8);
                            for s in 0..players.len() as u8 {
                                send_to!(
                                    s,
                                    ServerMsg::GameOver {
                                        winner,
                                        reason: "fleet destroyed".into()
                                    }
                                );
                            }
                            game_over = true;
                        }
                        break;
                    }
                    Err(e) => {
                        for s in 0..players.len() as u8 {
                            send_to!(s, ServerMsg::Error { message: format!("combat error: {e}") });
                        }
                        break;
                    }
                }
            }
        };
    }

    while let Some(cmd) = cmds.recv().await {
        match cmd {
            SessionCmd::Join { name, squad, resp } => {
                if players.len() >= 2 {
                    let _ = resp.send(Err("game is full".into()));
                    continue;
                }
                let seat = players.len() as u8;
                let squad = squad.unwrap_or_else(|| {
                    let classes = if seat == 0 { &FLEET_SOUTH } else { &FLEET_NORTH };
                    Squad::basic(&content, "basic", &basic_fleet(&content, classes))
                });
                if let Err(errors) = validate_squad(&squad, &content, &rules) {
                    let msg: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
                    let _ = resp.send(Err(format!("squad rejected: {}", msg.join("; "))));
                    continue;
                }
                let (tx, rx) = mpsc::channel(64);
                players.push((name, tx, squad));
                let _ = resp.send(Ok((seat, rx)));
                if players.len() == 2 {
                    // One red die, drawn now — only used if squad totals tie.
                    let tie_roll = sf_core::dice::AttackFace::from_d8(rand::random::<u8>());
                    let gs = GameState::from_squads(
                        default_board(),
                        &content,
                        [&players[0].2, &players[1].2],
                        tie_roll,
                    )
                    .expect("validated squads");
                    for s in 0..2u8 {
                        let opponent = players[1 - s as usize].0.clone();
                        send_to!(s, ServerMsg::GameStart { seat: s, opponent, board: gs.board });
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
                    ClientMsg::Rename { ship_id, callsign } => {
                        match gs.rename(player, ship_id, &callsign) {
                            Ok(()) => snapshots!(&*gs),
                            Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                        }
                    }
                    ClientMsg::PlanAction { ship_id, action } => {
                        match gs.plan_action(&content, player, ship_id, action) {
                            // Plans are secret: only the planner's view changes.
                            Ok(()) => send_to!(
                                seat,
                                ServerMsg::Snapshot {
                                    phase: gs.phase,
                                    turn: gs.turn,
                                    ships: gs.snapshot_for(&content, player),
                                    committed: gs.committed,
                                    initiative: gs.initiative.0 as u8,
                                    squad_totals: gs.squad_totals,
                                }
                            ),
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
                                    ships: gs.snapshot_for(&content, player),
                                    committed: gs.committed,
                                    initiative: gs.initiative.0 as u8,
                                    squad_totals: gs.squad_totals,
                                }
                            ),
                            Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                        }
                    }
                    ClientMsg::CommitPlans => {
                        match gs.commit_plans_begin(&content, player, &mut || rand::random::<u8>())
                        {
                            Ok(None) => snapshots!(&*gs),
                            Ok(Some(act)) => {
                                let (moves, events) = (act.moves, act.events);
                                streamed = events.len();
                                for s in 0..players.len() as u8 {
                                    send_to!(
                                        s,
                                        ServerMsg::MovementResult {
                                            moves: moves.clone(),
                                            events: events.clone(),
                                        }
                                    );
                                }
                                drive_combat!(gs);
                                if game_over {
                                    break;
                                }
                            }
                            Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                        }
                    }
                    ClientMsg::DeclareTarget { target } => {
                        match gs
                            .declare_target(&content, player, target, &mut || rand::random::<u8>())
                        {
                            Ok(rec) => {
                                let all = gs.combat_events();
                                let events = all[streamed.min(all.len())..].to_vec();
                                streamed = all.len();
                                for s in 0..players.len() as u8 {
                                    send_to!(
                                        s,
                                        ServerMsg::AttackResult {
                                            attack: rec.clone(),
                                            events: events.clone(),
                                        }
                                    );
                                }
                                drive_combat!(gs);
                                if game_over {
                                    break;
                                }
                            }
                            Err(e) => send_to!(seat, ServerMsg::Rejected { reason: e.to_string() }),
                        }
                    }
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
                    send_to!(
                        winner,
                        ServerMsg::GameOver {
                            winner: Some(winner),
                            reason: "opponent disconnected".into(),
                        }
                    );
                }
                break;
            }
        }
    }
    lobby.lock().await.remove(&code);
}
