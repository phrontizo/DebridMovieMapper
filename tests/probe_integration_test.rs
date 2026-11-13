//! Live probe integration test (`#[ignore]`; requires API tokens).
//!
//! Guards the CDN range-response handling: `probe_tracks` must read the front window and parse a
//! cached file's tracks against BOTH providers' CDNs. The two behave differently on a ranged GET —
//! **Real-Debrid replies `206 Partial Content`**, **TorBox replies `200 OK`** (it ignores the
//! `Range` header and streams from byte 0). Before the fix the probe required `206` and mapped
//! TorBox's `200` to `Transient`, so every TorBox probe deferred forever; now it accepts a `200`
//! for the offset-0 front read (mirroring `dav_fs::fetch_cdn_range`, which always handled it).
//!
//! Uses the Creative-Commons *Sintel* torrent (cached on both services), and cleans up after itself,
//! also when a step fails. Each sub-test skips cleanly if its token (`RD_API_TOKEN` /
//! `TORBOX_API_KEY`) is unset.

mod common;

use common::{cleanup_by_hash, sintel_magnet, with_cleanup, SINTEL_HASH};
use debridmoviemapper::acquire::locator_for;
use debridmoviemapper::probe::{probe_tracks, ProbeError, Track};
use debridmoviemapper::provider::DebridProvider;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::torbox_client::TorBoxClient;
use debridmoviemapper::vfs::is_video_file;
use std::sync::Arc;
use std::time::Duration;

/// Add Sintel, resolve its video file's CDN url through the real provider path, probe it, clean up
/// (also when Sintel never resolves).
async fn run_probe(provider: Arc<dyn DebridProvider>, label: &str) {
    cleanup_by_hash(&provider, SINTEL_HASH).await;
    let outcome = with_cleanup(
        probe_sintel(&provider, label),
        cleanup_by_hash(&provider, SINTEL_HASH),
    )
    .await;
    assert!(
        !matches!(outcome, Err(ProbeError::Transient)),
        "[{label}] probe deferred (Transient) — the CDN's range response was NOT read; \
         this is exactly the bug the 200-handling fix addresses"
    );
    match outcome {
        Ok(tracks) => println!("[{label}] probe OK — {} track(s) parsed", tracks.len()),
        Err(e) => {
            println!("[{label}] probe reached a non-deferring verdict: {e:?} (fetch succeeded)")
        }
    }
}

/// Add Sintel, wait until its video file resolves to a CDN url, and probe that url.
async fn probe_sintel(
    provider: &Arc<dyn DebridProvider>,
    label: &str,
) -> Result<Vec<Track>, ProbeError> {
    let id = provider
        .add_magnet(&sintel_magnet())
        .await
        .expect("add_magnet")
        .id;
    println!("[{label}] add_magnet -> id={id}");

    // Poll until the file list resolves, the torrent is downloaded, and the locator resolves to a
    // CDN url (RD: needs the per-file link to populate after selection; TorBox: cached/instant).
    let mut resolved: Option<String> = None;
    for _ in 0..40 {
        if let Ok(info) = provider.get_torrent_info(&id).await {
            let video = info.files.iter().find(|f| is_video_file(&f.path)).cloned();
            if let Some(video) = video {
                // Select all video files (RD needs this to download + mint links; TorBox no-op).
                let ids = info
                    .files
                    .iter()
                    .filter(|f| is_video_file(&f.path))
                    .map(|f| f.id.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let _ = provider.select_files(&id, &ids).await;
                if info.status == "downloaded" {
                    let locator = locator_for(&info, SINTEL_HASH, &video.path);
                    if let Ok(url) = provider.resolve_url(&locator).await {
                        resolved = Some(url);
                        break;
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }

    let url = resolved
        .unwrap_or_else(|| panic!("[{label}] Sintel never reached a resolvable downloaded video"));

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap();
    // The crux: the probe must READ the CDN response (RD 206 / TorBox 200) and reach a verdict —
    // it must NOT defer with `Transient`. (`Transient` is precisely what the old code returned for
    // TorBox's Range-ignoring 200.) Any other outcome — tracks parsed, or an accept-verdict like
    // TracksNotFound/Unsupported when the header isn't in the front window — means the fetch worked.
    probe_tracks(&http, &url).await
}

#[tokio::test]
#[ignore]
async fn probe_works_through_real_debrid_cdn() {
    let Some(rd) = common::token("RD_API_TOKEN") else {
        println!("skipping probe_works_through_real_debrid_cdn: RD_API_TOKEN not set");
        return;
    };
    run_probe(Arc::new(RealDebridClient::new(rd).unwrap()), "RD").await;
}

#[tokio::test]
#[ignore]
async fn probe_works_through_torbox_cdn() {
    let Some(tb) = common::token("TORBOX_API_KEY") else {
        println!("skipping probe_works_through_torbox_cdn: TORBOX_API_KEY not set");
        return;
    };
    run_probe(Arc::new(TorBoxClient::new(tb).unwrap()), "TorBox").await;
}
