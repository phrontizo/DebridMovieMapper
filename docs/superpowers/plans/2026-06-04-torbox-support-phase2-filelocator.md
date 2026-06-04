# TorBox Support — Phase 2: FileLocator Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace RD-specific, link-based file resolution (`unrestrict_link(link)`) with a provider-neutral, locator-based model (`resolve_url(&FileLocator)`), so a file can be resolved by `(torrent_id, file_id)` (TorBox) or by a restricted link (RD). This is the prerequisite for the TorBox client.

**Architecture:** Introduce a `FileLocator { hash, torrent_id, file_id, file_path, link: Option<String> }`. The VFS `MediaFile` node stores a `FileLocator` instead of `(rd_link, rd_torrent_id)`. The `DebridProvider` trait gains `resolve_url(&FileLocator) -> Result<String, AppError>` and `invalidate(&FileLocator)`; `RealDebridClient` implements them by wrapping its existing `unrestrict_link`/`invalidate_unrestrict_cache` inherent methods. A new `AppError::Unavailable` variant signals the recoverable "broken/uncached → repair" case (RD: 503). `dav_fs` resolves via `resolve_url`; the existing instant-repair glue is preserved (it still calls `try_instant_repair` and rebuilds a `FileLocator` from the result). **No change to the scan loop, redb persistence, or `repair.rs`.**

**Tech Stack:** Rust 2021, `async-trait`, `tokio`, `reqwest`, `dav-server`.

---

## Scope & non-goals

In scope: `FileLocator`; `resolve_url`/`invalidate` trait methods; RD + `MockProvider` impls; VFS `MediaFile` node shape; `dav_fs` resolution path; the `AppError::Unavailable` signal; removing the now-unused `unrestrict_link`/`invalidate_unrestrict_cache` from the **trait** (kept as inherent on `RealDebridClient`).

**Out of scope (later phases):** `check_cached`/`add_by_hash` (Phase 3); generalising `repair.rs` to the trait (Phase 3); the TorBox client (Phase 4). `get_torrents`/`get_torrent_info`/`add_magnet`/`select_files`/`delete_torrent` stay on the trait unchanged — they already map to both providers. **The scan loop (`tasks.rs`) and redb persistence are untouched.**

**Definition of done:** `cargo build` + `cargo test` green; RD playback/repair behaviour is unchanged (the resolve path produces the same CDN URLs and the same 503→instant-repair flow); the VFS node and `dav_fs` now speak `FileLocator`.

## File structure

| File | Change |
|------|--------|
| `src/error.rs` | Add `AppError::Unavailable` |
| `src/provider.rs` | Add `FileLocator`; add `resolve_url`/`invalidate` to trait; RD + Mock impls; later remove `unrestrict_link`/`invalidate_unrestrict_cache` from trait |
| `src/rd_client.rs` | `RealDebridClient` impl of `resolve_url`/`invalidate` (wrap inherent methods) |
| `src/vfs.rs` | `MediaFile` node stores `FileLocator`; `build` constructs it; update tests |
| `src/dav_fs.rs` | `ProxiedMediaFile` stores `FileLocator`; resolve via `resolve_url`; repair glue rebuilds locator; update tests |

---

## Task 1: Add `AppError::Unavailable`

**Files:**
- Modify: `src/error.rs`
- Test: `src/error.rs` (new `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to `src/error.rs` (create the test module at the end of the file):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_variant_displays() {
        let e = AppError::Unavailable;
        assert_eq!(e.to_string(), "Debrid resource temporarily unavailable");
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib unavailable_variant_displays`
Expected: FAIL to compile — `no variant named \`Unavailable\``.

- [ ] **Step 3: Add the variant**

In `src/error.rs`, add this variant to the `AppError` enum (after the `Config` variant):

```rust
    #[error("Debrid resource temporarily unavailable")]
    Unavailable,
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib unavailable_variant_displays`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/error.rs
git commit -m "feat: add AppError::Unavailable for recoverable resolve failures"
```

---

## Task 2: Add the `FileLocator` type

**Files:**
- Modify: `src/provider.rs` (add the struct above the `DebridProvider` trait)
- Test: `src/provider.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/provider.rs`:

```rust
#[test]
fn file_locator_constructs_and_clones() {
    let loc = FileLocator {
        hash: "abc".to_string(),
        torrent_id: "t1".to_string(),
        file_id: 10,
        file_path: "Movie/Movie.mkv".to_string(),
        link: Some("https://rd/restricted".to_string()),
    };
    let cloned = loc.clone();
    assert_eq!(cloned, loc);
    assert_eq!(cloned.file_id, 10);
    assert_eq!(cloned.link.as_deref(), Some("https://rd/restricted"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib file_locator_constructs_and_clones`
Expected: FAIL to compile — `cannot find struct \`FileLocator\``.

- [ ] **Step 3: Add the struct**

Insert into `src/provider.rs` directly ABOVE the `#[async_trait::async_trait] pub trait DebridProvider` line:

```rust
/// Identifies a single media file for resolution. Stable identity is
/// `(hash, file_path)`; `torrent_id`/`file_id`/`link` are re-derivable (e.g. after
/// a re-acquire). `link` is the provider's per-file restricted link when it has
/// one (Real-Debrid); `None` for providers that resolve by `(torrent_id, file_id)`
/// (TorBox).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FileLocator {
    pub hash: String,
    pub torrent_id: String,
    pub file_id: u32,
    pub file_path: String,
    pub link: Option<String>,
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib file_locator_constructs_and_clones`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/provider.rs
git commit -m "feat: add FileLocator type for provider-neutral resolution"
```

---

## Task 3: Add `resolve_url` + `invalidate` to the trait (alongside the old methods)

Add the new methods to the trait and implement them for `RealDebridClient` (wrapping its inherent `unrestrict_link`/`invalidate_unrestrict_cache`) and `MockProvider`. The old `unrestrict_link`/`invalidate_unrestrict_cache` trait methods remain for now (removed in Task 6 once `dav_fs` no longer uses them).

**Files:**
- Modify: `src/provider.rs` (trait + `MockProvider` impl)
- Modify: `src/rd_client.rs` (trait impl for `RealDebridClient`)
- Test: `src/provider.rs` (`mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/provider.rs`:

```rust
#[tokio::test]
async fn mock_resolve_url_returns_configured_value() {
    let mock = MockProvider {
        resolved_url: Some("https://cdn/file".to_string()),
        ..Default::default()
    };
    let provider: Arc<dyn DebridProvider> = Arc::new(mock);
    let loc = FileLocator {
        hash: "h".to_string(),
        torrent_id: "t".to_string(),
        file_id: 1,
        file_path: "f.mkv".to_string(),
        link: None,
    };
    assert_eq!(provider.resolve_url(&loc).await.unwrap(), "https://cdn/file");
    provider.invalidate(&loc).await; // no-op, must not panic
}

#[tokio::test]
async fn mock_resolve_url_unavailable_by_default() {
    let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
    let loc = FileLocator::default();
    assert!(matches!(
        provider.resolve_url(&loc).await,
        Err(crate::error::AppError::Unavailable)
    ));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib mock_resolve_url`
Expected: FAIL to compile — trait has no `resolve_url`; `MockProvider` has no `resolved_url` field.

- [ ] **Step 3: Extend the trait**

In `src/provider.rs`, add these two methods to the `DebridProvider` trait (place them after `delete_torrent`, before the `invalidate_unrestrict_cache`/`evict_expired_cache` block):

```rust
    /// Resolve a file to a streamable CDN URL. Returns `AppError::Unavailable` when the
    /// file's bytes are not currently available (RD: 503 on a broken torrent) so callers
    /// can trigger re-acquire/repair.
    async fn resolve_url(&self, loc: &FileLocator) -> Result<String, crate::error::AppError>;

    /// Drop any cached resolution for `loc` (RD: the unrestrict-cache entry for its link).
    async fn invalidate(&self, loc: &FileLocator);
```

- [ ] **Step 4: Implement for `MockProvider`**

In `src/provider.rs`, add a `resolved_url: Option<String>` field to `MockProvider`:

```rust
    pub resolved_url: Option<String>,
```

and add these methods to the `impl DebridProvider for MockProvider` block:

```rust
    async fn resolve_url(&self, _loc: &FileLocator) -> Result<String, crate::error::AppError> {
        match &self.resolved_url {
            Some(u) => Ok(u.clone()),
            None => Err(crate::error::AppError::Unavailable),
        }
    }
    async fn invalidate(&self, _loc: &FileLocator) {}
```

- [ ] **Step 5: Implement for `RealDebridClient`**

In `src/rd_client.rs`, add these methods to the `impl crate::provider::DebridProvider for RealDebridClient` block (after `delete_torrent`):

```rust
    async fn resolve_url(
        &self,
        loc: &crate::provider::FileLocator,
    ) -> Result<String, crate::error::AppError> {
        let link = loc.link.as_deref().ok_or(crate::error::AppError::Unavailable)?;
        match self.unrestrict_link(link).await {
            Ok(resp) => Ok(resp.download),
            Err(e) if e.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE) => {
                Err(crate::error::AppError::Unavailable)
            }
            Err(e) => Err(crate::error::AppError::Http(e)),
        }
    }

    async fn invalidate(&self, loc: &crate::provider::FileLocator) {
        if let Some(link) = loc.link.as_deref() {
            self.invalidate_unrestrict_cache(link).await;
        }
    }
```

(The inherent `unrestrict_link` and `invalidate_unrestrict_cache` methods on `RealDebridClient` are unchanged and still used here.)

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --lib mock_resolve_url` then `cargo build`
Expected: both new tests PASS; build is warning-free.

- [ ] **Step 7: Commit**

```bash
git add src/provider.rs src/rd_client.rs
git commit -m "feat: add resolve_url/invalidate (FileLocator) to DebridProvider"
```

---

## Task 4: Store a `FileLocator` in the VFS `MediaFile` node

**Files:**
- Modify: `src/vfs.rs` (node enum at lines ~96-111; `add_path_to_tree` ~568; its two call sites — `add_torrent_files` ~553 and the show loop ~361; `mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/vfs.rs`:

```rust
#[test]
fn build_stores_file_locator_on_media_node() {
    let torrents = vec![(
        TorrentInfo {
            id: "tid".to_string(),
            filename: "Movie.2023.mkv".to_string(),
            original_filename: "Movie.2023.mkv".to_string(),
            hash: "deadbeef".to_string(),
            bytes: 1000,
            original_bytes: 1000,
            host: "h".to_string(),
            split: 1,
            progress: 100.0,
            status: "downloaded".to_string(),
            added: "2023-01-01".to_string(),
            files: vec![TorrentFile {
                id: 7,
                path: "/Movie.2023.mkv".to_string(),
                bytes: 1000,
                selected: 1,
            }],
            links: vec!["https://rd/restricted".to_string()],
            ended: None,
        },
        MediaMetadata {
            title: "Movie".to_string(),
            year: Some("2023".to_string()),
            media_type: MediaType::Movie,
            external_id: None,
        },
    )];
    let vfs = DebridVfs::build(torrents);
    if let VfsNode::Directory { children } = &vfs.root {
        let movies = children.get("Movies").unwrap();
        if let VfsNode::Directory { children: mc } = movies {
            let folder = mc.get("Movie").unwrap();
            if let VfsNode::Directory { children: files } = folder {
                let file = files.get("Movie.2023.mkv").expect("media file missing");
                if let VfsNode::MediaFile { locator, file_size } = file {
                    assert_eq!(*file_size, 1000);
                    assert_eq!(locator.hash, "deadbeef");
                    assert_eq!(locator.torrent_id, "tid");
                    assert_eq!(locator.file_id, 7);
                    assert_eq!(locator.file_path, "/Movie.2023.mkv");
                    assert_eq!(locator.link.as_deref(), Some("https://rd/restricted"));
                } else {
                    panic!("expected MediaFile");
                }
            }
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib build_stores_file_locator_on_media_node`
Expected: FAIL to compile — `MediaFile` has no `locator` field.

- [ ] **Step 3: Change the node enum**

In `src/vfs.rs`, add the import at the top (with the other `use` lines):

```rust
use crate::provider::FileLocator;
```

Change the `MediaFile` variant of `VfsNode` (currently `file_size`, `rd_link`, `rd_torrent_id`) to:

```rust
    MediaFile {
        file_size: u64,
        locator: FileLocator,
    },
```

- [ ] **Step 4: Update `add_path_to_tree` to take a `FileLocator`**

Change `add_path_to_tree`'s signature and body in `src/vfs.rs`. Replace the current signature:

```rust
    fn add_path_to_tree(
        root: &mut BTreeMap<String, VfsNode>,
        path: &str,
        size: u64,
        torrent_id: String,
        link: String,
    ) -> String {
```

with:

```rust
    fn add_path_to_tree(
        root: &mut BTreeMap<String, VfsNode>,
        path: &str,
        size: u64,
        locator: FileLocator,
    ) -> String {
```

and replace the node construction inside it (currently building `VfsNode::MediaFile { file_size: size, rd_link: link.clone(), rd_torrent_id: torrent_id.clone() }`) with:

```rust
                current_children.insert(
                    final_name,
                    VfsNode::MediaFile {
                        file_size: size,
                        locator: locator.clone(),
                    },
                );
```

- [ ] **Step 5: Update the two call sites to build a `FileLocator`**

In `add_torrent_files` (the movie path), replace the `Self::add_path_to_tree(destination, &path, file.bytes, torrent.id.clone(), link.clone())` call with:

```rust
                        Self::add_path_to_tree(
                            destination,
                            &path,
                            file.bytes,
                            FileLocator {
                                hash: torrent.hash.clone(),
                                torrent_id: torrent.id.clone(),
                                file_id: file.id,
                                file_path: file.path.clone(),
                                link: Some(link.clone()),
                            },
                        );
```

In the show-building loop, replace the `Self::add_path_to_tree(season_children, filename, file.bytes, torrent.id.clone(), link.clone())` call with:

```rust
                                            let strm_name = Self::add_path_to_tree(
                                                season_children,
                                                filename,
                                                file.bytes,
                                                FileLocator {
                                                    hash: torrent.hash.clone(),
                                                    torrent_id: torrent.id.clone(),
                                                    file_id: file.id,
                                                    file_path: file.path.clone(),
                                                    link: Some(link.clone()),
                                                },
                                            );
```

(The `let strm_name = ...` binding and the lines around it are otherwise unchanged.)

- [ ] **Step 6: Fix existing tests that read the old fields**

Search `src/vfs.rs` tests for `rd_torrent_id` and `rd_link`. In `test_vfs_duplicates`, change:

```rust
                    if let VfsNode::MediaFile { rd_torrent_id, .. } = file {
                        assert_eq!(
                            rd_torrent_id, "large",
```

to:

```rust
                    if let VfsNode::MediaFile { locator, .. } = file {
                        assert_eq!(
                            locator.torrent_id, "large",
```

Fix any other test in `src/vfs.rs` that destructures `MediaFile { rd_link, .. }` / `{ rd_torrent_id, .. }` the same way (compile errors will point them out).

- [ ] **Step 7: Run tests**

Run: `cargo test --lib vfs` then `cargo test --lib build_stores_file_locator_on_media_node`
Expected: PASS (all vfs tests green, including the new one).

- [ ] **Step 8: Commit**

```bash
git add src/vfs.rs
git commit -m "refactor: VFS MediaFile node stores a FileLocator"
```

---

## Task 5: Resolve via `resolve_url` in `dav_fs`

Migrate `ProxiedMediaFile` to hold a `FileLocator` and resolve through `resolve_url`. Preserve the existing 503→instant-repair behaviour by detecting `AppError::Unavailable` and rebuilding a fresh `FileLocator` from the repair result.

**Files:**
- Modify: `src/dav_fs.rs` (`ProxiedMediaFile` struct ~235; `open()` construction ~89-106; `resolve_cdn_url` ~251-322; `fetch_cdn_range` invalidate call ~409-411; `should_hide_torrent`/`metadata` use of `rd_torrent_id` ~435; `should_repair_on_unrestrict_error` ~228; tests)

- [ ] **Step 1: Write the failing test**

Add to the `provider_abstraction_tests` module at the end of `src/dav_fs.rs` (created in Phase 1):

```rust
    #[test]
    fn proxied_media_file_holds_locator() {
        use crate::provider::FileLocator;
        let loc = FileLocator {
            hash: "h".to_string(),
            torrent_id: "t".to_string(),
            file_id: 3,
            file_path: "Movie/Movie.mkv".to_string(),
            link: Some("https://rd/x".to_string()),
        };
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider {
            resolved_url: Some("https://cdn/movie".to_string()),
            ..Default::default()
        });
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let f = ProxiedMediaFile {
            name: "Movie.mkv".to_string(),
            locator: loc.clone(),
            file_size: 10,
            repair_manager: repair,
            rd_client: provider,
            http_client: reqwest::Client::new(),
            pos: 0,
            cdn_url: None,
            buffer: bytes::Bytes::new(),
            buffer_start: 0,
        };
        assert_eq!(f.locator.file_id, 3);
    }
```

> NOTE: `ProxiedMediaFile` is private to `dav_fs`; this test lives in the same file so it can name the struct. The test only checks construction with the new field.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib proxied_media_file_holds_locator`
Expected: FAIL to compile — `ProxiedMediaFile` has `rd_link`/`rd_torrent_id`, not `locator`.

- [ ] **Step 3: Change the `ProxiedMediaFile` struct**

In `src/dav_fs.rs`, replace the `rd_link: String,` and `rd_torrent_id: String,` fields of `ProxiedMediaFile` with a single field (keep the rest of the struct unchanged):

```rust
    locator: crate::provider::FileLocator,
```

- [ ] **Step 4: Update `open()` to build the file from the node's locator**

In the `open` method, the `VfsNode::MediaFile { .. }` arm currently destructures `file_size, rd_link, rd_torrent_id` and constructs `ProxiedMediaFile { ... rd_link, rd_torrent_id, ... }`. Change the destructure and construction to use `locator`:

```rust
                VfsNode::MediaFile {
                    file_size,
                    locator,
                } => Ok(Box::new(ProxiedMediaFile {
                    name,
                    locator,
                    file_size,
                    repair_manager: self.repair_manager.clone(),
                    rd_client: self.rd_client.clone(),
                    http_client: self.http_client.clone(),
                    pos: 0,
                    cdn_url: None,
                    buffer: Bytes::new(),
                    buffer_start: 0,
                }) as Box<dyn DavFile>),
```

- [ ] **Step 5: Rewrite `resolve_cdn_url` to use `resolve_url`**

Replace the body of `resolve_cdn_url` (the `match self.rd_client.unrestrict_link(...)` block) with the locator-based version. The new method:

```rust
    async fn resolve_cdn_url(&mut self) -> Result<String, FsError> {
        if let Some(ref url) = self.cdn_url {
            return Ok(url.clone());
        }

        match self.rd_client.resolve_url(&self.locator).await {
            Ok(url) => {
                self.cdn_url = Some(url.clone());
                Ok(url)
            }
            Err(crate::error::AppError::Unavailable) => {
                tracing::warn!(
                    "Resolve unavailable for {} — attempting instant repair",
                    self.name
                );
                let failed_link = self.locator.link.clone().unwrap_or_default();
                match self
                    .repair_manager
                    .try_instant_repair(&self.locator.torrent_id, &failed_link)
                    .await
                {
                    Ok(result) => {
                        tracing::info!(
                            "Instant repair succeeded for {} — new torrent {}",
                            self.name,
                            result.new_torrent_id
                        );
                        // Build a fresh locator: identity (hash, file_path, file_id) is
                        // unchanged; torrent_id and link are replaced by the repair result.
                        let old_locator = self.locator.clone();
                        self.locator.torrent_id = result.new_torrent_id;
                        self.locator.link = Some(result.new_rd_link);
                        self.buffer = Bytes::new();
                        self.buffer_start = 0;
                        // Drop the stale cached resolution for the old link.
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
            Err(e) => {
                tracing::warn!("Resolve failed for {} (not repairing): {}", self.name, e);
                Err(FsError::GeneralFailure)
            }
        }
    }
```

Then DELETE the now-unused `should_repair_on_unrestrict_error` function (lines ~224-230) and its doc comment — the recoverable case is now signalled by `AppError::Unavailable`. (The Phase 1 dav_fs test `should_repair_only_on_503` exercised that function; remove that test too — see Step 7.)

- [ ] **Step 6: Update `fetch_cdn_range` and `metadata` references**

In `fetch_cdn_range`, replace the stale-URL invalidate call:

```rust
                self.cdn_url = None;
                self.rd_client
                    .invalidate_unrestrict_cache(&self.rd_link)
                    .await;
```

with:

```rust
                self.cdn_url = None;
                self.rd_client.invalidate(&self.locator).await;
```

In `metadata` (the `DavFile for ProxiedMediaFile` impl), replace `self.repair_manager.should_hide_torrent(&self.rd_torrent_id)` with `self.repair_manager.should_hide_torrent(&self.locator.torrent_id)`. Search the rest of `dav_fs.rs` for any other `self.rd_link` / `self.rd_torrent_id` references and replace with `self.locator.link` (as `&str` via `.as_deref().unwrap_or_default()` where a `&str` is needed) / `self.locator.torrent_id`.

- [ ] **Step 7: Remove the obsolete `should_repair_only_on_503` test**

In `src/dav_fs.rs`'s `mod tests`, delete the `should_repair_only_on_503` test (it referenced the removed `should_repair_on_unrestrict_error`). If any other existing dav_fs test constructs a `ProxiedMediaFile` with `rd_link`/`rd_torrent_id`, update it to use `locator: FileLocator { .. }` (compile errors will flag them).

- [ ] **Step 8: Run tests**

Run: `cargo test --lib dav_fs` then `cargo test --lib proxied_media_file_holds_locator` and `cargo build`
Expected: PASS; warning-free build.

- [ ] **Step 9: Commit**

```bash
git add src/dav_fs.rs
git commit -m "refactor: dav_fs resolves via resolve_url(FileLocator)"
```

---

## Task 6: Remove the obsolete link-based methods from the trait

`dav_fs` no longer calls `unrestrict_link`/`invalidate_unrestrict_cache` through the trait, and no other consumer does. Remove them from the **trait** (keep them as inherent methods on `RealDebridClient`, which `resolve_url`/`invalidate` still call).

**Files:**
- Modify: `src/provider.rs` (trait + `MockProvider`)
- Modify: `src/rd_client.rs` (trait impl)

- [ ] **Step 1: Remove from the trait**

In `src/provider.rs`, delete these two method declarations from the `DebridProvider` trait:

```rust
    async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error>;
    async fn invalidate_unrestrict_cache(&self, link: &str);
```

- [ ] **Step 2: Remove the trait impls**

In `src/rd_client.rs`, delete the `unrestrict_link` and `invalidate_unrestrict_cache` methods from the `impl crate::provider::DebridProvider for RealDebridClient` block. (Do NOT delete the inherent `RealDebridClient::unrestrict_link` / `invalidate_unrestrict_cache` methods — `resolve_url`/`invalidate` depend on them.)

In `src/provider.rs`, delete the `unrestrict_link` and `invalidate_unrestrict_cache` methods from the `impl DebridProvider for MockProvider` block.

Also remove the now-unused `UnrestrictResponse` import from `src/provider.rs`'s top `use crate::rd_client::{...}` line **only if** it is no longer referenced there (the trait no longer mentions it; check whether anything else in `provider.rs` uses it — if not, drop it from the import to keep the build warning-free).

- [ ] **Step 3: Build and test**

Run: `cargo build` then `cargo test --lib`
Expected: warning-free build; all unit tests PASS. (If the build complains that `UnrestrictResponse` is unused, remove it from the import per Step 2.)

- [ ] **Step 4: Commit**

```bash
git add src/provider.rs src/rd_client.rs
git commit -m "refactor: drop link-based methods from DebridProvider trait"
```

---

## Task 7: Update documentation

**Files:**
- Modify: `CLAUDE.md`

- [ ] **Step 1: Update `CLAUDE.md`**

- In the data-flow / design section, note that file resolution is provider-neutral: the VFS `MediaFile` node stores a `FileLocator { hash, torrent_id, file_id, file_path, link }`, and `dav_fs` calls `provider.resolve_url(&FileLocator)` (RD resolves via its restricted link; the model also supports resolving by `(torrent_id, file_id)`).
- Update the `provider.rs` row / design bullet to mention `FileLocator`, `resolve_url`, `invalidate`, and `AppError::Unavailable` (the recoverable "needs repair/re-acquire" signal).

- [ ] **Step 2: Verify and commit**

Run: `cargo test`
Expected: PASS.

```bash
git add CLAUDE.md
git commit -m "docs: document FileLocator-based resolution (Phase 2)"
```

---

## Self-review

**Spec coverage (Phase 2 of the revised spec):**
- `FileLocator` canonical type → Task 2. ✓
- `resolve_url`/`invalidate` trait methods, RD impl → Tasks 3, 6. ✓
- VFS node stores `FileLocator` → Task 4. ✓
- `dav_fs` resolves via `resolve_url`, repair glue preserved → Task 5. ✓
- Recoverable-error signal (`AppError::Unavailable`) → Task 1. ✓
- List-driven persistence + `repair.rs` + scan loop untouched → not modified (by construction). ✓
- Out of scope (Phase 3/4): `check_cached`/`add_by_hash`, generalising `repair.rs`, TorBox client. Documented above.

**Placeholder scan:** none — every code step has complete code.

**Type consistency:** `FileLocator { hash: String, torrent_id: String, file_id: u32, file_path: String, link: Option<String> }` used identically in Tasks 2, 3, 4, 5. `resolve_url(&FileLocator) -> Result<String, AppError>` consistent across trait decl (Task 3), RD impl (Task 3), Mock impl (Task 3), and call site (Task 5). `AppError::Unavailable` introduced in Task 1 and matched in Tasks 3 and 5. The VFS node's new shape `MediaFile { file_size, locator }` is produced in Task 4 and consumed in Task 5's `open()`.
