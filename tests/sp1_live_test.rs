//! SP1 live smoke tests. `#[ignore]`; require real tokens in `.env`.
use debridmoviemapper::provider::{choose_provider, ProviderKind};
use debridmoviemapper::scraper::{build_http_client, MediaKind, Scraper, TorrentioScraper};

fn provider_from_env() -> Option<(ProviderKind, String)> {
    dotenvy::dotenv().ok();
    choose_provider(
        std::env::var("RD_API_TOKEN").ok(),
        std::env::var("TORBOX_API_KEY").ok(),
    )
    .ok()
}

/// A single provider token (RD preferred) for tests that only need to template the Torrentio URL —
/// unlike `provider_from_env`, this works even when BOTH tokens are set (as in a cross-provider
/// `.env`), where `choose_provider` deliberately errors.
fn single_provider_from_env() -> Option<(ProviderKind, String)> {
    dotenvy::dotenv().ok();
    let non_empty = |k: &str| std::env::var(k).ok().filter(|s| !s.trim().is_empty());
    if let Some(t) = non_empty("RD_API_TOKEN") {
        Some((ProviderKind::RealDebrid, t))
    } else {
        non_empty("TORBOX_API_KEY").map(|t| (ProviderKind::TorBox, t))
    }
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
    let scraper =
        TorrentioScraper::new(std::env::var("SCRAPER_ADDON_URL").ok(), kind, &token, http);
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

/// Quick TCP reachability probe (3s) of a proxy URL's `host:port`, so the live proxy test can skip
/// its positive case cleanly when the proxy sits on a network this environment can't route to
/// (e.g. a LAN proxy run from CI / a sandbox).
async fn proxy_reachable(proxy_url: &str) -> bool {
    let after_scheme = proxy_url
        .split_once("://")
        .map(|(_, r)| r)
        .unwrap_or(proxy_url);
    let authority = after_scheme.split('/').next().unwrap_or(after_scheme);
    let hostport = authority.rsplit('@').next().unwrap_or(authority); // strip any user:pass@
    let Some((host, port)) = hostport.rsplit_once(':') else {
        return false;
    };
    let Ok(port) = port.parse::<u16>() else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::net::TcpStream::connect((host, port)),
        )
        .await,
        Ok(Ok(_))
    )
}

/// Live test for `SCRAPER_PROXY_URL` (the scraper-only HTTP proxy). Proves two things:
///  1. a dead proxy makes the scrape FAIL — the client routes through the proxy and never silently
///     falls back to a direct connection (which would succeed and leak the real IP), and
///  2. scraping succeeds THROUGH the real proxy and returns parseable candidates.
///
/// `#[ignore]`; requires a provider token. The negative control (1) always runs. The positive case
/// (2) runs only when `SCRAPER_PROXY_URL` is set — it is sourced from the git-ignored `.env`, so no
/// internal proxy address ever lives in the repo — AND the proxy is reachable from this environment
/// (it may be on a LAN that CI/a sandbox can't route to); otherwise it skips cleanly. The proxy URL
/// is never printed, so it can't leak into CI logs either.
#[tokio::test]
#[ignore]
async fn scraper_live_through_http_proxy() {
    let Some((kind, token)) = single_provider_from_env() else {
        eprintln!("skipping: no provider token");
        return;
    };

    // (1) Negative control: a dead proxy (nothing listens on 127.0.0.1:1). The scrape MUST fail —
    // if it succeeded, the client would be bypassing the proxy to a direct connection.
    let dead = build_http_client(Some("http://127.0.0.1:1")).expect("build dead-proxy client");
    let dead_res = TorrentioScraper::new(None, kind, &token, dead)
        .find("tt1727587", MediaKind::Movie, None, None)
        .await;
    assert!(
        dead_res.is_err(),
        "scraping through a dead proxy must fail (proves the proxy is honored, not bypassed); \
         got Ok with {:?} candidates",
        dead_res.map(|c| c.len())
    );

    // (2) Real proxy — from SCRAPER_PROXY_URL (.env), kept out of the repo. Skip cleanly if unset
    // or not routable from here. The URL is never echoed (no leak into logs).
    let Some(proxy_url) = std::env::var("SCRAPER_PROXY_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        eprintln!("skipping positive case: SCRAPER_PROXY_URL not set (put it in .env to run this)");
        return;
    };
    if !proxy_reachable(&proxy_url).await {
        eprintln!("skipping positive case: configured proxy not reachable from this environment");
        return;
    }
    let proxied = build_http_client(Some(&proxy_url))
        .unwrap_or_else(|e| panic!("build client for the configured proxy: {e}"));
    let cands = TorrentioScraper::new(
        std::env::var("SCRAPER_ADDON_URL").ok(),
        kind,
        &token,
        proxied,
    )
    .find("tt1727587", MediaKind::Movie, None, None)
    .await
    .unwrap_or_else(|e| panic!("scrape Sintel through the configured proxy: {e}"));
    assert!(
        !cands.is_empty(),
        "expected >=1 stream for Sintel through the configured proxy"
    );
    assert!(cands.iter().all(|c| !c.info_hash.is_empty()));
    eprintln!(
        "scraper_live_through_http_proxy: {} candidates via the configured proxy",
        cands.len()
    );
}
