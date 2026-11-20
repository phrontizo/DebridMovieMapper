# `config.rs`

`Config` — all startup env parsing/validation (`from_env`/`from_parts`); shaped for a future DB-override layer.
Also parses the optional `TraktConfig` (`Some` only when both `TRAKT_CLIENT_ID` + `TRAKT_CLIENT_SECRET` are present; carries the two Trakt interval knobs plus the optional `catchup_lookback_secs` from `TRAKT_CATCHUP_LOOKBACK` and `enrol_allowed_hosts` from `TRAKT_ENROL_ALLOWED_HOSTS` via `parse_allowed_hosts`; `trakt_half_configured` makes `TraktConfig::from_parts` warn when exactly one of the client id / secret is set).
Numeric knobs go through `parse_num` (trim, blank = default, warn + default on a parse error, clamp the whole result), except `PORT` (`resolve_port`: 0 or invalid → 8080 with a warning), `UPGRADE_BUDGET_PER_TICK` (`parse_upgrade_budget`: invalid → no cap) and `UPGRADE_INTERVAL_SECS` (`parse_upgrade_interval`: `0`/`off`/`false`/`no` → disabled, any other non-number → daily with a warning that the engine is on).
Booleans go through the single parser `AcquisitionConfig::parse_bool(value, name, default)` (`true`/`1`/`yes`/`on`, `false`/`0`/`no`/`off`, case-insensitive).
An unrecognised non-blank value is `warn!`ed (`Invalid <NAME> value ..., falling back to <default>`), like the numeric knobs; absent or blank stays silent.
`parse_lookback` accepts d/w/m(o)/y units in short and long spellings (see `TRAKT_CATCHUP_LOOKBACK`).
`AudioReq::parse` / `SubReq::parse` check each value with `parse_lang_code` (`probe::is_known_language_code`; see `AUDIO_LANGUAGE`).
`load_dotenv` (moved here from `main.rs` with its `dotenv_failure` helper) loads `.env` and returns a token-free `Err` when the file exists but can't be loaded; `main` exits on it and `tests/common`'s `load_env` panics on it.
The source-guard test `compose_passes_every_setting_through_to_the_container` fails when a variable the service reads (`env::var("X")`/`flag("X")` in config.rs, main.rs or jellyfin_client.rs) isn't passed through compose.yml as `X=${X:...}`.
`DB_PATH`, `JELLYFIN_URL` and `JELLYFIN_RCLONE_MOUNT_PATH` need only be listed (they are fixed there), and `PORT` must not be listed, since it sets only the published host port
