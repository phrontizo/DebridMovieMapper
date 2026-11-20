# `jellyfin_client.rs`

Optional Jellyfin notification client.
Notifies Jellyfin of changed paths via `POST /Library/Media/Updated`, authenticating with `Authorization: MediaBrowser Token="<key>"` (newer Jellyfin releases disable the legacy `X-Emby-Token` header by default; 10.x accepts the same scheme).
`new()` rejects a key containing `"`, `\` or `,`, which would break the quoted parameter.
`from_env` delegates to the pure `from_values`, so the trim and blank-as-unset handling is tested without mutating the process environment.
A send that fails to connect, times out, or loses its connection after the request was sent (a reset, or a close before any response, as when Jellyfin restarts mid-request) is retried every 5 s, up to 10 sends; a 502/503/504 or 429 is retried the same way, and any other status or error gives up.
Each transient error is logged with its cause chain (`error_chain`: the error and its `source()` chain, since reqwest's Display names only the kind and URL), so a DNS failure, a refused port and a TLS error read differently, and the give-up line names the last cause
