# `enrolment.rs`

Local-network Trakt device-flow enrolment page (`/trakt/accounts`; enrol/refresh/remove) served on the WebDAV listener; `poll_to_completion` stores tokens keyed by Trakt user slug (a `/users/me` answer with no slug or username counts as a failed `me()` attempt and is never stored under an empty key).
Its polling stops `expires_in` seconds after it starts, by the clock.
A poll held up by the throttled Trakt limiter counts toward that, and one still waiting at the deadline is cut off, so a throttled enrolment frees its `MAX_CONCURRENT_ENROLMENTS` slot when its device code expires.
A Host allowlist (`host_is_allowed`: IP literals, `localhost`, single-label names, `.local`/`.lan`/`.internal`/`.home.arpa`, plus `TRAKT_ENROL_ALLOWED_HOSTS` via `EnrolmentService::with_allowed_hosts`) refuses DNS-rebinding requests with 421 before the CSRF same-origin guard runs.
Every page is served with `frame-ancestors 'none'` in its CSP plus `X-Frame-Options: DENY` (`html()`), so another site can't frame a page and have a click submit its same-origin Remove form (clickjacking), which the CSRF guard alone would let through.
A Refresh click's token refresh runs in its own task, so a browser that closes the connection can't cut it off after Trakt has rotated the single-use refresh token.
The click waits for it at most 20 s (`TRAKT_CLICK_WAIT`), then redirects while the refresh finishes in the background, and a click while that account's refresh is still running starts no other.
An Enrol click whose device-code request takes longer than that answers 503.
Both calls share the Trakt client's rate limiter with the background sync, whose 429 back-off could otherwise hold a click for minutes. *Remove* (`remove_account` → `Store::remove_trakt_account`) drops the tokens and the user's wanted rows in one transaction, so Trigger B can then remove what only that account wanted; with no account left, `removals_trustworthy` holds and every watchlist-only title is removed
