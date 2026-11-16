//! Live repair integration tests (`#[ignore]`), run against every configured provider: Real-Debrid
//! when `RD_API_TOKEN` is set and TorBox when `TORBOX_API_KEY` is set. Each provider's tests skip
//! when its token is unset or blank.
//!
//! `test_instant_repair_of_sintel_*` MODIFIES the live account: it adds Blender's Creative-Commons
//! *Sintel*, repairs it, and deletes every Sintel copy afterwards (also when it fails). A
//! Real-Debrid repair re-adds the hash under a new id and deletes the original. On TorBox a re-add
//! of a hash already in the account returns the existing torrent (the client recovers it by hash),
//! so there the repair keeps the id and deletes nothing. The test never repairs one of the
//! account's own torrents: on Real-Debrid that would churn a library torrent or, if it can't
//! re-cache, lose it.
//! `test_broken_torrents_hidden_from_webdav_*` only reads the account: its provider calls go
//! through `common::ReadOnlyProvider`.
//!
//! Run with: `cargo test --test repair_integration_test -- --ignored --show-output`
//! (needs `RD_API_TOKEN` and/or `TORBOX_API_KEY` in `.env`). libtest captures a passing test's
//! stderr, and `--show-output` prints it, so a WARNING from the cleanup (`common::cleanup_added`)
//! that a Sintel copy may be left in the account is seen.

mod common;

use common::{
    cleanup_added, cleanup_by_hash, find_ids_by_hash, label, poll_until_listed, provider,
    record_added_id, with_cleanup, AddedIds, INFO_NOT_FOUND_BACKOFF, SINTEL_HASH,
};
use debridmoviemapper::acquire::locator_for;
use debridmoviemapper::provider::{torrent_info_retrying_not_found, DebridProvider, ProviderKind};
use debridmoviemapper::repair::{RepairFailure, RepairManager};
use debridmoviemapper::vfs::is_video_file;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

/// Poll the listing (every 2 s for up to 30 s, since TorBox's can lag an add) until it shows the
/// torrent `id`, and fail the test with what the polls saw if it never does.
async fn assert_listed(provider: &Arc<dyn DebridProvider>, id: &str, what: &str, label: &str) {
    let listed_as_id = move || async move {
        find_ids_by_hash(provider, SINTEL_HASH)
            .await
            .map(|ids| ids.into_iter().filter(|i| i == id).collect::<Vec<_>>())
    };
    if let Err(why) = poll_until_listed(listed_as_id, 16, Duration::from_secs(2)).await {
        panic!("[{label}] {what} {id} should be listed: {why}");
    }
}

#[tokio::test]
#[ignore]
async fn test_instant_repair_of_sintel_real_debrid() {
    run_instant_repair(ProviderKind::RealDebrid).await;
}

#[tokio::test]
#[ignore]
async fn test_instant_repair_of_sintel_torbox() {
    run_instant_repair(ProviderKind::TorBox).await;
}

/// What a CDN 503 does on the read path, on a torrent this test owns: the torrent is marked broken
/// (hidden from WebDAV), then repaired at once (a first repair has no cooldown), and the repair
/// hands back a locator for the same file in the re-added torrent.
async fn run_instant_repair(kind: ProviderKind) {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(provider) = provider(kind, "test_instant_repair_of_sintel") else {
        return;
    };
    // Delete any Sintel left by an interrupted run; afterwards delete the original and any
    // replacement the repair adds (both carry the Sintel hash), by id first and then by hash.
    cleanup_by_hash(&provider, SINTEL_HASH).await;
    let added = AddedIds::default();
    with_cleanup(
        repair_sintel(&provider, &added, label(kind)),
        cleanup_added(&provider, &added, SINTEL_HASH),
    )
    .await;
}

async fn repair_sintel(provider: &Arc<dyn DebridProvider>, added: &AddedIds, label: &str) {
    let info = common::add_sintel_downloaded(provider, added, label).await;
    // A library torrent is listed before anything reads it. On TorBox the repair's re-add finds the
    // existing torrent through the listing, so repairing before the listing shows it (TorBox's lags
    // an add) would only test that lag.
    assert_listed(provider, &info.id, "the added torrent", label).await;
    let video = info
        .files
        .iter()
        .find(|f| f.selected == 1 && is_video_file(&f.path))
        .expect("Sintel should have a selected video file");
    let locator = locator_for(&info, &info.hash, &video.path);
    let repair_manager = RepairManager::new(provider.clone());

    println!("\n=== [{label}] Mark Broken ===");
    repair_manager.mark_broken(&info.id).await;
    assert!(
        repair_manager.should_hide_torrent(&info.id).await,
        "[{label}] a broken torrent should be hidden from WebDAV"
    );
    println!("  ✓ Torrent marked broken and hidden");

    println!("\n=== [{label}] Instant Repair ===");
    match repair_manager.try_instant_repair(&locator).await {
        Ok(new_locator) => {
            record_added_id(added, &new_locator.torrent_id);
            assert_eq!(
                new_locator.file_path, locator.file_path,
                "[{label}] the repair should hand back a locator for the same file"
            );
            assert!(
                new_locator.hash.eq_ignore_ascii_case(SINTEL_HASH),
                "[{label}] the repaired locator should carry Sintel's hash, got {}",
                new_locator.hash
            );
            assert!(
                !repair_manager
                    .should_hide_torrent(&new_locator.torrent_id)
                    .await,
                "[{label}] the repaired torrent should be visible"
            );
            assert_listed(
                provider,
                &new_locator.torrent_id,
                "the repaired torrent",
                label,
            )
            .await;
            if new_locator.torrent_id == info.id {
                // Same-id re-add (TorBox recovers the existing torrent by hash): the torrent under
                // repair IS the repaired one, so the repair must not delete it. Give a stray delete
                // time to land, then look the torrent up (a deleted one keeps answering 404).
                sleep(Duration::from_secs(2)).await;
                let kept = torrent_info_retrying_not_found(
                    provider.as_ref(),
                    &info.id,
                    &INFO_NOT_FOUND_BACKOFF,
                )
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "[{label}] the torrent repaired in place ({}) should still exist: {e}",
                        info.id
                    )
                });
                assert_eq!(
                    kept.status, "downloaded",
                    "[{label}] the torrent repaired in place should stay downloaded"
                );
                println!("  ✓ Repaired in place: {} kept its id", info.id);
            } else {
                // New-id re-add (Real-Debrid): the repair deletes the original.
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
                    "[{label}] the repair should delete the original torrent {}",
                    info.id
                );
                common::forget_added_id(added, &info.id);
                println!("  ✓ Repaired: {} -> {}", info.id, new_locator.torrent_id);
            }
        }
        // The re-added torrent hadn't settled (or cached) within repair's short window. Neither is
        // a failure of the repair path: the budget is reset and a later read retries.
        Err(e @ (RepairFailure::NotCached(_) | RepairFailure::Deferred(_))) => {
            println!("  Repair deferred: {e}")
        }
        Err(RepairFailure::Failed(e)) => panic!("[{label}] instant repair of Sintel failed: {e}"),
    }

    println!("\n=== [{label}] Status Summary ===");
    let (healthy, repairing, failed) = repair_manager.get_status_summary().await;
    println!(
        "  Healthy: {}, Repairing/Broken: {}, Failed: {}",
        healthy, repairing, failed
    );
    assert_eq!(
        failed, 0,
        "[{label}] one repair should never mark a torrent Failed"
    );
}

#[tokio::test]
#[ignore]
async fn test_broken_torrents_hidden_from_webdav_real_debrid() {
    run_broken_torrents_hidden(ProviderKind::RealDebrid).await;
}

#[tokio::test]
#[ignore]
async fn test_broken_torrents_hidden_from_webdav_torbox() {
    run_broken_torrents_hidden(ProviderKind::TorBox).await;
}

async fn run_broken_torrents_hidden(kind: ProviderKind) {
    let _ = tracing_subscriber::fmt::try_init();
    let Some(client) = provider(kind, "test_broken_torrents_hidden_from_webdav") else {
        return;
    };
    let label = label(kind);
    // Marking is in-memory only, and `ReadOnlyProvider` refuses every call that would change the
    // account, so this test never changes it.
    let provider: Arc<dyn DebridProvider> = Arc::new(common::ReadOnlyProvider(client));
    let repair_manager = Arc::new(RepairManager::new(provider.clone()));

    println!("\n=== [{label}] Test: Broken Torrents Hidden from WebDAV ===");

    let torrents = provider
        .get_torrents()
        .await
        .expect("Failed to get torrents");

    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(3)
        .collect();

    if downloaded.len() < 2 {
        println!("[{label}] Need at least 2 torrents, skipping test.");
        return;
    }

    // Verify torrents are initially not hidden
    for torrent in downloaded.iter().take(2) {
        let should_hide = repair_manager.should_hide_torrent(&torrent.id).await;
        assert!(
            !should_hide,
            "[{label}] Torrent should not be hidden initially"
        );
    }
    println!("  ✓ Torrents initially visible in WebDAV");

    // Mark the first two as broken (skip any whose info is unavailable).
    let mut marked_ids = Vec::new();
    for torrent in &downloaded {
        if marked_ids.len() >= 2 {
            break;
        }
        let info = match torrent_info_retrying_not_found(
            provider.as_ref(),
            &torrent.id,
            &INFO_NOT_FOUND_BACKOFF,
        )
        .await
        {
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
        println!("[{label}] Could not find 2 accessible torrents to mark, skipping test.");
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
        "[{label}] Should have at least 2 hidden torrents, found {}",
        hidden_count
    );
    println!(
        "\n  ✓ Successfully verified {} broken torrents are hidden",
        hidden_count
    );

    println!("\n=== [{label}] Test Passed: Broken Torrents Hidden ===");
}
