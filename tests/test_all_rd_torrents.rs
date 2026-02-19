use debridmoviemapper::rd_client::RealDebridClient;
use debridmoviemapper::tmdb_client::TmdbClient;
use debridmoviemapper::identification::identify_torrent;

#[tokio::test]
#[ignore]
async fn test_all_real_debrid_torrents() {
    dotenvy::dotenv().ok();

    let rd_token = std::env::var("RD_API_TOKEN").expect("RD_API_TOKEN must be set");
    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");

    let rd_client = RealDebridClient::new(rd_token);
    let tmdb_client = TmdbClient::new(tmdb_api_key);
    
    println!("\n========================================");
    println!("TESTING ALL REAL DEBRID TORRENTS");
    println!("========================================\n");
    
    // Fetch all torrents
    let torrents = match rd_client.get_torrents().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to fetch torrents: {}", e);
            return;
        }
    };
    
    println!("Found {} torrents in Real Debrid\n", torrents.len());
    
    let mut results = Vec::new();
    
    for (i, torrent_summary) in torrents.iter().enumerate() {
        // Get detailed info
        let torrent_info = match rd_client.get_torrent_info(&torrent_summary.id).await {
            Ok(info) => info,
            Err(e) => {
                println!("[{}/{}] ❌ Failed to get info for {}: {}", 
                    i+1, torrents.len(), torrent_summary.filename, e);
                continue;
            }
        };
        
        // Identify
        let metadata = identify_torrent(&torrent_info, &tmdb_client).await;
        
        let status = if metadata.external_id.is_some() { "✓" } else { "✗" };
        
        println!("[{}/{}] {} {}", 
            i+1, torrents.len(), status, torrent_info.filename);
        println!("       → {} ({:?}) [{}]",
            metadata.title,
            metadata.year,
            metadata.external_id.as_ref().unwrap_or(&"UNIDENTIFIED".to_string())
        );
        
        results.push((
            torrent_info.filename.clone(),
            metadata.title.clone(),
            metadata.year.clone(),
            metadata.external_id.clone(),
            metadata.media_type
        ));
        
        // Small delay to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    
    println!("\n========================================");
    println!("SUMMARY");
    println!("========================================");
    
    let total = results.len();
    let identified = results.iter().filter(|(_, _, _, id, _)| id.is_some()).count();
    let unidentified = total - identified;
    
    println!("Total: {}", total);
    println!("Identified: {} ({:.1}%)", identified, (identified as f64 / total as f64) * 100.0);
    println!("Unidentified: {} ({:.1}%)", unidentified, (unidentified as f64 / total as f64) * 100.0);
    
    println!("\n========================================");
    println!("UNIDENTIFIED TORRENTS");
    println!("========================================");
    
    for (filename, title, year, id, _) in &results {
        if id.is_none() {
            println!("• {} → {} ({:?})", filename, title, year);
        }
    }
    
    println!("\n========================================");
    println!("IDENTIFIED TORRENTS");
    println!("========================================");
    
    for (filename, title, year, id, media_type) in &results {
        if id.is_some() {
            println!("• {} → {} ({:?}) [{:?}] [{}]", 
                filename, title, year, media_type, id.as_ref().unwrap());
        }
    }
}
