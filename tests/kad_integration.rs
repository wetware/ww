//! Integration tests for Kademlia DHT plumbing.
//!
//! Host CI supplies `WW_TEST_KUBO_ADDR` and requires a reachable daemon with
//! `WW_TEST_REQUIRE_KUBO=1`. Local runs also accept `IPFS_API_URL`.

#[path = "support/kubo.rs"]
mod kubo;

use libp2p::kad;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Connect an ephemeral peer so an isolated test daemon has a real swarm entry.
async fn connected_peers(client: &ww::ipfs::HttpClient) -> Vec<(String, String)> {
    use futures::StreamExt;
    use libp2p::swarm::{dial_opts::DialOpts, SwarmEvent};
    use std::time::Duration;

    let info = client.kubo_info().await.expect("Kubo identity");
    let mut peer = libp2p::SwarmBuilder::with_new_identity()
        .with_tokio()
        .with_tcp(
            Default::default(),
            libp2p::noise::Config::new,
            libp2p::yamux::Config::default,
        )
        .expect("peer TCP transport")
        .with_quic()
        .with_behaviour(|_| libp2p::swarm::dummy::Behaviour)
        .expect("peer behaviour")
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(10)))
        .build();
    let peer_id = peer.local_peer_id().to_string();
    let addresses = info
        .swarm_addrs
        .iter()
        .map(|addr| addr.parse().expect("Kubo swarm address"))
        .collect();
    peer.dial(
        DialOpts::peer_id(info.peer_id.parse().expect("Kubo peer ID"))
            .addresses(addresses)
            .build(),
    )
    .expect("dial test Kubo");

    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match peer.select_next_some().await {
                SwarmEvent::ConnectionEstablished { .. } => break,
                SwarmEvent::OutgoingConnectionError { error, .. } => {
                    panic!("dial test Kubo: {error}")
                }
                _ => {}
            }
        }
        loop {
            let peers = client.swarm_peers().await.expect("swarm_peers");
            if peers.iter().any(|(id, _)| id == &peer_id) {
                return peers;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("Kubo did not report the connected fixture peer")
}

/// Parse raw `(String, String)` swarm peers into typed `(PeerId, Multiaddr)`.
fn parse_peers(raw: Vec<(String, String)>) -> Vec<(libp2p::PeerId, libp2p::Multiaddr)> {
    raw.into_iter()
        .filter_map(|(p, a)| {
            let peer_id: libp2p::PeerId = p.parse().ok()?;
            let addr: libp2p::Multiaddr = a.parse().ok()?;
            Some((peer_id, addr))
        })
        .collect()
}

/// Build a fresh Kad client-mode behaviour with no peers.
fn fresh_kad() -> (libp2p::PeerId, kad::Behaviour<kad::store::MemoryStore>) {
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let peer_id = keypair.public().to_peer_id();
    let store = kad::store::MemoryStore::new(peer_id);
    let mut config = kad::Config::new(kad::PROTOCOL_NAME);
    config.set_periodic_bootstrap_interval(None);
    let mut kad = kad::Behaviour::with_config(peer_id, store, config);
    kad.set_mode(Some(kad::Mode::Client));
    (peer_id, kad)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Verify that `kubo_info()` returns a parseable peer ID and at least one
/// swarm address. This is the first call `Net::new` makes.
#[tokio::test]
async fn test_kubo_info_valid_identity() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let info = client.kubo_info().await.expect("kubo_info");

    let peer_id: libp2p::PeerId = info
        .peer_id
        .parse()
        .expect("Kubo peer ID should parse as libp2p PeerId");

    assert!(
        !info.swarm_addrs.is_empty(),
        "Kubo should report at least one swarm address"
    );

    // Every swarm address should parse as a valid Multiaddr.
    for raw in &info.swarm_addrs {
        let _addr: libp2p::Multiaddr = raw
            .parse()
            .unwrap_or_else(|e| panic!("swarm addr '{raw}' should parse: {e}"));
    }

    eprintln!(
        "OK: peer_id={peer_id}, {} swarm addrs",
        info.swarm_addrs.len(),
    );
}

/// Verify that swarm_peers() returns parseable peers and that they can be
/// added to a Kad routing table.  This is the plumbing that
/// `Net::new` relies on to populate the routing table from Kubo's
/// connected peers.
#[tokio::test]
async fn test_swarm_peers_populate_routing_table() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let raw = connected_peers(&client).await;
    assert!(!raw.is_empty(), "Kubo should have connected swarm peers");

    let peers = parse_peers(raw);
    assert!(
        !peers.is_empty(),
        "At least one swarm peer should parse into a valid (PeerId, Multiaddr)"
    );

    let (_local_id, mut kad) = fresh_kad();
    for (pid, addr) in &peers {
        kad.add_address(pid, addr.clone());
    }

    let bucket_count: usize = kad.kbuckets().map(|b| b.num_entries()).sum();
    assert!(
        bucket_count > 0,
        "Kad routing table should have entries after populating from swarm peers"
    );

    eprintln!(
        "OK: {} peers parsed, {} kbucket entries",
        peers.len(),
        bucket_count,
    );
}

/// Verify that the Kubo node itself can be added as a Kad bootstrap peer.
///
/// `Net::new` adds Kubo as a bootstrap entry alongside random swarm
/// peers.  This test validates that the `KuboInfo` peer ID + address can
/// populate the routing table the same way.
#[tokio::test]
async fn test_kubo_bootstrap_entry() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let info = client.kubo_info().await.expect("kubo_info");
    let kubo_pid: libp2p::PeerId = info.peer_id.parse().expect("parse kubo peer id");

    // Use the first parseable multiaddr.  Strip any trailing `/p2p/<id>`
    // component so we don't confuse Kad (the PeerId is already supplied
    // separately via `add_address`).
    let addr: libp2p::Multiaddr = info
        .swarm_addrs
        .iter()
        .find_map(|a| {
            let ma: libp2p::Multiaddr = a.parse().ok()?;
            // Strip trailing /p2p/… if present.
            let stripped: libp2p::Multiaddr = ma
                .iter()
                .filter(|p| !matches!(p, libp2p::multiaddr::Protocol::P2p(_)))
                .collect();
            if stripped.iter().count() > 0 {
                Some(stripped)
            } else {
                None
            }
        })
        .expect("Kubo should have at least one parseable swarm addr");

    let (_local_id, mut kad) = fresh_kad();
    kad.add_address(&kubo_pid, addr.clone());

    let bucket_count: usize = kad.kbuckets().map(|b| b.num_entries()).sum();
    assert_eq!(
        bucket_count, 1,
        "Adding Kubo bootstrap peer should yield exactly one kbucket entry"
    );

    eprintln!("OK: kubo {kubo_pid} added at {addr}");
}

/// Verify the CID → multihash → Kad RecordKey pipeline with a real CID
/// produced by Kubo.
///
/// This exercises the same CID-to-multihash conversion used by
/// `AnnouncerImpl::provide` and `FinderImpl::find_providers`, ensuring that CIDs
/// returned by Kubo's `add` endpoint are valid Kad keys.
#[tokio::test]
async fn test_real_cid_to_kad_record_key() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let cid_str = client
        .add_bytes(b"kad integration test payload")
        .await
        .expect("add_bytes");

    // Parse CID → extract multihash → build RecordKey.
    let cid: cid::Cid = cid_str
        .parse()
        .unwrap_or_else(|e| panic!("CID '{cid_str}' from Kubo should parse: {e}"));

    let mh_bytes = cid.hash().to_bytes();
    assert!(!mh_bytes.is_empty(), "Multihash bytes should not be empty");

    let record_key = kad::RecordKey::new(&mh_bytes);
    assert!(
        !record_key.as_ref().is_empty(),
        "RecordKey should contain the multihash bytes"
    );

    // Round-trip: RecordKey bytes should match the original multihash.
    assert_eq!(
        record_key.as_ref(),
        mh_bytes.as_slice(),
        "RecordKey should preserve multihash bytes exactly"
    );

    eprintln!("OK: CID {cid_str} → {} byte Kad key", mh_bytes.len());
}

/// Verify that distinct CIDs produce distinct Kad keys (collision sanity).
#[tokio::test]
async fn test_distinct_cids_produce_distinct_kad_keys() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let cid_a = client.add_bytes(b"payload-a").await.expect("add a");
    let cid_b = client.add_bytes(b"payload-b").await.expect("add b");

    assert_ne!(cid_a, cid_b, "distinct data should yield distinct CIDs");

    let key_a = cid_a.parse::<cid::Cid>().unwrap().hash().to_bytes();
    let key_b = cid_b.parse::<cid::Cid>().unwrap().hash().to_bytes();

    assert_ne!(key_a, key_b, "distinct CIDs should yield distinct Kad keys");

    eprintln!("OK: {cid_a} ≠ {cid_b}");
}

/// Verify that swarm peers have at least some unique peer IDs (dedup check).
///
/// The raw `swarm_peers()` response may contain multiple addrs per peer.
/// After dedup by PeerId, we should still have peers but fewer-or-equal
/// entries than the raw list.
#[tokio::test]
async fn test_swarm_peers_dedup_by_peer_id() {
    let Some(client) = kubo::client().await else {
        return;
    };

    let raw = connected_peers(&client).await;
    let total = raw.len();

    let mut seen = std::collections::HashSet::new();
    let unique: Vec<_> = raw
        .into_iter()
        .filter_map(|(p, a)| {
            let pid: libp2p::PeerId = p.parse().ok()?;
            let addr: libp2p::Multiaddr = a.parse().ok()?;
            if seen.insert(pid) {
                Some((pid, addr))
            } else {
                None
            }
        })
        .collect();

    assert!(
        !unique.is_empty(),
        "Should have at least one unique peer after dedup"
    );

    eprintln!("OK: {} raw entries → {} unique peers", total, unique.len(),);
}

#[test]
fn required_kubo_cannot_skip_kad_operations() {
    kubo::assert_required_kubo_fails("test_kubo_info_valid_identity");
}
