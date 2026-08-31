//! End-to-end: two WebSocket clients create/join a game, place fleets,
//! secretly plan, commit, and receive the same resolved turn.

use std::f64::consts::FRAC_PI_2;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

use sf_core::data::Content;
use sf_core::game::Phase;
use sf_core::geometry::Pose;
use sf_core::maneuver::Steer;
use sf_core::ship::{ShipClassId, ShipId};
use sf_proto::codec::{decode, encode};
use sf_proto::messages::{ClientMsg, ServerMsg};

type Ws = tokio_tungstenite::WebSocketStream<
    tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
>;

fn content() -> Content {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/data");
    Content::from_ron(
        &std::fs::read_to_string(format!("{dir}/ships.ron")).unwrap(),
        &std::fs::read_to_string(format!("{dir}/maneuvers.ron")).unwrap(),
    )
    .unwrap()
}

async fn send(ws: &mut Ws, msg: &ClientMsg) {
    ws.send(Message::Text(encode(msg))).await.unwrap();
}

/// Next ServerMsg, skipping nothing — 5s guard against hangs.
async fn recv(ws: &mut Ws) -> ServerMsg {
    let frame = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("timed out waiting for server message")
        .expect("stream ended")
        .expect("ws error");
    match frame {
        Message::Text(t) => decode(&t).expect("decodable ServerMsg"),
        other => panic!("unexpected frame {other:?}"),
    }
}

/// Receive until the predicate matches, discarding earlier messages.
async fn recv_until<T>(ws: &mut Ws, mut pick: impl FnMut(ServerMsg) -> Option<T>) -> T {
    for _ in 0..20 {
        if let Some(v) = pick(recv(ws).await) {
            return v;
        }
    }
    panic!("expected message never arrived");
}

fn dial_index(c: &Content, class: ShipClassId, steer: Steer, dist: u8) -> u8 {
    let set = c.ships.class(class).unwrap().maneuver_set;
    c.dials
        .set(set)
        .unwrap()
        .maneuvers
        .iter()
        .position(|m| m.steer == steer && m.distance == dist)
        .unwrap() as u8
}

#[tokio::test]
async fn two_clients_play_a_full_turn() {
    let c = content();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(sf_server::run(listener, Arc::new(content())));

    let url = format!("ws://127.0.0.1:{port}");
    let (mut a, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut b, _) = tokio_tungstenite::connect_async(&url).await.unwrap();

    // Handshakes.
    for (ws, name) in [(&mut a, "alice"), (&mut b, "bob")] {
        send(
            ws,
            &ClientMsg::Hello {
                proto_version: sf_proto::PROTOCOL_VERSION,
                name: name.into(),
                password: String::new(),
            },
        )
        .await;
        assert!(matches!(recv(ws).await, ServerMsg::Welcome { .. }));
    }

    // Create + join.
    send(&mut a, &ClientMsg::CreateGame).await;
    let code = recv_until(&mut a, |m| match m {
        ServerMsg::GameCreated { code } => Some(code),
        _ => None,
    })
    .await;
    send(&mut b, &ClientMsg::JoinGame { code }).await;

    let seat_a = recv_until(&mut a, |m| match m {
        ServerMsg::GameStart { seat, opponent, .. } => {
            assert_eq!(opponent, "bob");
            Some(seat)
        }
        _ => None,
    })
    .await;
    let seat_b = recv_until(&mut b, |m| match m {
        ServerMsg::GameStart { seat, .. } => Some(seat),
        _ => None,
    })
    .await;
    assert_eq!((seat_a, seat_b), (0, 1));

    // Initial snapshots: 4 ships, placement phase.
    let ships = recv_until(&mut a, |m| match m {
        ServerMsg::Snapshot { phase: Phase::Placement, ships, .. } => Some(ships),
        _ => None,
    })
    .await;
    assert_eq!(ships.len(), 4);
    recv_until(&mut b, |m| matches!(m, ServerMsg::Snapshot { .. }).then_some(())).await;

    // Illegal placement rejected (wrong zone).
    send(
        &mut a,
        &ClientMsg::PlaceShip { ship_id: ShipId(0), pose: Pose::new(10.0, 10.0, FRAC_PI_2) },
    )
    .await;
    recv_until(&mut a, |m| matches!(m, ServerMsg::Rejected { .. }).then_some(())).await;

    // Legal placements: A south (ships 0,1), B north (ships 2,3).
    for (ws, placements) in [
        (&mut a, [(0u32, 8.0, 2.0, FRAC_PI_2), (1, 12.0, 2.0, FRAC_PI_2)]),
        (&mut b, [(2, 8.0, 18.0, -FRAC_PI_2), (3, 12.0, 18.0, -FRAC_PI_2)]),
    ] {
        for (id, x, y, h) in placements {
            send(ws, &ClientMsg::PlaceShip { ship_id: ShipId(id), pose: Pose::new(x, y, h) })
                .await;
        }
    }

    // Both should reach Planning; B's view must now include A's poses.
    let ships_b = recv_until(&mut b, |m| match m {
        ServerMsg::Snapshot { phase: Phase::Planning, ships, .. } => Some(ships),
        _ => None,
    })
    .await;
    assert!(ships_b.iter().all(|s| s.pose.is_some()));
    assert!(ships_b.iter().all(|s| s.plan.is_none() || s.owner.0 == 1));
    recv_until(&mut a, |m| match m {
        ServerMsg::Snapshot { phase: Phase::Planning, .. } => Some(()),
        _ => None,
    })
    .await;

    // Secret plans: everyone straight-2 (blue on both dials).
    let tie_s2 = dial_index(&c, ShipClassId(1), Steer::Straight, 2);
    let xw_s2 = dial_index(&c, ShipClassId(2), Steer::Straight, 2);
    for id in [0u32, 1] {
        send(&mut a, &ClientMsg::PlanManeuver { ship_id: ShipId(id), maneuver_index: tie_s2 })
            .await;
    }
    for id in [2u32, 3] {
        send(&mut b, &ClientMsg::PlanManeuver { ship_id: ShipId(id), maneuver_index: xw_s2 })
            .await;
    }
    send(&mut a, &ClientMsg::CommitPlans).await;
    send(&mut b, &ClientMsg::CommitPlans).await;

    // Both clients get the identical resolved turn.
    let moves_a = recv_until(&mut a, |m| match m {
        ServerMsg::TurnResult { moves, .. } => Some(moves),
        _ => None,
    })
    .await;
    let moves_b = recv_until(&mut b, |m| match m {
        ServerMsg::TurnResult { moves, .. } => Some(moves),
        _ => None,
    })
    .await;
    assert_eq!(moves_a, moves_b);

    // Movement order: both TIEs (skill 1) before both X-Wings (skill 2).
    let order: Vec<u32> = moves_a.iter().map(|m| m.ship.0).collect();
    assert_eq!(order, vec![0, 1, 2, 3]);
    // Straight-2: TIEs advance from y=2 to 4; X-Wings from 18 to 16.
    assert!((moves_a[0].end.anchor.y - 4.0).abs() < 1e-9);
    assert!((moves_a[2].end.anchor.y - 16.0).abs() < 1e-9);
    assert!(moves_a.iter().all(|m| !m.bumped && !m.destroyed));

    // Resign ends it for both.
    send(&mut a, &ClientMsg::Resign).await;
    let w = recv_until(&mut b, |m| match m {
        ServerMsg::GameOver { winner, .. } => Some(winner),
        _ => None,
    })
    .await;
    assert_eq!(w, Some(1));
}
