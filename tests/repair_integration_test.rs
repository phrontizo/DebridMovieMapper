//! Live Real-Debrid repair integration tests (`#[ignore]`; require `RD_API_TOKEN`, skip without it).
//!
//! `test_instant_repair_of_sintel` MODIFIES the live account: it adds Blender's Creative-Commons
//! *Sintel*, repairs it, and deletes every Sintel copy afterwards (also when it fails). It never
//! repairs one of the account's own torrents: a Real-Debrid repair re-adds the hash under a new id
//! and deletes the original, which would churn a library torrent or, if it can't re-cache, lose
//! it. `test_broken_torrents_hidden_from_webdav` only reads the account.

mod common;

use common::{cleanup_by_hash, find_ids_by_hash, with_cleanup, SINTEL_HASH};
use debridmoviemapper::acquire::locator_for;
use debridmoviemapper::provider::DebridProvider;
use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::repair::{RepairFailure, RepairManager};
use debridmoviemapper::vfs::is_video_file;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// The Real-Debrid provider, or `None` (after printing a skip line for `test`) when `RD_API_TOKEN`
/// is unset or blank. A set token that the provider then rejects is a real failure, not a skip.
fn rd_provider(test: &str) -> Option<Arc<dyn DebridProvider>> {
    let Some(rd) = common::token("RD_API_TOKEN") else {
        println!("skipping {test}: RD_API_TOKEN not set");
        return None;
    };
    Some(Arc::new(RealDebridClient::new(rd).unwrap()))
}

/// What a CDN 503 does on the read path, on a torrent this test owns: the torrent is marked broken
/// (hidden from WebDAV), then repaired at once (a first repair has no cooldown), and the repair
/// hands back a locator for the same file in the re-added torrent.
#[tokio::test]
#[ignore]
async fn test_instant_repair_of_sintel() {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(provider) = rd_provider("test_instant_repair_of_sintel") else {
        return;
    };
    // Delete any Sintel left by an interrupted run; afterwards delete the original and the
    // replacement the repair adds (both carry the Sintel hash).
    cleanup_by_hash(&provider, SINTEL_HASH).await;
    with_cleanup(
        repair_sintel(&provider),
        cleanup_by_hash(&provider, SINTEL_HASH),
    )
    .await;
}

async fn repair_sintel(provider: &Arc<dyn DebridProvider>) {
    let info = common::add_sintel_downloaded(provider, "real-debrid").await;
    let video = info
        .files
        .iter()
        .find(|f| f.selected == 1 && is_video_file(&f.path))
        .expect("Sintel should have a selected video file");
    let locator = locator_for(&info, &info.hash, &video.path);
    let repair_manager = RepairManager::new(provider.clone());

    println!("\n=== Mark Broken ===");
    repair_manager.mark_broken(&info.id).await;
    assert!(
        repair_manager.should_hide_torrent(&info.id).await,
        "a broken torrent should be hidden from WebDAV"
    );
    println!("  ✓ Torrent marked broken and hidden");

    println!("\n=== Instant Repair ===");
    match repair_manager.try_instant_repair(&locator).await {
        Ok(new_locator) => {
            assert_eq!(
                new_locator.file_path, locator.file_path,
                "the repair should hand back a locator for the same file"
            );
            assert!(
                new_locator.hash.eq_ignore_ascii_case(SINTEL_HASH),
                "the repaired locator should carry Sintel's hash, got {}",
                new_locator.hash
            );
            assert!(
                !repair_manager
                    .should_hide_torrent(&new_locator.torrent_id)
                    .await,
                "the repaired torrent should be visible"
            );
            let listed = find_ids_by_hash(provider, SINTEL_HASH)
                .await
                .unwrap_or_else(|e| panic!("listing the account failed: {e}"));
            assert!(
                listed.contains(&new_locator.torrent_id),
                "the repaired torrent {} should be listed (listed Sintel ids: {listed:?})",
                new_locator.torrent_id
            );
            if new_locator.torrent_id != info.id {
                // Real-Debrid re-adds under a new id; the repair deletes the original.
                let mut gone = false;
                for _ in 0..10 {
                    // A failed listing proves nothing either way, so it doesn't count as gone.
                    if matches!(
                        find_ids_by_hash(provider, SINTEL_HASH).await,
                        Ok(ids) if !ids.contains(&info.id)
                    ) {
                        gone = true;
                        break;
                    }
                    sleep(Duration::from_secs(1)).await;
                }
                assert!(
                    gone,
                    "the repair should delete the original torrent {}",
                    info.id
                );
            }
            println!("  ✓ Repaired: {} -> {}", info.id, new_locator.torrent_id);
        }
        // The re-added torrent hadn't settled (or cached) within repair's short window. Neither is
        // a failure of the repair path: the budget is reset and a later read retries.
        Err(e @ (RepairFailure::NotCached(_) | RepairFailure::Deferred(_))) => {
            println!("  Repair deferred: {e}")
        }
        Err(RepairFailure::Failed(e)) => panic!("instant repair of Sintel failed: {e}"),
    }

    println!("\n=== Status Summary ===");
    let (healthy, repairing, failed) = repair_manager.get_status_summary().await;
    println!(
        "  Healthy: {}, Repairing/Broken: {}, Failed: {}",
        healthy, repairing, failed
    );
    assert_eq!(failed, 0, "one repair should never mark a torrent Failed");
}

#[tokio::test]
#[ignore]
async fn test_broken_torrents_hidden_from_webdav() {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(rd_client) = rd_provider("test_broken_torrents_hidden_from_webdav") else {
        return;
    };
    // Marking is in-memory only: this test never changes the account.
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));

    println!("\n=== Test: Broken Torrents Hidden from WebDAV ===");

    let torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents");

    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(3)
        .collect();

    if downloaded.len() < 2 {
        println!("Need at least 2 torrents, skipping test.");
        return;
    }

    // Verify torrents are initially not hidden
    for torrent in downloaded.iter().take(2) {
        let should_hide = repair_manager.should_hide_torrent(&torrent.id).await;
        assert!(!should_hide, "Torrent should not be hidden initially");
    }
    println!("  ✓ Torrents initially visible in WebDAV");

    // Mark the first two as broken (skip any whose info is unavailable).
    let mut marked_ids = Vec::new();
    for torrent in &downloaded {
        if marked_ids.len() >= 2 {
            break;
        }
        let info = match rd_client.get_torrent_info(&torrent.id).await {
            Ok(i) => i,
            Err(e) => {
                println!("Skipping torrent {} (info unavailable: {})", torrent.id, e);
                continue;
            }
        };

        println!(
            "Marking torrent {} as broken: {}",
            marked_ids.len() + 1,
            info.filename
        );
        repair_manager.mark_broken(&torrent.id).await;
        marked_ids.push(torrent.id.clone());
    }

    if marked_ids.len() < 2 {
        println!("Could not find 2 accessible torrents to mark, skipping test.");
        return;
    }

    sleep(Duration::from_millis(500)).await;

    // Verify they are now hidden
    let mut hidden_count = 0;
    for id in &marked_ids {
        let should_hide = repair_manager.should_hide_torrent(id).await;
        if should_hide {
            hidden_count += 1;
            println!("  ✓ Torrent {} is hidden from WebDAV", id);
        }
    }

    assert!(
        hidden_count >= 2,
        "Should have at least 2 hidden torrents, found {}",
        hidden_count
    );
    println!(
        "\n  ✓ Successfully verified {} broken torrents are hidden",
        hidden_count
    );

    println!("\n=== Test Passed: Broken Torrents Hidden ===");
}
