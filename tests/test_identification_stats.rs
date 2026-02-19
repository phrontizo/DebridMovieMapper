use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::identification::identify_torrent;

#[tokio::test]
#[ignore]
async fn test_identification_statistics() {
    dotenvy::dotenv().ok();

    let rd_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set");
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");

    let rd_client = RealDebridClient::new(rd_token);
    let tmdb_client = TmdbClient::new(tmdb_api_key);

    println!("\n========================================");
    println!("IDENTIFICATION STATISTICS");
    println!("========================================\n");

    // Fetch all torrents
    let torrents = match rd_client.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch torrents: {}", e);
            return;
        }
    };

    println!("Total torrents: {}\n", torrents.len());

    let mut identified = 0;
    let mut unidentified = Vec::new();

    for (i, torrent_summary) in torrents.iter().enumerate() {
        if (i + 1) % 50 == 0 {
            println!("Progress: {}/{}", i + 1, torrents.len());
        }

        // Get detailed info
        let torrent_info = match rd_client.get_torrent_info(&torrent_summary.id).await {
            Ok(info) => info,
            Err(e) => {
                println!("Failed to get info for {}: {}", torrent_summary.filename, e);
                continue;
            }
        };

        // Identify
        let metadata = identify_torrent(&torrent_info, &tmdb_client).await;

        if metadata.external_id.is_some() {
            identified += 1;
        } else {
            unidentified.push(torrent_info.filename.clone());
        }

        // Small delay to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    println!("\n========================================");
    println!("RESULTS");
    println!("========================================");
    println!("Total: {}", torrents.len());
    println!("Identified: {} ({:.1}%)", identified, (identified as f64 / torrents.len() as f64) * 100.0);
    println!("Unidentified: {} ({:.1}%)", unidentified.len(), (unidentified.len() as f64 / torrents.len() as f64) * 100.0);

    if !unidentified.is_empty() {
        println!("\n========================================");
        println!("UNIDENTIFIED TORRENTS");
        println!("========================================");
        for filename in &unidentified {
            println!("• {}", filename);
        }
    } else {
        println!("\n🎉 100% IDENTIFICATION ACHIEVED! 🎉");
    }
}
