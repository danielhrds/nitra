//! Against a running room server (`npm run dev` in worker/, or
//! `NITRA_ROOM_SERVER=ws://host:port`): `cargo test -p rooms --test live --
//! --ignored --test-threads=1`.

use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use identity::Identity;
use rooms::protocol::new_room_id;
use rooms::ws::{WebSocket, WsUrl};
use rooms::{Command, Join, RoomClient, RoomConfig, RoomEvent, ScreenInfo, ScreenKind};

fn server() -> String {
    std::env::var("NITRA_ROOM_SERVER").unwrap_or_else(|_| "ws://127.0.0.1:8787".into())
}

fn start(
    room: &str,
    name: &str,
    join: Join,
    identity: &Arc<Identity>,
) -> (RoomClient, Receiver<RoomEvent>) {
    RoomClient::start(
        RoomConfig {
            server: server(),
            room: room.into(),
            name: name.into(),
            join,
        },
        Arc::clone(identity),
    )
}

/// The next event that `pick` accepts, within `secs`.
fn wait<T>(
    events: &Receiver<RoomEvent>,
    secs: u64,
    mut pick: impl FnMut(RoomEvent) -> Option<T>,
) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let event = events
            .recv_timeout(left)
            .expect("timed out waiting for an event");
        if let Some(value) = pick(event) {
            return value;
        }
    }
}

const SCREEN: fn() -> ScreenInfo = || ScreenInfo {
    kind: ScreenKind::Desktop,
    name: "Monitor 1".into(),
    width: 1920,
    height: 1080,
    fps: 60,
};

#[test]
#[ignore = "needs a running room server"]
fn two_apps_meet_in_a_room_and_exchange_signals() {
    let room = new_room_id().unwrap();
    let (alice_id, bob_id) = (
        Arc::new(Identity::generate().unwrap()),
        Arc::new(Identity::generate().unwrap()),
    );
    let started = Instant::now();
    let (alice, alice_events) = start(
        &room,
        "alice",
        Join::Create {
            name: "Live test".into(),
        },
        &alice_id,
    );
    let welcome = wait(&alice_events, 10, |e| match e {
        RoomEvent::Connected(w) => Some(w),
        _ => None,
    });
    println!("alice signed in after {:?}", started.elapsed());
    assert_eq!(
        welcome.you.key,
        alice_id.public_key().to_string().replace('-', "")
    );
    alice.send(Command::StartScreen(SCREEN())).unwrap();
    alice
        .send(Command::CreateInvite {
            ttl_secs: 3600,
            max_uses: 1,
        })
        .unwrap();
    let invite = wait(&alice_events, 5, |e| match e {
        RoomEvent::InviteCreated { invite, .. } => Some(invite),
        _ => None,
    });
    println!(
        "invite text: {} ({} chars)",
        invite.text(),
        invite.text().len()
    );

    let (bob, bob_events) = start(&room, "bob", Join::Invite(invite.token.clone()), &bob_id);
    let bob_welcome = wait(&bob_events, 10, |e| match e {
        RoomEvent::Connected(w) => Some(w),
        _ => None,
    });
    let alice_live = bob_welcome
        .presence
        .iter()
        .find(|p| p.key == welcome.you.key)
        .expect("alice is online");
    assert_eq!(
        alice_live.screen.as_ref().map(|s| s.info.name.as_str()),
        Some("Monitor 1")
    );
    wait(&alice_events, 5, |e| {
        matches!(e, RoomEvent::MemberJoined(_)).then_some(())
    });

    // An offer-sized envelope there and an answer back.
    let offer = format!(
        "{{\"body\":\"{}\",\"signature\":\"{}\"}}",
        "a".repeat(3000),
        "ab".repeat(64)
    );
    let t0 = Instant::now();
    bob.send(Command::Signal {
        to: *alice_id.public_key(),
        envelope: offer.clone(),
    })
    .unwrap();
    let (from, envelope) = wait(&alice_events, 5, |e| match e {
        RoomEvent::Signal { from, envelope } => Some((from, envelope)),
        _ => None,
    });
    assert_eq!(&from, bob_id.public_key());
    assert_eq!(envelope, offer);
    alice
        .send(Command::Signal {
            to: *bob_id.public_key(),
            envelope: "answer".into(),
        })
        .unwrap();
    let answer = wait(&bob_events, 5, |e| match e {
        RoomEvent::Signal { envelope, .. } => Some(envelope),
        _ => None,
    });
    assert_eq!(answer, "answer");
    println!("offer + answer through the room: {:?}", t0.elapsed());

    // Bob leaves for good: his client ends, alice hears it.
    bob.send(Command::Leave).unwrap();
    let reason = wait(&bob_events, 5, |e| match e {
        RoomEvent::Ended { reason } => Some(reason),
        _ => None,
    });
    println!("bob's client ended: {reason}");
    wait(&alice_events, 5, |e| {
        matches!(e, RoomEvent::MemberLeft(_)).then_some(())
    });
    alice.send(Command::Leave).unwrap();
    wait(&alice_events, 5, |e| {
        matches!(e, RoomEvent::Ended { .. }).then_some(())
    });
}

#[test]
#[ignore = "needs a running room server"]
fn a_stranger_is_refused_for_good() {
    let room = new_room_id().unwrap();
    let owner = Arc::new(Identity::generate().unwrap());
    let (_owner, owner_events) = start(
        &room,
        "owner",
        Join::Create {
            name: "Live test".into(),
        },
        &owner,
    );
    wait(&owner_events, 10, |e| {
        matches!(e, RoomEvent::Connected(_)).then_some(())
    });
    let stranger = Arc::new(Identity::generate().unwrap());
    let (_client, events) = start(&room, "eve", Join::Member, &stranger);
    let reason = wait(&events, 10, |e| match e {
        RoomEvent::Ended { reason } => Some(reason),
        RoomEvent::Disconnected { reason, .. } => panic!("retried a refusal: {reason}"),
        _ => None,
    });
    assert!(reason.contains("4003"), "{reason}");
}

/// Aborting unblocks a receive waiting on another thread (what frees the
/// reader of a dead connection: WinHTTP does not time out WebSocket
/// receives, measured: a receive waited 20 s for the room's own timeout).
#[test]
#[ignore = "needs a running room server"]
fn abort_unblocks_a_waiting_receive() {
    let url = WsUrl::parse(&format!("{}/rooms/{}/ws", server(), new_room_id().unwrap())).unwrap();
    let socket = Arc::new(WebSocket::connect(&url, Duration::from_secs(5)).unwrap());
    println!("first message: {:?}", socket.receive().unwrap());
    let aborter = {
        let socket = Arc::clone(&socket);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(500));
            socket.abort();
        })
    };
    let started = Instant::now();
    let result = socket.receive();
    let took = started.elapsed();
    aborter.join().unwrap();
    println!("then: {result:?} after {took:?}");
    assert!(result.is_err(), "{result:?}");
    assert!(took < Duration::from_secs(2), "{took:?}");
    assert!(
        socket.send_text("ping").is_err(),
        "aborted sockets refuse sends"
    );
}

/// Run with the server stopped and started again while it waits:
/// `NITRA_RESTART_TEST=1`.
#[test]
#[ignore = "needs the room server restarted during the test"]
fn reconnects_and_announces_again_after_a_server_restart() {
    if std::env::var("NITRA_RESTART_TEST").is_err() {
        return;
    }
    let room = new_room_id().unwrap();
    let identity = Arc::new(Identity::generate().unwrap());
    let (client, events) = start(
        &room,
        "alice",
        Join::Create {
            name: "Live test".into(),
        },
        &identity,
    );
    wait(&events, 10, |e| {
        matches!(e, RoomEvent::Connected(_)).then_some(())
    });
    client.send(Command::StartScreen(SCREEN())).unwrap();
    println!("READY: restart the server now");
    let (reason, retry_in) = wait(&events, 120, |e| match e {
        RoomEvent::Disconnected { reason, retry_in } => Some((reason, retry_in)),
        _ => None,
    });
    let dropped = Instant::now();
    println!("disconnected: {reason} (retry in {retry_in:?})");
    let welcome = wait(&events, 120, |e| match e {
        RoomEvent::Connected(w) => Some(w),
        RoomEvent::Disconnected { reason, retry_in } => {
            println!("still down: {reason} (retry in {retry_in:?})");
            None
        }
        _ => None,
    });
    println!("back after {:?}", dropped.elapsed());
    assert_eq!(welcome.you.name, "alice");
    // The screen was announced again on its own.
    let me = welcome.you.key.clone();
    let live = wait(&events, 10, |e| match e {
        RoomEvent::Presence(p) if p.key == me && p.screen.is_some() => Some(p),
        _ => None,
    });
    println!(
        "screen announced again: {:?}",
        live.screen.map(|s| s.info.name)
    );
}
