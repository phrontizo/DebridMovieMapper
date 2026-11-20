# Database

## Database is self-healing (but only for genuine corruption)

`Store::open` stamps a schema version and runs forward migrations.
A genuinely corrupt / damaged `metadata.db` (a bad-header `Io(InvalidData)`, a truncated file, redb's `Corrupted`/`RepairAborted`) is **moved aside** to `<db_path>.corrupt` (or `<db_path>.corrupt.<n>` when that name already exists, so an earlier backup is never clobbered) and recreated rather than crashing the service.
The old file is kept as `<db_path>.corrupt`, but the recreated DB starts without the Trakt enrolments, provenance, selections and blacklist (only the `matches` cache and the `upgrade_checks` cursor regenerate), so this is not lossless.
A truncated file whose intact header records a longer layout makes redb panic inside `Database::create`; `try_open` catches the panic and treats it as corruption.
Authoritative tables added in later phases are migrated, never silently dropped (e.g. v7→v8 rewrites owned records in place to flag `quality.unverified` on those a `selection` entry serves from a non-MKV/MP4 file — engine-acquired copies; pre-existing account-mirror copies have no selection entry and are not flagged retroactively).
A **transient/incompatible** open failure — another instance holding the lock (`DatabaseAlreadyOpen`), a permission/disk I/O error, an intact database in an older `redb` on-disk format (`UpgradeRequired`, e.g. after a `redb` dependency bump), an intact database in a NEWER `redb` on-disk format (redb reports it as `Corrupted("Expected file format version <= N, found M")`, which `db_open_failure` classifies as transient), **or a schema written by a NEWER build of this app** (our stored schema version is ahead of the binary, e.g. after a rollback) — is classified separately (`db_open_failure` / the `try_open` version check) and **fails startup** instead of discarding a possibly-intact database (its authoritative `owned_hashes`/`authoritative_ids`/`selection`/`trakt_tokens` tables are not regenerable, and `wanted` only comes back once the accounts are re-enrolled, so the operator rolls the binary forward / backs up / migrates rather than losing them).
The newer-schema case is treated like `UpgradeRequired` — intact-but-unusable, never moved aside.
The test-only constructor `Store::from_database` (`#[cfg(test)]`, behind `Store::in_memory` and the `fail_writes` helpers) refuses it too.
