# TorBox Support — Phase 3: Generalised Re-acquire Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make on-demand instant repair (re-acquire) provider-neutral. Today `try_instant_repair` is RD-specific: it locates the replacement file by the *positional index* of a restricted link in `TorrentInfo.links`. Replace that with **matching the file by `file_path`** and returning a fresh `FileLocator`. This works for both Real-Debrid (per-file restricted links) and TorBox (resolve by `(torrent_id, file_id)`, no link array), and is safer (path matching can't serve the wrong file).

**Architecture:** `try_instant_repair(&FileLocator) -> Result<FileLocator, String>` (was `(torrent_id, failed_link) -> InstantRepairResult`). A small `locator_for_file` helper builds the new `FileLocator` from the re-added torrent's info, pairing the per-file link by position among selected files (RD) or leaving `link: None` (TorBox). `dav_fs` consumes the returned `FileLocator` directly (simplifying the Phase-2 glue). The `InstantRepairResult` struct is removed. **No new trait methods** — the existing `add_magnet`/`get_torrent_info`/`select_files`/`delete_torrent` suffice. The health state machine, cooldowns, and the not-cached branch are unchanged.

**Tech Stack:** Rust 2021, `async-trait`, `tokio`, `reqwest`.

---

## Scope & non-goals

In scope: refactor `try_instant_repair` + add `locator_for_file`; remove `InstantRepairResult`; update its single caller in `dav_fs`; update/extend `repair.rs` tests (including a NEW `MockProvider`-backed cached-success test that the old suite lacked).

Out of scope: the TorBox client (Phase 4); `check_cached`/`add_by_hash` (found unnecessary); `repair_torrent`/`repair_by_id` (unused in production — left as-is, still compile); the scan loop and persistence.

**Definition of done:** `cargo build` + `cargo test` green; the live RD instant-repair behaviour is preserved (re-add by hash, cached → swap to the same file, not-cached → leave to download); the new cached-success unit test passes via `MockProvider`.

## Key facts (verified)
- `try_instant_repair` is called ONLY from `src/dav_fs.rs` (in `ProxiedMediaFile::resolve_cdn_url`).
- `InstantRepairResult { new_torrent_id, new_rd_link }` is constructed only in `repair.rs` and consumed only in `dav_fs.rs`; there's also a compile-check test `_assert_instant_repair_result_fields` to delete.
- `make_test_manager()` (repair tests) builds a `RepairManager` from a real `RealDebridClient` with a fake token; guard tests pre-seed `health_status` so they return before any network call.
- The link pairing rule (from `vfs.rs`): iterate a torrent's files, increment a `link_idx` for every `selected == 1` file, and pair that file with `links[link_idx]`. The target file's link is `links[its index among selected files]`.

---

## Task 1: Make `try_instant_repair` provider-neutral (repair.rs + dav_fs.rs + tests)

This is one cohesive change: the signature change is atomic across `repair.rs` (definition + tests) and `dav_fs.rs` (the single caller), so the crate keeps building.

**Files:**
- Modify: `src/repair.rs` (remove `InstantRepairResult`; add `locator_for_file`; rewrite `try_instant_repair`; update tests)
- Modify: `src/dav_fs.rs` (`resolve_cdn_url` repair arm)

- [ ] **Step 1: Write the new failing test (cached-success via MockProvider)**

Add to `src/repair.rs`'s `#[cfg(test)] mod tests` block:

```rust
#[tokio::test]
async fn try_instant_repair_cached_returns_new_locator() {
    use crate::provider::FileLocator;
    use crate::rd_client::{AddMagnetResponse, TorrentFile, TorrentInfo};

    // MockProvider returns a "downloaded" torrent containing the target file with a link.
    let mock = crate::provider::MockProvider {
        add_magnet: Some(AddMagnetResponse {
            id: "new_tid".to_string(),
            uri: String::new(),
        }),
        torrent_info: Some(TorrentInfo {
            id: "new_tid".to_string(),
            hash: "H".to_string(),
            status: "downloaded".to_string(),
            files: vec![TorrentFile {
                id: 5,
                path: "/Movie.mkv".to_string(),
                bytes: 1000,
                selected: 1,
            }],
            links: vec!["https://rd/newlink".to_string()],
            ..Default::default()
        }),
        ..Default::default()
    };
    let manager = RepairManager::new(std::sync::Arc::new(mock));

    let old = FileLocator {
        hash: "H".to_string(),
        torrent_id: "old_tid".to_string(),
        file_id: 1,
        file_path: "/Movie.mkv".to_string(),
        link: Some("https://rd/oldlink".to_string()),
    };
    let new = manager.try_instant_repair(&old).await.expect("repair should succeed");
    assert_eq!(new.torrent_id, "new_tid");
    assert_eq!(new.file_id, 5);
    assert_eq!(new.file_path, "/Movie.mkv");
    assert_eq!(new.link.as_deref(), Some("https://rd/newlink"));
    assert_eq!(new.hash, "H");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib try_instant_repair_cached_returns_new_locator`
Expected: FAIL to compile — `try_instant_repair` still takes `(&str, &str)` and returns `InstantRepairResult`.

- [ ] **Step 3: Remove `InstantRepairResult`**

In `src/repair.rs`, DELETE the struct (lines ~9-13):

```rust
#[derive(Debug)]
pub struct InstantRepairResult {
    pub new_torrent_id: String,
    pub new_rd_link: String,
}
```

- [ ] **Step 4: Add the `locator_for_file` helper**

Add this associated function inside `impl RepairManager` (place it directly above `try_instant_repair`):

```rust
    /// Build a `FileLocator` for the file at `file_path` within `info`. The per-file
    /// restricted link is paired by position among selected files (Real-Debrid); for
    /// providers with no per-file link array (TorBox) `links` is empty so `link` is
    /// `None` and the file is addressed by `(torrent_id, file_id)`. Returns `None` if
    /// no selected file matches `file_path`.
    fn locator_for_file(
        info: &TorrentInfo,
        hash: &str,
        file_path: &str,
    ) -> Option<crate::provider::FileLocator> {
        let mut link_idx = 0;
        for file in &info.files {
            if file.selected == 1 {
                if file.path == file_path {
                    return Some(crate::provider::FileLocator {
                        hash: hash.to_string(),
                        torrent_id: info.id.clone(),
                        file_id: file.id,
                        file_path: file_path.to_string(),
                        link: info.links.get(link_idx).cloned(),
                    });
                }
                link_idx += 1;
            }
        }
        None
    }
```

- [ ] **Step 5: Rewrite `try_instant_repair`**

Replace the ENTIRE existing `try_instant_repair` method (its doc comment + body) with:

```rust
    /// Attempt instant repair (re-acquire) for a broken/uncached file. Re-adds the
    /// torrent by hash; if the replacement is immediately available, returns a fresh
    /// `FileLocator` for the SAME file (matched by `file_path`). Returns `Err` if the
    /// torrent needs downloading or the repair fails.
    pub async fn try_instant_repair(
        &self,
        locator: &crate::provider::FileLocator,
    ) -> Result<crate::provider::FileLocator, String> {
        let torrent_id = locator.torrent_id.as_str();
        let attempt_num = self.check_and_begin_repair(torrent_id).await?;
        info!(
            "Instant repair attempt #{} for torrent {}",
            attempt_num, torrent_id
        );

        // Fetch old torrent info to know which files were selected (for re-selection).
        let old_info = match self.rd_client.get_torrent_info(torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to get torrent info: {}", e));
            }
        };

        info!("Instant repair: adding magnet for hash {}", old_info.hash);
        let (new_torrent_id, _new_info) = self
            .add_and_select_files(torrent_id, &old_info, Duration::from_millis(500))
            .await?;
        info!("Instant repair: new torrent ID {}", new_torrent_id);

        // Brief wait for the provider to process file selection.
        tokio::time::sleep(Duration::from_millis(500)).await;

        let final_info = match self.rd_client.get_torrent_info(&new_torrent_id).await {
            Ok(info) => info,
            Err(e) => {
                self.cleanup_leaked_torrent(&new_torrent_id).await;
                self.set_repair_failed(torrent_id).await;
                return Err(format!("Failed to get final torrent info: {}", e));
            }
        };

        if final_info.status == "downloaded" {
            // Match the SAME file by path (provider-neutral; no positional link index).
            match Self::locator_for_file(&final_info, &locator.hash, &locator.file_path) {
                Some(new_locator) => {
                    self.complete_repair(torrent_id, &new_torrent_id).await;
                    info!(
                        "Instant repair SUCCEEDED for torrent {} -> new ID {} (file {})",
                        torrent_id, new_torrent_id, locator.file_path
                    );
                    Ok(new_locator)
                }
                None => {
                    self.cleanup_leaked_torrent(&new_torrent_id).await;
                    self.set_repair_failed(torrent_id).await;
                    Err(format!(
                        "Repaired torrent missing file path {}",
                        locator.file_path
                    ))
                }
            }
        } else {
            // Not cached -- torrent needs actual download.
            info!(
                "Torrent {} not cached (status: {}), leaving new torrent {} to download",
                torrent_id, final_info.status, new_torrent_id
            );

            if let Err(e) = self.rd_client.delete_torrent(torrent_id).await {
                warn!("Failed to delete old torrent {}: {}", torrent_id, e);
            }

            self.repair_replacements
                .write()
                .await
                .insert(new_torrent_id.to_string(), torrent_id.to_string());

            let mut health_map = self.health_status.write().await;
            if let Some(health) = health_map.get_mut(torrent_id) {
                health.state = RepairState::Broken;
            }

            Err(format!(
                "Torrent not cached (status: {}), needs download",
                final_info.status
            ))
        }
    }
```

- [ ] **Step 6: Update the `dav_fs` caller**

In `src/dav_fs.rs`, the `Err(crate::error::AppError::Unavailable)` arm of `resolve_cdn_url` currently calls `try_instant_repair(&self.locator.torrent_id, &failed_link)` and manually rebuilds the locator from `InstantRepairResult`. Replace the whole `Err(crate::error::AppError::Unavailable) => { ... }` arm with:

```rust
            Err(crate::error::AppError::Unavailable) => {
                tracing::warn!(
                    "Resolve unavailable for {} — attempting instant repair",
                    self.name
                );
                match self.repair_manager.try_instant_repair(&self.locator).await {
                    Ok(new_locator) => {
                        tracing::info!(
                            "Instant repair succeeded for {} — new torrent {}",
                            self.name,
                            new_locator.torrent_id
                        );
                        let old_locator = std::mem::replace(&mut self.locator, new_locator);
                        self.buffer = Bytes::new();
                        self.buffer_start = 0;
                        self.rd_client.invalidate(&old_locator).await;
                        match self.rd_client.resolve_url(&self.locator).await {
                            Ok(url) => {
                                self.cdn_url = Some(url.clone());
                                return Ok(url);
                            }
                            Err(e2) => {
                                tracing::error!(
                                    "Failed to resolve repaired locator for {}: {}",
                                    self.name,
                                    e2
                                );
                            }
                        }
                    }
                    Err(reason) => {
                        tracing::error!(
                            "Instant repair failed for {}: {} — file unavailable",
                            self.name,
                            reason
                        );
                    }
                }
                Err(FsError::GeneralFailure)
            }
```

(The `let failed_link = ...` line from the Phase-2 version is removed — `try_instant_repair` now takes the whole locator.)

- [ ] **Step 7: Update the repair guard tests**

In `src/repair.rs`'s `mod tests`, the four guard tests call `try_instant_repair("torrentN", "some_link")`. Change each call to pass a `FileLocator`. Add `use crate::provider::FileLocator;` at the top of the `mod tests` block (next to the existing `use super::*;`). Then change each call site:

- in `try_instant_repair_rate_limited_within_30s`:
  ```rust
  let result = manager
      .try_instant_repair(&FileLocator {
          torrent_id: "torrent1".to_string(),
          link: Some("some_link".to_string()),
          ..Default::default()
      })
      .await;
  ```
- in `try_instant_repair_max_attempts_exceeded`: same, with `torrent_id: "torrent2"`.
- in `try_instant_repair_skips_already_repairing`: same, with `torrent_id: "torrent3"`.
- in `try_instant_repair_skips_permanently_failed`: same, with `torrent_id: "torrent4"`.

(These still hit the `check_and_begin_repair` guards and return early — no network — so their assertions are unchanged.)

- [ ] **Step 8: Delete the obsolete compile-check test**

In `src/repair.rs`'s `mod tests`, DELETE the `_assert_instant_repair_result_fields` test/helper (it references the removed `InstantRepairResult`). If a `non_cached_branch_inserts_repair_replacement` test uses `include_str!` to assert the source contains `"repair_replacements"`, leave it — that text still exists in the not-cached branch.

- [ ] **Step 9: Build and run the full suite**

Run: `cargo build` (warning-free) then `cargo test --lib`
Expected: all unit tests PASS, including the new `try_instant_repair_cached_returns_new_locator`. Then run `cargo test --no-run` to confirm all integration test targets still compile.

- [ ] **Step 10: Commit**

```bash
git add src/repair.rs src/dav_fs.rs
git commit -m "refactor: provider-neutral instant repair via FileLocator + file_path matching"
```

---

## Task 2: Update documentation

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update `CLAUDE.md`**

- In the `repair.rs` module-table row and/or the "Synchronous instant repair" design bullet, note that instant repair is now provider-neutral: it re-adds by hash, matches the replacement file by `file_path` (not a positional link index), and returns a fresh `FileLocator` that `dav_fs` swaps in and re-resolves. Mention there is no `InstantRepairResult` anymore.

- [ ] **Step 2: Verify and commit**

Run: `cargo test`
Expected: PASS.

```bash
git add CLAUDE.md
git commit -m "docs: document provider-neutral instant repair (Phase 3)"
```

---

## Self-review

**Spec coverage (Phase 3 of the revised spec):**
- Provider-neutral re-acquire via `file_path` matching → Task 1. ✓
- Returns a fresh `FileLocator`; `dav_fs` consumes it → Task 1 (Steps 5, 6). ✓
- `InstantRepairResult` removed → Task 1 (Step 3). ✓
- No new trait methods (existing methods suffice) → by construction. ✓
- Health state machine / cooldowns / not-cached branch preserved → Task 1 (Step 5 keeps them). ✓
- New cached-success test coverage → Task 1 (Step 1). ✓
- Docs → Task 2. ✓

**Placeholder scan:** none — every code step has complete code.

**Type consistency:** `try_instant_repair(&FileLocator) -> Result<FileLocator, String>` used identically in the definition (Step 5), the `dav_fs` caller (Step 6), the guard tests (Step 7), and the new test (Step 1). `locator_for_file(&TorrentInfo, &str, &str) -> Option<FileLocator>` defined in Step 4 and called in Step 5. `FileLocator { hash, torrent_id, file_id: u32, file_path, link: Option<String> }` consistent throughout. The removed `InstantRepairResult` has no remaining references after Steps 3, 6, 8.
