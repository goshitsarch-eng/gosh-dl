use gosh_dl::{DownloadEngine, DownloadEvent, DownloadOptions, EngineConfig};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const MAGNET_URI: &str = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny&tr=udp%3A%2F%2Fexplodie.org%3A6969&tr=udp%3A%2F%2Ftracker.coppersurfer.tk%3A6969&tr=udp%3A%2F%2Ftracker.empire-js.us%3A1337&tr=udp%3A%2F%2Ftracker.leechers-paradise.org%3A6969&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=wss%3A%2F%2Ftracker.btorrent.xyz&tr=wss%3A%2F%2Ftracker.fastcast.nz&tr=wss%3A%2F%2Ftracker.openwebtorrent.com&ws=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2F&xs=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2Fbig-buck-bunny.torrent";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let download_dir = std::env::var("GOSH_DL_TEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("gosh-dl-magnet-smoke"));
    std::fs::create_dir_all(&download_dir)?;

    let config = EngineConfig {
        download_dir,
        ..Default::default()
    };

    let engine = DownloadEngine::new(config).await?;
    let mut events = engine.subscribe();

    let id = engine
        .add_magnet(MAGNET_URI, DownloadOptions::default())
        .await?;

    let start = Instant::now();
    let mut ticker = tokio::time::interval(Duration::from_secs(2));

    loop {
        if start.elapsed() > Duration::from_secs(90) {
            println!("Timed out waiting for magnet activity.");
            break;
        }

        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(evt) => {
                        println!("event: {evt:?}");
                        if matches!(evt, DownloadEvent::Completed { .. } | DownloadEvent::Failed { .. }) {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            _ = ticker.tick() => {
                if let Some(status) = engine.status(id) {
                    println!("status: {:?}", status.state);
                }
            }
        }
    }

    engine.shutdown().await.ok();
    Ok(())
}
