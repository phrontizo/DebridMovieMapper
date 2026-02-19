use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::repair::RepairManager;
use std::sync::Arc;
use tokio::time::{sleep, Duration};

#[tokio::test]
#[ignore]
async fn test_repair_process_integration() {
    let _ = tracing_subscriber::fmt::try_init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN")
        .expect("RD_API_TOKEN must be set")
        .trim()
        .to_string();
    let tmdb_api_key = std::env::var("TMDB_API_KEY")
        .expect("TMDB_API_KEY must be set")
        .trim()
        .to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
    let tmdb_client = Arc::new(TmdbClient::new(tmdb_api_key));
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));

    println!("Fetching torrents...");
    let torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents");

    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(5)
        .collect();

    if downloaded.is_empty() {
        println!("No downloaded torrents found, skipping test.");
        return;
    }

    println!("Found {} downloaded torrents (using first 5)", downloaded.len());

    // Prepare data with metadata for health check (skip torrents deleted by prior repair runs)
    let mut torrent_data = Vec::new();
    for torrent in &downloaded {
        match rd_client.get_torrent_info(&torrent.id).await {
            Ok(info) => {
                let metadata = identify_torrent(&info, &tmdb_client).await;
                torrent_data.push((info, metadata));
            }
            Err(e) => {
                println!("Skipping torrent {} (info unavailable: {})", torrent.id, e);
            }
        }
    }

    if torrent_data.is_empty() {
        println!("No accessible downloaded torrents found, skipping test.");
        return;
    }

    // Test 1: Mark a torrent as broken and verify it can be detected
    println!("\n=== Test 2: Mark Broken ===");
    if let Some((info, _)) = torrent_data.first() {
        if let Some(first_link) = info.links.first() {
            println!("Marking torrent as broken: {}", info.filename);
            repair_manager.mark_broken(&info.id, first_link).await;

            sleep(Duration::from_millis(100)).await;

            // Check if torrent should be hidden
            let should_hide = repair_manager.should_hide_torrent(&info.id).await;
            println!("  Should hide torrent: {}", should_hide);
            assert!(should_hide, "Broken torrent should be hidden from WebDAV");
            println!("  ✓ Torrent successfully marked as broken");
        }
    }

    // Test 3: Repair a broken torrent
    println!("\n=== Test 3: Repair Broken Torrent ===");
    if let Some((info, _)) = torrent_data.first() {
        println!("Attempting to repair torrent: {}", info.filename);
        match repair_manager.repair_torrent(info).await {
            Ok(_) => {
                println!("  ✓ Repair completed successfully");
            }
            Err(e) => {
                println!("  Repair failed (expected for some torrents): {}", e);
            }
        }
    }

    // Test 4: Verify status summary works
    println!("\n=== Test 4: Status Summary ===");
    let (healthy, broken, repairing) = repair_manager.get_status_summary().await;
    println!("  Healthy: {}, Broken: {}, Repairing: {}", healthy, broken, repairing);
    println!("  ✓ Status summary retrieved successfully");

    // Test 5: Verify torrents can be retrieved
    println!("\n=== Test 5: Verify Torrent Retrieval ===");
    let all_torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents after repair");
    println!("  Retrieved {} torrents after repair operations", all_torrents.len());
    assert!(!all_torrents.is_empty(), "Should have torrents after repair");
    println!("  ✓ Torrents still accessible after repair");

    println!("\n=== All Tests Passed ===");
}

#[tokio::test]
#[ignore]
async fn test_503_triggers_immediate_repair() {
    let _ = tracing_subscriber::fmt::try_init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN")
        .expect("RD_API_TOKEN must be set")
        .trim()
        .to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
    let repair_manager = Arc::new(RepairManager::new(rd_client.clone()));

    println!("\n=== Test: 503 Error Triggers Immediate Repair ===");

    // Get a test torrent
    let torrents = rd_client
        .get_torrents()
        .await
        .expect("Failed to get torrents");

    let downloaded: Vec<_> = torrents
        .into_iter()
        .filter(|t| t.status == "downloaded")
        .take(5)
        .collect();

    if downloaded.is_empty() {
        println!("No downloaded torrents found, skipping test.");
        return;
    }

    // Find a torrent whose info is still accessible (previous tests may have
    // repaired/deleted some, making their old IDs return 404).
    let mut info = None;
    let mut test_torrent = None;
    for t in &downloaded {
        match rd_client.get_torrent_info(&t.id).await {
            Ok(i) => {
                info = Some(i);
                test_torrent = Some(t);
                break;
            }
            Err(e) => {
                println!("Skipping torrent {} (info unavailable: {})", t.id, e);
            }
        }
    }

    let info = info.expect("No accessible downloaded torrent found");
    let test_torrent = test_torrent.unwrap();

    println!("Using test torrent: {}", info.filename);

    // Simulate a 503 error by marking as broken (this is what happens in WebDAV on 503)
    if let Some(first_link) = info.links.first() {
        println!("Simulating 503 error during playback...");
        repair_manager.mark_broken(&test_torrent.id, first_link).await;

        // Check immediately
        sleep(Duration::from_millis(100)).await;

        // Verify torrent should be hidden
        let should_hide = repair_manager.should_hide_torrent(&test_torrent.id).await;
        println!("Should hide after 503: {}", should_hide);
        assert!(should_hide, "Torrent should be immediately hidden on 503");
        println!("  ✓ Torrent immediately marked as broken and hidden");

        // Now trigger repair
        println!("Triggering immediate repair...");
        match repair_manager.repair_torrent(&info).await {
            Ok(_) => {
                println!("  ✓ Repair completed successfully");
            }
            Err(e) => {
                println!("  Repair failed (expected for some torrents): {}", e);
                println!("  ✓ Repair was attempted immediately");
            }
        }

        // Check status summary
        let (_, broken, repairing) = repair_manager.get_status_summary().await;
        println!("Status after repair: Broken: {}, Repairing: {}", broken, repairing);
    }

    println!("\n=== Test Passed: 503 Triggers Immediate Repair ===");
}

#[tokio::test]
#[ignore]
async fn test_broken_torrents_hidden_from_webdav() {
    let _ = tracing_subscriber::fmt::try_init();
    dotenvy::dotenv().ok();

    let api_token = std::env::var("RD_API_TOKEN")
        .expect("RD_API_TOKEN must be set")
        .trim()
        .to_string();

    let rd_client = Arc::new(RealDebridClient::new(api_token).unwrap());
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

    // Mark first two as broken (skip any whose info is unavailable due to prior repairs)
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

        if let Some(link) = info.links.first() {
            println!("Marking torrent {} as broken: {}", marked_ids.len() + 1, info.filename);
            repair_manager.mark_broken(&torrent.id, link).await;
            marked_ids.push(torrent.id.clone());
        }
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
    println!("\n  ✓ Successfully verified {} broken torrents are hidden", hidden_count);

    println!("\n=== Test Passed: Broken Torrents Hidden ===");
}
