#![cfg(feature = "torrent")]
//! Torrent regression tests for defects found during the deep-dive audit
//! that followed 0.6.4. Each test documents the original failure in its doc
//! comment.

mod mock_peer;
mod test_helpers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gosh_dl::config::EncryptionPolicy;
use gosh_dl::torrent::{
    AnnounceEvent, AnnounceRequest, BencodeValue, Metainfo, PeerConnection, PeerMessage,
    TrackerClient,
};
use gosh_dl::{
    DownloadEngine, DownloadId, DownloadOptions, DownloadState, EngineConfig, EngineError,
    NetworkErrorKind,
};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use mock_peer::{MockPeer, MockPeerConfig};
use test_helpers::TestTorrentBuilder;

const PROTOCOL: &[u8] = b"BitTorrent protocol";

fn engine_config(dir: &std::path::Path) -> EngineConfig {
    let mut config = EngineConfig {
        download_dir: dir.to_path_buf(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    // The scripted peers below speak plaintext only.
    config.torrent.encryption.policy = EncryptionPolicy::Disabled;
    config
}

fn build_torrent(name: &str, piece_length: usize, num_pieces: usize) -> (Vec<u8>, Vec<Vec<u8>>) {
    let total = piece_length * num_pieces;
    let content: Vec<u8> = (0..total).map(|i| (i % 256) as u8).collect();
    let (data, _) = TestTorrentBuilder::new(name)
        .piece_length(piece_length as u64)
        .add_file(name, content.clone())
        .build();
    let pieces = content
        .chunks(piece_length)
        .map(|c| c.to_vec())
        .collect::<Vec<_>>();
    (data, pieces)
}

fn with_announce(torrent: &[u8], url: &str) -> Vec<u8> {
    let mut value = BencodeValue::parse_exact(torrent).unwrap();
    if let BencodeValue::Dict(ref mut dict) = value {
        dict.insert(
            b"announce".to_vec(),
            BencodeValue::Bytes(url.as_bytes().to_vec()),
        );
    }
    value.encode()
}

/// An HTTP tracker that hands out exactly one peer.
async fn tracker_announcing(peer: SocketAddr) -> wiremock::MockServer {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let std::net::IpAddr::V4(ip) = peer.ip() else {
        panic!("test peers bind IPv4");
    };
    let mut body = b"d8:intervali1800e5:peers6:".to_vec();
    body.extend_from_slice(&ip.octets());
    body.extend_from_slice(&peer.port().to_be_bytes());
    body.push(b'e');

    let tracker = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/announce"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&tracker)
        .await;
    tracker
}

async fn wait_for(
    engine: &DownloadEngine,
    id: DownloadId,
    timeout: Duration,
    mut done: impl FnMut(&gosh_dl::DownloadStatus) -> bool,
) -> gosh_dl::DownloadStatus {
    tokio::time::timeout(timeout, async {
        loop {
            let status = engine.status(id).expect("download should exist");
            if done(&status) {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("download did not reach the expected state in time")
}

/// Original failure: `cancel(id, true)` on a multi-file torrent ran
/// `remove_dir_all` on `<save_dir>/<torrent name>`. A torrent named after a
/// directory that already existed (`Documents`, `Music`, a previous
/// download) took everything in that directory with it, including files
/// that were never part of the torrent.
#[tokio::test]
async fn cancel_with_delete_removes_only_the_torrents_own_files() {
    let dir = TempDir::new().unwrap();
    let (data, _) = TestTorrentBuilder::new("shared")
        .add_file("sub/a.bin", vec![1u8; 100])
        .add_file("b.bin", vec![2u8; 100])
        .build();

    // The directory already exists and holds files of the user's own, plus
    // (partial) torrent data from an earlier run.
    let root = dir.path().join("shared");
    tokio::fs::create_dir_all(root.join("sub")).await.unwrap();
    tokio::fs::create_dir_all(root.join("mine")).await.unwrap();
    tokio::fs::write(root.join("keep.txt"), b"precious")
        .await
        .unwrap();
    tokio::fs::write(root.join("mine/notes.txt"), b"also precious")
        .await
        .unwrap();
    tokio::fs::write(root.join("sub/a.bin"), vec![1u8; 100])
        .await
        .unwrap();
    tokio::fs::write(root.join("b.bin"), vec![2u8; 100])
        .await
        .unwrap();

    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_torrent(
            &data,
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    engine.cancel(id, true).await.unwrap();

    assert!(!root.join("sub/a.bin").exists(), "torrent file is deleted");
    assert!(!root.join("b.bin").exists(), "torrent file is deleted");
    assert!(
        !root.join("sub").exists(),
        "directories emptied by the delete are removed"
    );
    assert_eq!(
        tokio::fs::read(root.join("keep.txt")).await.unwrap(),
        b"precious",
        "files that are not part of the torrent must survive"
    );
    assert!(root.join("mine/notes.txt").exists());
    engine.shutdown().await.unwrap();
}

/// A torrent that owned its whole directory still leaves nothing behind.
#[tokio::test]
async fn cancel_with_delete_removes_a_directory_the_torrent_filled_alone() {
    let dir = TempDir::new().unwrap();
    let (data, _) = TestTorrentBuilder::new("alone")
        .add_file("deep/er/a.bin", vec![1u8; 100])
        .add_file("b.bin", vec![2u8; 100])
        .build();
    let root = dir.path().join("alone");
    tokio::fs::create_dir_all(root.join("deep/er"))
        .await
        .unwrap();
    tokio::fs::write(root.join("deep/er/a.bin"), vec![1u8; 100])
        .await
        .unwrap();
    tokio::fs::write(root.join("b.bin"), vec![2u8; 100])
        .await
        .unwrap();

    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_torrent(
            &data,
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    engine.cancel(id, true).await.unwrap();
    assert!(!root.exists(), "nothing of the torrent is left on disk");
    engine.shutdown().await.unwrap();
}

/// Original failure: when a verified piece could not be written (disk full,
/// permission denied, the output path being unusable) the error was logged
/// and the piece stayed in the "pending" set fully received, where neither
/// stale-piece cleanup nor piece selection would ever touch it again. The
/// torrent sat in `Downloading` forever and no failure was ever reported.
#[tokio::test]
async fn unwritable_output_fails_the_torrent_instead_of_hanging() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (data, pieces) = build_torrent("blocked", piece_length, num_pieces);
    let metainfo = Metainfo::parse(&data).unwrap();

    let mut seeder_config = MockPeerConfig::new(metainfo.info_hash, num_pieces);
    for (i, piece) in pieces.iter().enumerate() {
        seeder_config = seeder_config.with_piece(i as u32, piece.clone());
    }
    let seeder = Arc::new(MockPeer::new(seeder_config).await.unwrap());
    Arc::clone(&seeder).start_accepting();
    let tracker = tracker_announcing(seeder.addr()).await;

    let dir = TempDir::new().unwrap();
    // A directory sits where the torrent's single output file must go, so
    // every piece write fails.
    tokio::fs::create_dir_all(dir.path().join("blocked"))
        .await
        .unwrap();

    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let mut events = engine.subscribe();
    let id = engine
        .add_torrent(
            &with_announce(&data, &format!("{}/announce", tracker.uri())),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    let status = wait_for(&engine, id, Duration::from_secs(30), |s| {
        matches!(
            s.state,
            DownloadState::Error { .. } | DownloadState::Completed
        )
    })
    .await;
    let DownloadState::Error { message, .. } = &status.state else {
        panic!("a torrent that cannot be written must fail, got {status:?}");
    };
    assert!(message.contains("piece"), "unhelpful error: {message}");

    // ... and the failure is announced, not just visible in the status.
    let failed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match events.recv().await {
                Ok(gosh_dl::DownloadEvent::Failed { id: failed_id, .. }) if failed_id == id => {
                    return true
                }
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(failed, "a Failed event must be emitted");
    engine.shutdown().await.unwrap();
}

/// A scripted seeder that records the `Have` messages it receives.
async fn recording_seeder(
    info_hash: [u8; 20],
    pieces: Vec<Vec<u8>>,
) -> (SocketAddr, Arc<parking_lot::Mutex<Vec<u32>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let haves = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let recorded = Arc::clone(&haves);
    let pieces = Arc::new(pieces);

    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let pieces = Arc::clone(&pieces);
            let recorded = Arc::clone(&recorded);
            tokio::spawn(async move {
                let mut handshake = [0u8; 68];
                if sock.read_exact(&mut handshake).await.is_err() || handshake[0] != 19 {
                    return;
                }
                let mut reply = vec![19u8];
                reply.extend_from_slice(PROTOCOL);
                reply.extend_from_slice(&[0u8; 8]);
                reply.extend_from_slice(&info_hash);
                reply.extend_from_slice(b"-RS0001-recordseeder");
                if sock.write_all(&reply).await.is_err() {
                    return;
                }

                // Bitfield (all pieces) + unchoke
                let mut bits = vec![0u8; pieces.len().div_ceil(8)];
                for i in 0..pieces.len() {
                    bits[i / 8] |= 0x80 >> (i % 8);
                }
                let _ = sock
                    .write_all(&PeerMessage::Bitfield { bitfield: bits }.encode())
                    .await;
                let _ = sock.write_all(&PeerMessage::Unchoke.encode()).await;

                loop {
                    let mut len = [0u8; 4];
                    if sock.read_exact(&mut len).await.is_err() {
                        return;
                    }
                    let len = u32::from_be_bytes(len) as usize;
                    if len == 0 {
                        continue;
                    }
                    let mut body = vec![0u8; len];
                    if sock.read_exact(&mut body).await.is_err() {
                        return;
                    }
                    match PeerMessage::decode(&body) {
                        Ok(PeerMessage::Have { piece_index }) => {
                            recorded.lock().push(piece_index);
                        }
                        Ok(PeerMessage::Request {
                            index,
                            begin,
                            length,
                        }) => {
                            let piece = &pieces[index as usize];
                            let block = piece[begin as usize..(begin + length) as usize].to_vec();
                            let msg = PeerMessage::Piece {
                                index,
                                begin,
                                block,
                            };
                            if sock.write_all(&msg.encode()).await.is_err() {
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
    });

    (addr, haves)
}

/// Original failure: the client sent its bitfield when a connection opened
/// and then never sent a single `Have`. Peers were left believing we held
/// only what we had at connect time, so they never requested the pieces we
/// completed afterwards and tit-for-tat peers treated us as a leech.
#[tokio::test]
async fn completed_pieces_are_announced_with_have_messages() {
    let piece_length = 16384;
    let num_pieces = 6;
    let (data, pieces) = build_torrent("announce-haves", piece_length, num_pieces);
    let metainfo = Metainfo::parse(&data).unwrap();
    let (seeder_addr, haves) = recording_seeder(metainfo.info_hash, pieces).await;
    let tracker = tracker_announcing(seeder_addr).await;

    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_torrent(
            &with_announce(&data, &format!("{}/announce", tracker.uri())),
            DownloadOptions::default(),
        )
        .await
        .unwrap();

    wait_for(&engine, id, Duration::from_secs(30), |s| {
        s.progress.completed_size == (piece_length * num_pieces) as u64
    })
    .await;

    // Every piece but possibly the last is announced while blocks are still
    // flowing; the last one follows on the connection's next wake-up.
    let announced = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if haves.lock().len() >= num_pieces - 1 {
                return haves.lock().clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| haves.lock().clone());
    assert!(
        announced.len() >= num_pieces - 1,
        "expected a Have per completed piece, the peer saw {announced:?}"
    );
    let mut unique = announced.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), announced.len(), "no piece is announced twice");
    engine.shutdown().await.unwrap();
}

/// Original failure: `PeerConnection::recv` wrapped `read_exact` in a
/// timeout. When the timer fired after part of a message had been read,
/// those bytes were gone; the caller treats a receive timeout as "the peer
/// is idle" and calls `recv` again, which then parsed the middle of the
/// message as a new length prefix. The connection was lost (or, on an
/// encrypted stream, permanently garbled).
///
/// This test takes ~11s: the receive timeout is 10s and not configurable.
#[tokio::test]
async fn a_receive_timeout_mid_message_does_not_desynchronise_the_stream() {
    let info_hash = [7u8; 20];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut handshake = [0u8; 68];
        sock.read_exact(&mut handshake).await.unwrap();
        let mut reply = vec![19u8];
        reply.extend_from_slice(PROTOCOL);
        reply.extend_from_slice(&[0u8; 8]);
        reply.extend_from_slice(&info_hash);
        reply.extend_from_slice(b"-SL0001-slowpeer0000");
        sock.write_all(&reply).await.unwrap();

        // `Have { piece_index: 7 }` is 00 00 00 05 | 04 | 00 00 00 07.
        // Send half of the length prefix, stall past the receive timeout,
        // then send the rest.
        sock.write_all(&[0, 0]).await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(10_600)).await;
        sock.write_all(&[0, 5, 4, 0, 0, 0, 7]).await.unwrap();
        sock.flush().await.unwrap();
        tokio::time::sleep(Duration::from_secs(5)).await;
    });

    let mut conn = PeerConnection::connect(addr, info_hash, [1u8; 20], 16)
        .await
        .expect("handshake");

    let first = conn.recv().await;
    assert!(
        matches!(
            first,
            Err(EngineError::Network {
                kind: NetworkErrorKind::Timeout,
                ..
            })
        ),
        "the stalled message should time out, got {first:?}"
    );

    let second = conn.recv().await.expect("stream must still be in sync");
    assert_eq!(second, PeerMessage::Have { piece_index: 7 });
}

/// Original failure: UDP tracker sockets were always bound to `0.0.0.0`,
/// and the first address the resolver returned was used as-is. On
/// dual-stack hosts resolvers commonly list IPv6 first, and an IPv4 socket
/// cannot connect to an IPv6 address, so UDP announces to any tracker with
/// an AAAA record failed outright.
#[tokio::test]
async fn udp_tracker_reachable_over_ipv6() {
    let Ok(tracker) = tokio::net::UdpSocket::bind("[::1]:0").await else {
        eprintln!("skipping: no IPv6 loopback on this machine");
        return;
    };
    let port = tracker.local_addr().unwrap().port();

    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        loop {
            let Ok((len, from)) = tracker.recv_from(&mut buf).await else {
                return;
            };
            if len < 16 {
                continue;
            }
            let action = u32::from_be_bytes(buf[8..12].try_into().unwrap());
            let txid = &buf[12..16];
            let mut reply = Vec::new();
            if action == 0 {
                // connect -> action, transaction id, connection id
                reply.extend_from_slice(&0u32.to_be_bytes());
                reply.extend_from_slice(txid);
                reply.extend_from_slice(&0x1122_3344_5566_7788u64.to_be_bytes());
            } else {
                // announce -> action, transaction id, interval, leechers, seeders
                reply.extend_from_slice(&1u32.to_be_bytes());
                reply.extend_from_slice(txid);
                reply.extend_from_slice(&1800u32.to_be_bytes());
                reply.extend_from_slice(&3u32.to_be_bytes());
                reply.extend_from_slice(&5u32.to_be_bytes());
            }
            let _ = tracker.send_to(&reply, from).await;
        }
    });

    let client = TrackerClient::new().unwrap();
    let request = AnnounceRequest {
        info_hash: [9u8; 20],
        peer_id: *client.peer_id(),
        port: 6881,
        uploaded: 0,
        downloaded: 0,
        left: 1000,
        event: AnnounceEvent::Started,
        compact: true,
        numwant: Some(50),
        key: None,
        tracker_id: None,
    };
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.announce(&format!("udp://[::1]:{port}/announce"), &request),
    )
    .await
    .expect("announce should not hang")
    .expect("an IPv6 UDP tracker must be reachable");
    assert_eq!(response.complete, Some(5));
    assert_eq!(response.incomplete, Some(3));
}
