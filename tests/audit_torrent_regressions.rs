#![cfg(feature = "torrent")]
//! Torrent regression tests for defects found during the 0.6.4 audit.

mod mock_peer;
mod test_helpers;

use std::sync::Arc;
use std::time::Duration;

use gosh_dl::torrent::{BencodeValue, Metainfo};
use gosh_dl::{DownloadEngine, DownloadId, DownloadOptions, DownloadState, EngineConfig};
use tempfile::TempDir;

use mock_peer::{MockPeer, MockPeerConfig};
use test_helpers::TestTorrentBuilder;

fn engine_config(dir: &std::path::Path) -> EngineConfig {
    EngineConfig {
        download_dir: dir.to_path_buf(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    }
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

fn set_announce(torrent: &[u8], announce: Option<&str>) -> Vec<u8> {
    let mut value = BencodeValue::parse_exact(torrent).unwrap();
    if let BencodeValue::Dict(ref mut dict) = value {
        match announce {
            Some(url) => {
                dict.insert(
                    b"announce".to_vec(),
                    BencodeValue::Bytes(url.as_bytes().to_vec()),
                );
            }
            None => {
                dict.remove(b"announce".as_slice());
            }
        }
    }
    value.encode()
}

async fn wait_for(
    engine: &DownloadEngine,
    id: DownloadId,
    timeout: Duration,
    mut done: impl FnMut(&gosh_dl::DownloadStatus) -> bool,
) {
    tokio::time::timeout(timeout, async {
        loop {
            let status = engine.status(id).expect("download should exist");
            if done(&status) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("download did not reach the expected state in time");
}

/// Original failure: the magnet `dn` parameter was stored verbatim as the
/// output filename, and `cancel(id, true)` joined it onto the download
/// directory and called `remove_dir_all`, so `dn=..%2Fvictim` deleted a
/// directory *outside* the download directory.
#[tokio::test]
async fn magnet_display_name_cannot_escape_the_download_dir_on_delete() {
    let root = TempDir::new().unwrap();
    let downloads = root.path().join("downloads");
    let victim = root.path().join("victim");
    tokio::fs::create_dir_all(&downloads).await.unwrap();
    tokio::fs::create_dir_all(&victim).await.unwrap();
    tokio::fs::write(victim.join("keep.txt"), b"precious")
        .await
        .unwrap();

    let engine = DownloadEngine::new(engine_config(&downloads))
        .await
        .unwrap();
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=..%2Fvictim";
    let id = engine
        .add_magnet(
            magnet,
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    engine.cancel(id, true).await.unwrap();

    assert!(
        victim.join("keep.txt").exists(),
        "cancel with delete must never leave the download directory"
    );
    engine.shutdown().await.unwrap();
}

/// Original failure: a magnet with no metadata has no output name, yet
/// `cancel(id, true)` fell back to deleting `<save_dir>/download`, an
/// unrelated file that happened to share the HTTP fallback name.
#[tokio::test]
async fn cancel_with_delete_on_a_metadata_less_magnet_leaves_other_files_alone() {
    let dir = TempDir::new().unwrap();
    let bystander = dir.path().join("download");
    tokio::fs::write(&bystander, b"unrelated").await.unwrap();
    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_magnet(
            "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567",
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    engine.cancel(id, true).await.unwrap();
    assert!(bystander.exists(), "unrelated file must survive");
    engine.shutdown().await.unwrap();
}

/// Original failure: `Metainfo::parse` accepted `..` and absolute segments
/// in the torrent name and file paths. Piece writes rejected them later, but
/// the engine already used the raw name for lifecycle paths (delete).
#[test]
fn metainfo_rejects_names_and_paths_that_escape_the_save_dir() {
    let (data, _) = build_torrent("../outside", 16384, 1);
    let err = Metainfo::parse(&data).expect_err("parent-dir name must be rejected");
    assert!(err.to_string().contains("parent"), "{err}");

    let (data, _) = build_torrent("/abs", 16384, 1);
    assert!(
        Metainfo::parse(&data).is_err(),
        "absolute name must be rejected"
    );

    // Multi-file torrent with a traversal in a file path.
    let (data, _) = TestTorrentBuilder::new("multi")
        .add_file("multi/a.txt", vec![1u8; 100])
        .add_file("multi/b.txt", vec![2u8; 100])
        .build();
    let mut value = BencodeValue::parse_exact(&data).unwrap();
    if let BencodeValue::Dict(ref mut dict) = value {
        if let Some(BencodeValue::Dict(info)) = dict.get_mut(b"info".as_slice()) {
            if let Some(BencodeValue::List(files)) = info.get_mut(b"files".as_slice()) {
                if let Some(BencodeValue::Dict(file)) = files.get_mut(0) {
                    file.insert(
                        b"path".to_vec(),
                        BencodeValue::List(vec![
                            BencodeValue::Bytes(b"..".to_vec()),
                            BencodeValue::Bytes(b"escaped.txt".to_vec()),
                        ]),
                    );
                }
            }
        }
    }
    assert!(
        Metainfo::parse(&value.encode()).is_err(),
        "file path traversal must be rejected"
    );

    // Sanity: a well-formed torrent still parses.
    let (data, _) = build_torrent("fine", 16384, 1);
    assert!(Metainfo::parse(&data).is_ok());
}

/// Original failure: the streaming reader requested 64 KiB chunks from the
/// piece manager, but `read_block` caps requests at one wire block
/// (16 KiB + 1 KiB), so the reader looped forever on any torrent whose
/// piece length exceeded 16 KiB, i.e. every real-world torrent.
#[tokio::test]
async fn reader_streams_torrents_with_pieces_larger_than_a_block() {
    use tokio::io::AsyncReadExt;
    let piece_length = 64 * 1024;
    let (data, pieces) = build_torrent("big-pieces", piece_length, 2);
    let content: Vec<u8> = pieces.iter().flatten().copied().collect();
    let dir = TempDir::new().unwrap();
    tokio::fs::write(dir.path().join("big-pieces"), &content)
        .await
        .unwrap();
    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_torrent(&set_announce(&data, None), DownloadOptions::default())
        .await
        .unwrap();
    let mut reader = engine.open_reader(id, 0).unwrap();
    let mut streamed = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), reader.read_to_end(&mut streamed))
        .await
        .expect("reader must not stall on large pieces")
        .unwrap();
    assert_eq!(streamed, content);
    engine.shutdown().await.unwrap();
}

/// Original failure: cancelling an active torrent aborted the peer-loop
/// task, but the per-peer connection tasks it owned were only aborted by
/// that loop's own cleanup, which never ran. A lingering peer finished its
/// in-flight block after `cancel(id, true)` had deleted the files and wrote
/// the piece straight back to disk.
#[cfg(feature = "http")]
#[tokio::test]
async fn cancel_with_delete_stops_in_flight_peer_writes() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let piece_length = 16384;
    let num_pieces = 64;
    let (data, pieces) = build_torrent("swarm-file", piece_length, num_pieces);
    let metainfo = Metainfo::parse(&data).unwrap();

    // A mock seeder with every piece, advertised through a mock HTTP tracker.
    let mut seeder_config = MockPeerConfig::new(metainfo.info_hash, num_pieces);
    for (i, piece) in pieces.iter().enumerate() {
        seeder_config = seeder_config.with_piece(i as u32, piece.clone());
    }
    let seeder = Arc::new(MockPeer::new(seeder_config).await.unwrap());
    Arc::clone(&seeder).start_accepting();
    let seeder_addr = seeder.addr();
    let std::net::IpAddr::V4(ip) = seeder_addr.ip() else {
        panic!("mock peer must bind IPv4");
    };
    let mut compact = ip.octets().to_vec();
    compact.extend_from_slice(&seeder_addr.port().to_be_bytes());
    let mut tracker_body = b"d8:intervali1800e5:peers6:".to_vec();
    tracker_body.extend_from_slice(&compact);
    tracker_body.push(b'e');

    let tracker = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/announce"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tracker_body))
        .mount(&tracker)
        .await;

    let dir = TempDir::new().unwrap();
    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_torrent(
            &set_announce(&data, Some(&format!("{}/announce", tracker.uri()))),
            DownloadOptions {
                // Throttle so the transfer is reliably mid-flight when cancelled.
                max_download_speed: Some(96 * 1024),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    wait_for(&engine, id, Duration::from_secs(20), |s| {
        s.progress.completed_size > 0
            && s.progress.completed_size < (piece_length * num_pieces) as u64
    })
    .await;

    engine.cancel(id, true).await.unwrap();
    let target = dir.path().join("swarm-file");
    assert!(!target.exists(), "cancel must delete the file");
    // Any peer task that survived cancel would write its next block here.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !target.exists(),
        "peer tasks must be stopped so they cannot resurrect deleted files"
    );
    assert!(engine.status(id).is_none());
    engine.shutdown().await.unwrap();
}

/// A paused torrent's completion must not be reported as `Completed` by a
/// worker that was already stopped, and resume must rebuild it. Kept as a
/// guard around the handle bookkeeping touched by the cancel fix.
#[tokio::test]
async fn paused_torrent_can_be_cancelled_without_delete() {
    let dir = TempDir::new().unwrap();
    let (data, _) = build_torrent("pause-cancel", 16384, 1);
    let engine = DownloadEngine::new(engine_config(dir.path()))
        .await
        .unwrap();
    let id = engine
        .add_torrent(
            &set_announce(&data, None),
            DownloadOptions {
                start_paused: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(engine.status(id).unwrap().state, DownloadState::Paused);
    engine.cancel(id, false).await.unwrap();
    assert!(engine.status(id).is_none());
    engine.shutdown().await.unwrap();
}

/// Original failure: with the default encryption policy (`Preferred`), the
/// outgoing MSE handshake reported a peer that closed the socket as
/// "plaintext" and the engine then sent its BitTorrent handshake on that
/// dead socket. The documented fallback (a fresh plaintext connection) was
/// unreachable, so the engine could not download from any plaintext-only
/// peer unless encryption was explicitly disabled.
#[cfg(feature = "http")]
#[tokio::test]
async fn default_encryption_policy_falls_back_to_plaintext_peers() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let piece_length = 16384;
    let num_pieces = 4;
    let (data, pieces) = build_torrent("plain-peer", piece_length, num_pieces);
    let metainfo = Metainfo::parse(&data).unwrap();
    let content: Vec<u8> = pieces.iter().flatten().copied().collect();

    let mut seeder_config = MockPeerConfig::new(metainfo.info_hash, num_pieces);
    for (i, piece) in pieces.iter().enumerate() {
        seeder_config = seeder_config.with_piece(i as u32, piece.clone());
    }
    let seeder = Arc::new(MockPeer::new(seeder_config).await.unwrap());
    Arc::clone(&seeder).start_accepting();
    let seeder_addr = seeder.addr();
    let std::net::IpAddr::V4(ip) = seeder_addr.ip() else {
        panic!("mock peer must bind IPv4");
    };
    let mut tracker_body = b"d8:intervali1800e5:peers6:".to_vec();
    tracker_body.extend_from_slice(&ip.octets());
    tracker_body.extend_from_slice(&seeder_addr.port().to_be_bytes());
    tracker_body.push(b'e');
    let tracker = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/announce"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(tracker_body))
        .mount(&tracker)
        .await;

    let dir = TempDir::new().unwrap();
    let config = engine_config(dir.path());
    assert_eq!(
        config.torrent.encryption.policy,
        gosh_dl::config::EncryptionPolicy::Preferred,
        "test must run with the default (Preferred) policy"
    );
    let engine = DownloadEngine::new(config).await.unwrap();
    let id = engine
        .add_torrent(
            &set_announce(&data, Some(&format!("{}/announce", tracker.uri()))),
            DownloadOptions::default(),
        )
        .await
        .unwrap();
    wait_for(&engine, id, Duration::from_secs(20), |s| {
        matches!(s.state, DownloadState::Seeding | DownloadState::Completed)
    })
    .await;
    assert_eq!(
        tokio::fs::read(dir.path().join("plain-peer"))
            .await
            .unwrap(),
        content
    );
    engine.shutdown().await.unwrap();
}
