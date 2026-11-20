//! Host and viewer bound on every local adapter (not 127.0.0.1), the way
//! two PCs on a LAN would connect. Ignored by default: listening on the LAN
//! makes Windows Firewall ask for permission the first time a test binary
//! runs. Run with `cargo test -p transport --test network -- --ignored`.

mod common;

use std::time::{Duration, Instant};

use common::{FPS, access_unit, is_keyframe};
use transport::gather::{NetworkConfig, Scope};
use transport::interfaces::InterfaceKind;
use transport::{HostConfig, HostEvent, HostSession, VIDEO_CLOCK_HZ, ViewerEvent, ViewerSession};

#[test]
#[ignore]
fn host_and_viewer_connect_over_the_lan_addresses() {
    let lan = NetworkConfig {
        scope: Scope::Lan,
        stun_servers: Vec::new(),
    };
    let mut host = HostSession::bind_network(
        &lan,
        HostConfig {
            start_bitrate_bps: 12_000_000,
            max_bitrate_bps: 20_000_000,
        },
    )
    .unwrap();
    let candidates = &host.gathered().candidates;
    println!("host candidates: {candidates:#?}");
    if !candidates
        .iter()
        .any(|c| c.interface == InterfaceKind::Physical)
    {
        println!("no physical network adapter: nothing to test");
        return;
    }
    assert!(candidates.iter().all(|c| !c.addr.ip().is_loopback()));

    let (mut viewer, offer) = ViewerSession::offer_network(&lan).unwrap();
    let (_, answer) = host.accept(&offer).unwrap();
    viewer.accept_answer(&answer).unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut connected = false;
    let mut received = 0;
    let mut index = 0_u64;
    let mut next = Instant::now();
    while received < 60 {
        assert!(
            Instant::now() < deadline,
            "connected {connected}, {received} frames"
        );
        if connected && Instant::now() >= next {
            host.send_frame(
                &access_unit(index),
                index * VIDEO_CLOCK_HZ / FPS,
                is_keyframe(index),
            )
            .unwrap();
            index += 1;
            next += Duration::from_nanos(1_000_000_000 / FPS);
        }
        host.poll(Instant::now() + Duration::from_millis(1))
            .unwrap();
        while let Some(event) = host.next_event() {
            connected |= matches!(event, HostEvent::Connected(_));
        }
        viewer
            .poll(Instant::now() + Duration::from_millis(1))
            .unwrap();
        while let Some(event) = viewer.next_event() {
            if let ViewerEvent::Frame(frame) = event {
                let id = frame.rtp_time * FPS / VIDEO_CLOCK_HZ;
                assert_eq!(*frame.data, *access_unit(id));
                received += 1;
            }
        }
    }
    let (local, remote) = viewer.path().expect("no path");
    println!("viewer receives on {local} from {remote}");
    assert!(!local.ip().is_loopback());
}
