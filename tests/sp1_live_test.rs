//! SP1 live smoke tests. `#[ignore]`; require real tokens in `.env`.
use debridmoviemapper::provider::{choose_provider, ProviderKind};
use debridmoviemapper::scraper::{MediaKind, Scraper, TorrentioScraper};

fn provider_from_env() -> Option<(ProviderKind, String)> {
    dotenvy::dotenv().ok();
    choose_provider(std::env::var("RD_API_TOKEN").ok(), std::env::var("TORBOX_API_KEY").ok()).ok()
}

#[tokio::test]
#[ignore]
async fn scraper_live_returns_parseable_streams() {
    let Some((kind, token)) = provider_from_env() else {
        eprintln!("skipping: no provider token");
        return;
    };
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap();
    let scraper = TorrentioScraper::new(
        std::env::var("SCRAPER_ADDON_URL").ok(),
        kind,
        &token,
        http,
    );
    // Sintel (Creative Commons): tt1727587
    let cands = scraper
        .find("tt1727587", MediaKind::Movie, None, None)
        .await
        .expect("scrape");
    assert!(
        !cands.is_empty(),
        "expected at least one stream for Sintel — check the Torrentio URL/option format"
    );
    assert!(cands.iter().all(|c| !c.info_hash.is_empty()));
    eprintln!(
        "scraper_live: {} candidates; first cached flag: {:?}",
        cands.len(),
        debridmoviemapper::release::parse(&cands[0]).cached
    );
}
