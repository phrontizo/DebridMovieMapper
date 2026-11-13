use debridmoviemapper::identification::identify_torrent;
use debridmoviemapper::rd_client::{TorrentFile, TorrentInfo};
use debridmoviemapper::tmdb_client::TmdbClient;

#[tokio::test]
#[ignore]
async fn test_short_title_identification() {
    dotenvy::dotenv().ok();

    let tmdb_api_key = std::env::var("TMDB_API_KEY").expect("TMDB_API_KEY must be set");
    let tmdb_client = TmdbClient::new(tmdb_api_key).unwrap();

    println!("\n========================================");
    println!("TESTING SHORT TITLE IDENTIFICATION");
    println!("========================================\n");

    // Test cases: short (≤3-letter) titles that need the short-title identification path.
    // NB: verify any new expected id against the live API (title + year) before hardcoding it:
    // earlier expected ids here were simply wrong (Us was 458156, which is John Wick: Chapter 3;
    // Don was 940721, Godzilla Minus One), and an id whose TMDB entry was deleted or merged (Amy
    // was 318034) stops resolving.
    let test_cases = vec![
        ("Us", 2019, 458723), // Us (2019) by Jordan Peele (verified live: tmdb:458723, 2019-03-14)
        ("Don", 2022, 895033), // Don (2022) - Tamil film (TMDB 895033)
        ("Ran", 1985, 11645), // Ran (1985) by Akira Kurosawa
        ("Amy", 2015, 331781), // Amy (2015) - Amy Winehouse documentary (verified live: tmdb:331781, 2015-07-02)
    ];

    let mut failures: Vec<String> = Vec::new();

    for (title, year, expected_tmdb_id) in test_cases {
        println!("\n----------------------------------------");
        println!("Testing: {} ({})", title, year);
        println!("Expected TMDB ID: {}", expected_tmdb_id);
        println!("----------------------------------------");

        // Create a mock torrent info
        let info = TorrentInfo {
            id: "test_id".to_string(),
            filename: format!("{}.{}.1080p.BluRay.x264.mkv", title, year),
            original_filename: format!("{}.{}.1080p.BluRay.x264.mkv", title, year),
            hash: "hash".to_string(),
            bytes: 3000000000,
            original_bytes: 3000000000,
            host: "host".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2020-01-01".to_string(),
            files: vec![TorrentFile {
                id: 1,
                path: format!("{}.{}.1080p.BluRay.x264.mkv", title, year),
                bytes: 3000000000,
                selected: 1,
            }],
            links: vec!["http://link1".to_string()],
            ended: Some("2020-01-01".to_string()),
        };

        let metadata = identify_torrent(&info, &tmdb_client).await;

        println!("Result:");
        println!("  Title: {}", metadata.title);
        println!("  Year: {:?}", metadata.year);
        println!("  External ID: {:?}", metadata.external_id);
        println!("  Media Type: {:?}", metadata.media_type);

        let expected = format!("tmdb:{}", expected_tmdb_id);
        match metadata.external_id.as_deref() {
            Some(id) if id == expected => println!("  ✓ CORRECT"),
            Some(id) => {
                println!("  ✗ WRONG (expected {})", expected);
                failures.push(format!(
                    "{} ({}): got {}, expected {}",
                    title, year, id, expected
                ));
            }
            None => {
                println!("  ✗ UNIDENTIFIED");
                failures.push(format!(
                    "{} ({}): UNIDENTIFIED, expected {}",
                    title, year, expected
                ));
            }
        }

        // Small delay to avoid rate limiting
        tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;
    }

    assert!(
        failures.is_empty(),
        "Short-title identification regressed for {} case(s):\n  {}",
        failures.len(),
        failures.join("\n  ")
    );
}
