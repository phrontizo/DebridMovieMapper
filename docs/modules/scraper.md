# `scraper.rs`

`Scraper` trait + `TorrentioScraper` — calls a Stremio-compatible addon (default: Torrentio, auto-templated from the provider token) and parses stream objects into `RawCandidate` values.
Every request is paced by the scraper's own `AdaptiveRateLimiter` (`SCRAPER_MIN_INTERVAL_MS`, default 1 s — `with_min_interval`; the scraper is shared, so this bounds all callers together); a `429` backs it off (doubling, honouring a numeric `Retry-After`) and is retried, up to `MAX_SCRAPE_ATTEMPTS` (3) requests per scrape, before surfacing as the retriable error.
`build_http_client(proxy_url)` builds the scraper's reqwest client with an optional `SCRAPER_PROXY_URL` HTTP(S) proxy applied to scraper traffic ONLY (the CDN/probe/TMDB/provider clients stay direct); a non-http(s) proxy scheme is a hard startup error.
Only a 40-hex explicit `infoHash` is accepted (a base32 hash is treated as missing, since every other path keys torrents by 40-hex), and `find` returns no candidates, without a request, for an IMDB id that isn't `tt<digits>`
