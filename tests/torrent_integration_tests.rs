#![cfg(feature = "torrent")]
//! Torrent Integration Tests
//!
//! These tests exercise the BitTorrent downloading stack using MockPeer and
//! TestTorrentBuilder infrastructure. They cover peer handshake, piece
//! downloading, hash verification, state transitions, and engine-level
//! torrent management.

mod mock_peer;
mod test_helpers;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use gosh_dl::torrent::{Metainfo, PeerConnection, PieceManager, TorrentConfig, TorrentDownloader};
use gosh_dl::{DownloadEngine, DownloadEvent, DownloadId, DownloadOptions, EngineConfig};
use sha1::{Digest, Sha1};
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::timeout;

use mock_peer::{MockPeer, MockPeerConfig};
use test_helpers::TestTorrentBuilder;

// =============================================================================
// Helpers
// =============================================================================

/// Create a TorrentConfig with all network discovery disabled (no DHT/LPD/PEX).
fn test_torrent_config() -> TorrentConfig {
    TorrentConfig {
        max_peers: 10,
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        seed_ratio: Some(0.0), // Stop immediately after download
        tick_interval_ms: 50,
        connect_interval_secs: 1,
        choking_interval_secs: 5,
        request_timeout: Duration::from_secs(10),
        ..TorrentConfig::default()
    }
}

/// Build test torrent data and return (torrent_bytes, metainfo, piece_data_vec).
///
/// `piece_length` should be a multiple of BLOCK_SIZE (16384) for simplicity.
/// `num_pieces` determines how many pieces the torrent has.
fn build_test_torrent(
    name: &str,
    piece_length: usize,
    num_pieces: usize,
) -> (Vec<u8>, Metainfo, Vec<Vec<u8>>) {
    let total_size = piece_length * num_pieces;
    let content: Vec<u8> = (0..total_size).map(|i| (i % 256) as u8).collect();

    // Use the name directly as both the torrent name and filename.
    // For single-file torrents, PieceManager saves to save_dir/metainfo.info.name.
    let (torrent_data, _piece_hashes) = TestTorrentBuilder::new(name)
        .piece_length(piece_length as u64)
        .add_file(name, content.clone())
        .build();

    let metainfo = Metainfo::parse(&torrent_data).expect("TestTorrentBuilder should produce valid torrent data");

    // Slice content into piece data
    let mut pieces = Vec::with_capacity(num_pieces);
    for i in 0..num_pieces {
        let start = i * piece_length;
        let end = (start + piece_length).min(content.len());
        pieces.push(content[start..end].to_vec());
    }

    (torrent_data, metainfo, pieces)
}

/// Create a MockPeer that has all pieces of the given torrent.
async fn create_seeder(
    info_hash: [u8; 20],
    piece_data: &[Vec<u8>],
) -> Arc<MockPeer> {
    let num_pieces = piece_data.len();
    let mut config = MockPeerConfig::new(info_hash, num_pieces);
    config.auto_unchoke = true;
    config.support_extensions = true;

    for (i, data) in piece_data.iter().enumerate() {
        config = config.with_piece(i as u32, data.clone());
    }

    let peer = MockPeer::new(config).await.expect("MockPeer should bind");
    let peer = Arc::new(peer);
    peer.clone().start_accepting();
    peer
}

/// Create a TorrentDownloader, inject peers, start and run it.
/// Returns when the download completes or the timeout expires.
async fn run_torrent_download(
    metainfo: Metainfo,
    peers: &[SocketAddr],
    save_dir: &std::path::Path,
    timeout_secs: u64,
) -> Arc<TorrentDownloader> {
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _event_rx) = broadcast::channel(64);

    // Override announce URL to empty so start() doesn't try to contact real trackers
    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(id, metainfo, save_dir.to_path_buf(), config, event_tx)
            .expect("TorrentDownloader creation should succeed"),
    );

    // start() verifies existing files and sets state
    Arc::clone(&downloader)
        .start()
        .await
        .expect("start() should succeed with no trackers");

    // Inject mock peer addresses
    downloader.add_known_peers(peers.iter().cloned());

    // Run the peer loop in a background task
    let dl = Arc::clone(&downloader);
    let loop_handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    // Poll for completion
    let start = std::time::Instant::now();
    let deadline = Duration::from_secs(timeout_secs);
    while start.elapsed() < deadline {
        if downloader.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Stop the peer loop
    downloader.pause();
    tokio::time::sleep(Duration::from_millis(200)).await;
    loop_handle.abort();

    downloader
}

// =============================================================================
// Metainfo Parsing Tests
// =============================================================================

#[test]
fn test_metainfo_parse_from_builder() {
    let piece_length = 16384;
    let num_pieces = 2;
    let (_torrent_data, metainfo, piece_data) = build_test_torrent("parse-test", piece_length, num_pieces);

    // Verify basic metainfo fields
    assert_eq!(metainfo.info.name, "parse-test");
    assert_eq!(metainfo.info.piece_length, piece_length as u64);
    assert_eq!(metainfo.info.pieces.len(), num_pieces);
    assert_eq!(metainfo.info.total_size, (piece_length * num_pieces) as u64);
    assert!(metainfo.info.is_single_file);
    assert!(!metainfo.info.private);

    // Verify piece hashes match the actual data
    for (i, data) in piece_data.iter().enumerate() {
        let mut hasher = Sha1::new();
        hasher.update(data);
        let expected_hash: [u8; 20] = hasher.finalize().into();
        assert_eq!(
            metainfo.info.pieces[i], expected_hash,
            "Piece {} hash should match computed hash",
            i
        );
    }
}

#[test]
fn test_metainfo_parse_multi_file() {
    let (torrent_data, _) = TestTorrentBuilder::multi_file("multi-test").build();
    let metainfo = Metainfo::parse(&torrent_data).expect("Should parse multi-file torrent");

    assert_eq!(metainfo.info.name, "multi-test");
    assert!(!metainfo.info.is_single_file);
    assert!(metainfo.info.files.len() >= 3, "Should have at least 3 files");
    assert!(metainfo.info.total_size > 0);
}

#[test]
fn test_engine_rejects_invalid_torrent_data() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let temp_dir = TempDir::new().unwrap();
        let config = EngineConfig {
            download_dir: temp_dir.path().to_path_buf(),
            ..Default::default()
        };
        let engine = DownloadEngine::new(config).await.unwrap();

        // Empty data
        let result = engine.add_torrent(b"", DownloadOptions::default()).await;
        assert!(result.is_err(), "Should reject empty torrent data");

        // Invalid bencode
        let result = engine
            .add_torrent(b"not valid bencode", DownloadOptions::default())
            .await;
        assert!(result.is_err(), "Should reject invalid bencode");

        // Valid bencode but missing info dict
        let result = engine
            .add_torrent(b"d8:announcei42ee", DownloadOptions::default())
            .await;
        assert!(result.is_err(), "Should reject torrent without info dict");

        engine.shutdown().await.ok();
    });
}

// =============================================================================
// Peer Connection Tests
// =============================================================================

#[tokio::test]
async fn test_peer_handshake_with_mock_peer() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) = build_test_torrent("handshake-test", piece_length, 1);
    let info_hash = metainfo.info_hash;

    // Create and start the mock peer
    let mock_peer = create_seeder(info_hash, &piece_data).await;
    let peer_addr = mock_peer.addr();

    // Connect with PeerConnection
    let our_peer_id = *b"-GO0001-testhandshak";
    let conn = PeerConnection::connect(peer_addr, info_hash, our_peer_id, 1).await;

    assert!(conn.is_ok(), "Should successfully handshake with MockPeer");

    let conn = conn.unwrap();
    assert_eq!(conn.addr(), peer_addr);
    assert!(conn.peer_id().is_some(), "Should have peer ID after handshake");
}

#[tokio::test]
async fn test_peer_handshake_wrong_info_hash() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) = build_test_torrent("wrong-hash-test", piece_length, 1);
    let info_hash = metainfo.info_hash;

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    let peer_addr = mock_peer.addr();

    // Try to connect with a different info_hash
    let wrong_hash = [0xFFu8; 20];
    let our_peer_id = *b"-GO0001-wronghashtes";
    let conn = PeerConnection::connect(peer_addr, wrong_hash, our_peer_id, 1).await;

    assert!(conn.is_err(), "Should fail with mismatched info_hash");
}

// =============================================================================
// Single Piece Download Tests
// =============================================================================

#[tokio::test]
async fn test_single_piece_download() {
    let piece_length = 16384; // Exactly one block
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("single-piece", piece_length, 1);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader =
        run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Single-piece download should complete"
    );

    // Verify the downloaded file
    let file_path = temp_dir.path().join("single-piece");
    assert!(file_path.exists(), "Downloaded file should exist");

    let content = tokio::fs::read(&file_path).await.unwrap();
    assert_eq!(content, piece_data[0], "File content should match piece data");
}

#[tokio::test]
async fn test_multi_piece_download() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("multi-piece", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader =
        run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Multi-piece download should complete"
    );

    // Verify the file content
    let file_path = temp_dir.path().join("multi-piece");
    assert!(file_path.exists(), "Downloaded file should exist");

    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(
        content.len(),
        expected.len(),
        "File size should match total torrent size"
    );
    assert_eq!(content, expected, "File content should match");
}

// =============================================================================
// Multiple Peer Tests
// =============================================================================

#[tokio::test]
async fn test_download_from_multiple_peers() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("multi-peer", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();

    // Create 3 seeders, all with full data
    let peer1 = create_seeder(info_hash, &piece_data).await;
    let peer2 = create_seeder(info_hash, &piece_data).await;
    let peer3 = create_seeder(info_hash, &piece_data).await;

    let peer_addrs = vec![peer1.addr(), peer2.addr(), peer3.addr()];

    let downloader =
        run_torrent_download(metainfo, &peer_addrs, temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Download from multiple peers should complete"
    );

    // Verify content
    let file_path = temp_dir.path().join("multi-peer");
    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(content, expected);
}

// =============================================================================
// State Transition Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_state_transitions() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("state-test", piece_length, 2);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );

    // Initial state should be Checking
    assert_eq!(
        downloader.state(),
        gosh_dl::torrent::TorrentState::Checking,
        "Initial state should be Checking"
    );

    // After start(), state should be Downloading (since no files exist)
    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    assert_eq!(
        downloader.state(),
        gosh_dl::torrent::TorrentState::Downloading,
        "State should be Downloading after start()"
    );

    // Run peer loop until complete
    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if downloader.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(downloader.is_complete(), "Download should complete");

    // After completion, state should be Seeding (or Stopped if seed_ratio=0)
    let final_state = downloader.state();
    assert!(
        final_state == gosh_dl::torrent::TorrentState::Seeding
            || final_state == gosh_dl::torrent::TorrentState::Stopped,
        "Final state should be Seeding or Stopped, got {:?}",
        final_state
    );

    downloader.pause();
    handle.abort();
}

// =============================================================================
// Pause/Resume Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_pause() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("pause-test", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    // Start peer loop
    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    // Let it start downloading
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Pause
    downloader.pause();
    assert_eq!(
        downloader.state(),
        gosh_dl::torrent::TorrentState::Paused,
        "Should be paused"
    );

    // The peer loop should exit due to Paused state
    let _ = timeout(Duration::from_secs(5), handle).await;

    // Resume
    downloader.resume();
    let state = downloader.state();
    assert!(
        state == gosh_dl::torrent::TorrentState::Downloading
            || state == gosh_dl::torrent::TorrentState::Seeding,
        "Should resume to Downloading or Seeding, got {:?}",
        state
    );
}

// =============================================================================
// Piece Manager Tests
// =============================================================================

#[tokio::test]
async fn test_piece_manager_verify_existing() {
    let piece_length = 16384;
    let num_pieces = 2;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("verify-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();

    // Write piece data to disk as if it was already downloaded.
    // PieceManager expects the file at save_dir/metainfo.info.name.
    let file_path = temp_dir.path().join(&metainfo.info.name);
    let full_content: Vec<u8> = piece_data.iter().flatten().copied().collect();
    tokio::fs::write(&file_path, &full_content).await.unwrap();

    // Create PieceManager and verify
    let metainfo = Arc::new(metainfo);
    let pm = PieceManager::new(metainfo, temp_dir.path().to_path_buf());

    let valid_count = pm.verify_existing().await.unwrap();
    assert_eq!(valid_count, num_pieces, "Should verify all {} pieces", num_pieces);
    assert!(pm.is_complete(), "Should be complete after verifying all pieces");
}

#[tokio::test]
async fn test_piece_manager_detects_corrupt_data() {
    let piece_length = 16384;
    let num_pieces = 2;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("corrupt-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();

    // Write corrupt data (flip some bytes in piece 1)
    let mut corrupt_content: Vec<u8> = piece_data.iter().flatten().copied().collect();
    let midpoint = piece_length + piece_length / 2;
    corrupt_content[midpoint] ^= 0xFF; // Flip a byte in piece 1

    let file_path = temp_dir.path().join(&metainfo.info.name);
    tokio::fs::write(&file_path, &corrupt_content).await.unwrap();

    let metainfo = Arc::new(metainfo);
    let pm = PieceManager::new(metainfo, temp_dir.path().to_path_buf());

    let valid_count = pm.verify_existing().await.unwrap();
    assert_eq!(
        valid_count, 1,
        "Should verify only piece 0 (piece 1 is corrupt)"
    );
    assert!(
        !pm.is_complete(),
        "Should not be complete with corrupt piece"
    );
}

// =============================================================================
// Event System Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_events() {
    let piece_length = 16384;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("events-test", piece_length, 1);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, mut event_rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx.clone(),
        )
        .unwrap(),
    );

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    // Manually emit a Started event (the engine normally does this)
    let _ = event_tx.send(DownloadEvent::Started { id });

    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    // Collect events
    let mut received_started = false;
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        match timeout(Duration::from_millis(100), event_rx.recv()).await {
            Ok(Ok(DownloadEvent::Started { id: eid })) if eid == id => {
                received_started = true;
            }
            _ => {}
        }
        if downloader.is_complete() {
            break;
        }
    }

    assert!(received_started, "Should receive Started event");
    assert!(downloader.is_complete(), "Download should complete");

    downloader.pause();
    handle.abort();
}

// =============================================================================
// Engine-Level Torrent Tests
// =============================================================================

#[tokio::test]
async fn test_engine_add_torrent_creates_status() {
    let temp_dir = TempDir::new().unwrap();
    let piece_length = 16384;
    let (torrent_data, _metainfo, _piece_data) =
        build_test_torrent("engine-test", piece_length, 2);

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        max_concurrent_downloads: 4,
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config).await.unwrap();

    let id = engine
        .add_torrent(&torrent_data, DownloadOptions::default())
        .await
        .expect("Should accept valid torrent data");

    // Check status was created
    let status = engine.status(id);
    assert!(status.is_some(), "Should have status after add_torrent");

    let status = status.unwrap();
    assert_eq!(status.kind, gosh_dl::DownloadKind::Torrent);
    assert!(
        status.torrent_info.is_some(),
        "Should have torrent info in status"
    );

    // Verify the torrent info includes correct metadata
    let torrent_info = status.torrent_info.unwrap();
    assert_eq!(torrent_info.pieces_count, 2);

    // Verify download is listed
    let list = engine.list();
    assert!(
        list.iter().any(|s| s.id == id),
        "Download should appear in list"
    );

    engine.shutdown().await.ok();
}

#[tokio::test]
async fn test_engine_add_torrent_with_options() {
    let temp_dir = TempDir::new().unwrap();
    // Build a multi-file torrent
    let (torrent_data, _) = TestTorrentBuilder::multi_file("options-test").build();

    let config = EngineConfig {
        download_dir: temp_dir.path().to_path_buf(),
        enable_dht: false,
        enable_pex: false,
        enable_lpd: false,
        ..Default::default()
    };
    let engine = DownloadEngine::new(config).await.unwrap();

    // Add with sequential mode
    let options = DownloadOptions {
        sequential: Some(true),
        ..Default::default()
    };

    let id = engine
        .add_torrent(&torrent_data, options)
        .await
        .expect("Should add torrent with options");

    let status = engine.status(id).expect("Should have status");
    assert_eq!(status.kind, gosh_dl::DownloadKind::Torrent);

    engine.shutdown().await.ok();
}

// =============================================================================
// Progress Tracking Tests
// =============================================================================

#[tokio::test]
async fn test_torrent_progress_tracking() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("progress-test", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let id = DownloadId::new();
    let config = test_torrent_config();
    let (event_tx, _rx) = broadcast::channel(64);

    let mut metainfo = metainfo;
    metainfo.announce = None;
    metainfo.announce_list = vec![];

    let downloader = Arc::new(
        TorrentDownloader::from_torrent(
            id,
            metainfo,
            temp_dir.path().to_path_buf(),
            config,
            event_tx,
        )
        .unwrap(),
    );

    // Before start: progress should be empty
    let progress = downloader.progress();
    assert_eq!(progress.completed_size, 0, "Should start with 0 completed");

    let mock_peer = create_seeder(info_hash, &piece_data).await;
    downloader.add_known_peers(std::iter::once(mock_peer.addr()));

    Arc::clone(&downloader).start().await.unwrap();

    // After start with no existing files: completed should still be 0
    let progress = downloader.progress();
    assert_eq!(
        progress.total_size,
        Some((piece_length * num_pieces) as u64),
        "Total size should be set"
    );

    // Run download
    let dl = Arc::clone(&downloader);
    let handle = tokio::spawn(async move {
        dl.run_peer_loop().await.ok();
    });

    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_secs(30) {
        if downloader.is_complete() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(downloader.is_complete(), "Download should complete");

    downloader.pause();
    handle.abort();
}

// =============================================================================
// Larger Download Tests
// =============================================================================

#[tokio::test]
async fn test_download_multi_block_pieces() {
    // Each piece = 2 blocks (32768 bytes)
    let piece_length = 16384 * 2; // 32KB
    let num_pieces = 3;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("multi-block", piece_length, num_pieces);
    let info_hash = metainfo.info_hash;

    let temp_dir = TempDir::new().unwrap();
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader =
        run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(
        downloader.is_complete(),
        "Multi-block piece download should complete"
    );

    let file_path = temp_dir.path().join("multi-block");
    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(content.len(), expected.len());
    assert_eq!(content, expected);
}

// =============================================================================
// Resume Tests
// =============================================================================

#[tokio::test]
async fn test_resume_partial_download() {
    let piece_length = 16384;
    let num_pieces = 4;
    let (_torrent_data, metainfo, piece_data) =
        build_test_torrent("resume-test", piece_length, num_pieces);

    let temp_dir = TempDir::new().unwrap();

    // Pre-write the first 2 pieces (simulating a partial download)
    let file_path = temp_dir.path().join(&metainfo.info.name);
    let mut partial_content = piece_data[0].clone();
    partial_content.extend_from_slice(&piece_data[1]);
    // Pad to full size so piece manager can verify piece boundaries
    partial_content.resize(piece_length * num_pieces, 0);
    tokio::fs::write(&file_path, &partial_content).await.unwrap();

    // Create PieceManager and verify — should find 2 valid pieces
    let metainfo_arc = Arc::new(metainfo.clone());
    let pm = PieceManager::new(metainfo_arc, temp_dir.path().to_path_buf());
    let valid = pm.verify_existing().await.unwrap();
    assert_eq!(valid, 2, "Should verify 2 pre-existing pieces");

    // Now do a full download to get the remaining pieces
    let info_hash = metainfo.info_hash;
    let mock_peer = create_seeder(info_hash, &piece_data).await;

    let downloader = run_torrent_download(metainfo, &[mock_peer.addr()], temp_dir.path(), 30).await;

    assert!(downloader.is_complete(), "Resumed download should complete");

    // Verify complete file
    let content = tokio::fs::read(&file_path).await.unwrap();
    let expected: Vec<u8> = piece_data.into_iter().flatten().collect();
    assert_eq!(content, expected, "Final content should match");
}
