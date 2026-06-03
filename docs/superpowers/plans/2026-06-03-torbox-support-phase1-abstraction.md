# TorBox Support — Phase 1: Provider Abstraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Introduce a `DebridProvider` trait so the rest of the system depends on `Arc<dyn DebridProvider>` instead of the concrete `RealDebridClient`, and add startup provider-selection (RD xor TorBox), with **zero runtime behaviour change** — Real-Debrid still does exactly what it does today.

**Architecture:** Add a new `src/provider.rs` defining the `DebridProvider` trait (via `async_trait`) plus a pure `choose_provider` selection function and a test-only `MockProvider`. `RealDebridClient` implements the trait by delegating to its existing inherent methods. `RepairManager`, `DebridFileSystem`/`ProxiedMediaFile`, and `ScanConfig` swap their `Arc<RealDebridClient>` fields for `Arc<dyn DebridProvider>`. `main.rs` chooses the provider from the two optional tokens.

**Tech Stack:** Rust 2021, `async-trait`, `tokio`, `reqwest`, `redb`, `dav-server`.

---

## Phase 1 scope & relationship to the spec

This phase introduces the **seam** only. To keep it a true no-behaviour-change refactor, the trait's method set **mirrors the Real-Debrid operations the code calls today** (`get_torrents`, `get_torrent_info`, `unrestrict_link`, `add_magnet`, `select_files`, `delete_torrent`, plus the two cache helpers and `name()`), and the trait **references the existing `crate::rd_client` data types** rather than moving them.

The spec's provider-neutral reshape — moving the model into `provider.rs`, replacing per-file restricted links with a `FileLocator`, and `resolve_url`/`check_cached`/`add_by_hash` — is deliberately **deferred to Phase 2 (durable catalogue) and Phase 4 (TorBox client)**, where a second implementation and the catalogue validate the right shape. Designing the neutral trait now, with no second implementor to test it against, risks the wrong abstraction. Phase 1 makes `Arc<dyn DebridProvider>` flow everywhere so those later changes are localised.

**Definition of done for Phase 1:** `cargo build` and `cargo test` pass; running with only `RD_API_TOKEN` behaves identically to today; setting both tokens (or neither) exits with a clear error; `TORBOX_API_KEY` alone is recognised and exits with a "not yet available" message.

## File structure

| File | Responsibility after Phase 1 |
|------|------------------------------|
| `src/provider.rs` *(new)* | `DebridProvider` trait, `ProviderKind`, pure `choose_provider`, `#[cfg(test)] MockProvider` |
| `src/rd_client.rs` | Unchanged inherent API; gains `impl DebridProvider for RealDebridClient`; structs gain `Default` |
| `src/repair.rs` | `RepairManager` holds `Arc<dyn DebridProvider>` |
| `src/dav_fs.rs` | `DebridFileSystem` + `ProxiedMediaFile` hold `Arc<dyn DebridProvider>` |
| `src/tasks.rs` | `ScanConfig.rd_client` is `Arc<dyn DebridProvider>` |
| `src/main.rs` | Provider selection via `choose_provider` |
| `src/mapper.rs` | Declares `pub mod provider;` |

---

## Task 1: Add the `async-trait` dependency

**Files:**
- Modify: `Cargo.toml:7-24` (dependencies table)

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`, add this line to the `[dependencies]` section (alphabetical-ish placement, e.g. directly under the `[dependencies]` header above `tokio`):

```toml
async-trait = "0.1"
```

- [ ] **Step 2: Verify it resolves and builds**

Run: `cargo build`
Expected: builds successfully; `async-trait` is fetched and compiled. No warnings about an unused dependency yet (that's fine).

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "build: add async-trait dependency for provider abstraction"
```

---

## Task 2: Derive `Default` on the RD data structs

These derives let `MockProvider` (Task 4) construct canned values with `..Default::default()` and cost nothing at runtime. All fields are already `Default`-able (`String`, numeric, `Vec`, `Option`).

**Files:**
- Modify: `src/rd_client.rs:86` (`Torrent`), `:110` (`TorrentInfo`), `:140` (`TorrentFile`), `:152` (`UnrestrictResponse`), `:173` (`AddMagnetResponse`)
- Test: `src/rd_client.rs` (existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/rd_client.rs`:

```rust
#[test]
fn rd_structs_have_default() {
    let t = Torrent::default();
    assert_eq!(t.id, "");
    let info = TorrentInfo::default();
    assert!(info.files.is_empty());
    let f = TorrentFile::default();
    assert_eq!(f.selected, 0);
    let u = UnrestrictResponse::default();
    assert_eq!(u.download, "");
    let m = AddMagnetResponse::default();
    assert_eq!(m.id, "");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib rd_structs_have_default`
Expected: FAIL to compile with `the trait bound \`Torrent: Default\` is not satisfied` (and similar).

- [ ] **Step 3: Add `Default` to each derive**

In `src/rd_client.rs` change these five derive attributes:

- Line 86: `#[derive(Debug, Clone, Deserialize)]` → `#[derive(Debug, Clone, Default, Deserialize)]` (for `Torrent`)
- Line 110: `#[derive(Debug, Clone, Deserialize, Serialize)]` → `#[derive(Debug, Clone, Default, Deserialize, Serialize)]` (for `TorrentInfo`)
- Line 140: `#[derive(Debug, Clone, Deserialize, Serialize)]` → `#[derive(Debug, Clone, Default, Deserialize, Serialize)]` (for `TorrentFile`)
- Line 152: `#[derive(Debug, Clone, Deserialize)]` → `#[derive(Debug, Clone, Default, Deserialize)]` (for `UnrestrictResponse`)
- Line 173: `#[derive(Debug, Clone, Deserialize)]` → `#[derive(Debug, Clone, Default, Deserialize)]` (for `AddMagnetResponse`)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib rd_structs_have_default`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/rd_client.rs
git commit -m "feat: derive Default on RD data structs for test mocking"
```

---

## Task 3: Define the `DebridProvider` trait and implement it for `RealDebridClient`

The trait must declare ALL methods at once (you cannot implement a trait partially), so this task defines the trait and the RD impl together. The trait requires `Debug` as a supertrait because `RepairManager` derives `Debug` and will hold a trait object.

**Files:**
- Create: `src/provider.rs`
- Modify: `src/mapper.rs:1-9` (add module declaration)
- Modify: `src/rd_client.rs` (append the trait impl near the bottom, after the inherent `impl RealDebridClient { ... }` block that ends at line 564, before the `#[cfg(test)]` module)

- [ ] **Step 1: Write the failing test**

Create `src/provider.rs` with ONLY this test first (the rest of the file is added in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::RealDebridClient;
    use std::sync::Arc;

    #[test]
    fn real_debrid_client_is_a_debrid_provider() {
        let client = RealDebridClient::new("fake-token".to_string()).unwrap();
        let provider: Arc<dyn DebridProvider> = Arc::new(client);
        assert_eq!(provider.name(), "real-debrid");
    }
}
```

Add the module declaration to `src/mapper.rs` (keep the list alphabetical — insert between `jellyfin_client` and `rd_client`):

```rust
pub mod provider;
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib real_debrid_client_is_a_debrid_provider`
Expected: FAIL to compile — `cannot find trait \`DebridProvider\` in this scope`.

- [ ] **Step 3: Define the trait**

Insert ABOVE the `#[cfg(test)] mod tests` block in `src/provider.rs`:

```rust
use crate::error::AppError;
use crate::rd_client::{AddMagnetResponse, Torrent, TorrentInfo, UnrestrictResponse};
use async_trait::async_trait;

/// Abstraction over a debrid provider (Real-Debrid today, TorBox in a later phase).
///
/// The method set mirrors the Real-Debrid operations the codebase calls today so
/// this phase is a pure refactor. It is widened/reshaped in later phases.
///
/// `Debug` is a supertrait because `RepairManager` derives `Debug` while holding
/// an `Arc<dyn DebridProvider>`.
#[async_trait]
pub trait DebridProvider: Send + Sync + std::fmt::Debug {
    /// Stable, human-readable provider identifier (e.g. "real-debrid").
    fn name(&self) -> &'static str;

    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error>;
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error>;
    async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error>;
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error>;
    async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error>;
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error>;

    /// Remove a single cached resolution (RD: the unrestrict cache entry for `link`).
    async fn invalidate_unrestrict_cache(&self, link: &str);
    /// Evict expired cached resolutions.
    async fn evict_expired_cache(&self);
}

/// Suppress dead-code warnings for the imported types until later phases use them
/// directly here; the trait signatures reference them, so they are not unused.
#[allow(unused_imports)]
use crate::error::AppError as _KeepAppErrorImported;
```

> NOTE: `AppError` is imported now because `choose_provider` (Task 5) returns it. If `cargo build` warns that `AppError`/`_KeepAppErrorImported` is unused after this task, that is acceptable and is resolved in Task 5; alternatively delete the two `AppError` lines now and re-add the single `use crate::error::AppError;` in Task 5. Prefer deleting now to keep the build warning-free.

Now append the trait implementation to `src/rd_client.rs`, immediately AFTER the closing `}` of `impl RealDebridClient` (line 564) and BEFORE `#[cfg(test)]` (line 566):

```rust
#[async_trait::async_trait]
impl crate::provider::DebridProvider for RealDebridClient {
    fn name(&self) -> &'static str {
        "real-debrid"
    }

    // Each call uses method-call syntax on `&RealDebridClient`, which resolves to
    // the inherent method (inherent methods take priority over trait methods),
    // so these delegate rather than recurse.
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        self.get_torrents().await
    }
    async fn get_torrent_info(&self, id: &str) -> Result<TorrentInfo, reqwest::Error> {
        self.get_torrent_info(id).await
    }
    async fn unrestrict_link(&self, link: &str) -> Result<UnrestrictResponse, reqwest::Error> {
        self.unrestrict_link(link).await
    }
    async fn add_magnet(&self, magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        self.add_magnet(magnet).await
    }
    async fn select_files(&self, torrent_id: &str, file_ids: &str) -> Result<(), reqwest::Error> {
        self.select_files(torrent_id, file_ids).await
    }
    async fn delete_torrent(&self, torrent_id: &str) -> Result<(), reqwest::Error> {
        self.delete_torrent(torrent_id).await
    }
    async fn invalidate_unrestrict_cache(&self, link: &str) {
        self.invalidate_unrestrict_cache(link).await
    }
    async fn evict_expired_cache(&self) {
        self.evict_expired_cache().await
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib real_debrid_client_is_a_debrid_provider`
Expected: PASS. Then run `cargo build` and confirm no warnings (delete the `AppError` placeholder import lines from `provider.rs` if they warn — see the NOTE).

- [ ] **Step 5: Commit**

```bash
git add src/provider.rs src/mapper.rs src/rd_client.rs
git commit -m "feat: add DebridProvider trait, implemented by RealDebridClient"
```

---

## Task 4: Add a test-only `MockProvider`

A mock implementation enables unit tests of the consumers without network access. Gated `#[cfg(test)]` so it is not compiled into release binaries; it is still reachable from any in-crate unit test (e.g. `repair.rs` tests) during `cargo test`.

**Files:**
- Modify: `src/provider.rs` (add the mock above the existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/provider.rs`:

```rust
#[tokio::test]
async fn mock_provider_returns_canned_values() {
    let mock = MockProvider {
        torrents: vec![Torrent {
            id: "t1".to_string(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let provider: Arc<dyn DebridProvider> = Arc::new(mock);
    assert_eq!(provider.name(), "mock");
    let torrents = provider.get_torrents().await.unwrap();
    assert_eq!(torrents.len(), 1);
    assert_eq!(torrents[0].id, "t1");
    // Methods with no canned value return defaults / no-ops.
    assert_eq!(provider.get_torrent_info("x").await.unwrap().id, "");
    provider.invalidate_unrestrict_cache("x").await;
    provider.evict_expired_cache().await;
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib mock_provider_returns_canned_values`
Expected: FAIL to compile — `cannot find struct \`MockProvider\``.

- [ ] **Step 3: Implement the mock**

Insert ABOVE `#[cfg(test)] mod tests` in `src/provider.rs`:

```rust
/// Test-only in-memory provider. Returns configured canned values; unconfigured
/// methods return `Default`s or are no-ops. Not compiled into release builds.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct MockProvider {
    pub torrents: Vec<Torrent>,
    pub torrent_info: Option<TorrentInfo>,
    pub unrestrict: Option<UnrestrictResponse>,
    pub add_magnet: Option<AddMagnetResponse>,
}

#[cfg(test)]
#[async_trait]
impl DebridProvider for MockProvider {
    fn name(&self) -> &'static str {
        "mock"
    }
    async fn get_torrents(&self) -> Result<Vec<Torrent>, reqwest::Error> {
        Ok(self.torrents.clone())
    }
    async fn get_torrent_info(&self, _id: &str) -> Result<TorrentInfo, reqwest::Error> {
        Ok(self.torrent_info.clone().unwrap_or_default())
    }
    async fn unrestrict_link(&self, _link: &str) -> Result<UnrestrictResponse, reqwest::Error> {
        Ok(self.unrestrict.clone().unwrap_or_default())
    }
    async fn add_magnet(&self, _magnet: &str) -> Result<AddMagnetResponse, reqwest::Error> {
        Ok(self.add_magnet.clone().unwrap_or_default())
    }
    async fn select_files(&self, _torrent_id: &str, _file_ids: &str) -> Result<(), reqwest::Error> {
        Ok(())
    }
    async fn delete_torrent(&self, _torrent_id: &str) -> Result<(), reqwest::Error> {
        Ok(())
    }
    async fn invalidate_unrestrict_cache(&self, _link: &str) {}
    async fn evict_expired_cache(&self) {}
}
```

Also ensure the top-of-file imports in `src/provider.rs` include what the mock needs (add if missing): the test module already imports `Arc`; the mock itself uses the same `Torrent`/`TorrentInfo`/`UnrestrictResponse`/`AddMagnetResponse` already imported at the top for the trait.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib mock_provider_returns_canned_values`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/provider.rs
git commit -m "test: add MockProvider implementing DebridProvider"
```

---

## Task 5: Add the pure `choose_provider` selection function

Pure function (no env access) so it is fully unit-testable. `main.rs` wires it to the environment in Task 9.

**Files:**
- Modify: `src/provider.rs` (add `ProviderKind` + `choose_provider` above the mock)

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `src/provider.rs`:

```rust
#[test]
fn choose_provider_rd_only() {
    let (kind, token) =
        choose_provider(Some("rd-token".to_string()), None).unwrap();
    assert_eq!(kind, ProviderKind::RealDebrid);
    assert_eq!(token, "rd-token");
}

#[test]
fn choose_provider_torbox_only() {
    let (kind, token) =
        choose_provider(None, Some("tb-token".to_string())).unwrap();
    assert_eq!(kind, ProviderKind::TorBox);
    assert_eq!(token, "tb-token");
}

#[test]
fn choose_provider_both_set_is_error() {
    let err = choose_provider(Some("a".to_string()), Some("b".to_string()));
    assert!(err.is_err());
}

#[test]
fn choose_provider_neither_set_is_error() {
    assert!(choose_provider(None, None).is_err());
}

#[test]
fn choose_provider_treats_blank_token_as_unset() {
    // Whitespace-only RD token + real TorBox token → TorBox, not "both set".
    let (kind, _) =
        choose_provider(Some("   ".to_string()), Some("tb".to_string())).unwrap();
    assert_eq!(kind, ProviderKind::TorBox);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib choose_provider`
Expected: FAIL to compile — `cannot find function \`choose_provider\`` / `cannot find type \`ProviderKind\``.

- [ ] **Step 3: Implement the function**

Ensure `src/provider.rs` has `use crate::error::AppError;` at the top (re-add it if you deleted the placeholder in Task 3). Then insert above the mock:

```rust
/// Which provider the service should run against this deployment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    RealDebrid,
    TorBox,
}

/// Decide the active provider from the two optional tokens. Exactly one must be
/// set (non-blank). Both-set or neither-set is a configuration error.
pub fn choose_provider(
    rd_token: Option<String>,
    torbox_token: Option<String>,
) -> Result<(ProviderKind, String), AppError> {
    let rd = rd_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let tb = torbox_token
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    match (rd, tb) {
        (Some(_), Some(_)) => Err(AppError::Config(
            "Set only one of RD_API_TOKEN or TORBOX_API_KEY, not both".to_string(),
        )),
        (Some(token), None) => Ok((ProviderKind::RealDebrid, token)),
        (None, Some(token)) => Ok((ProviderKind::TorBox, token)),
        (None, None) => Err(AppError::Config(
            "Set one of RD_API_TOKEN or TORBOX_API_KEY".to_string(),
        )),
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib choose_provider`
Expected: all five PASS.

- [ ] **Step 5: Commit**

```bash
git add src/provider.rs
git commit -m "feat: add choose_provider startup selection logic"
```

---

## Task 6: Migrate `RepairManager` to `Arc<dyn DebridProvider>`

**Files:**
- Modify: `src/repair.rs:1` (import), `:35` (field), `:42` (constructor param)
- Test: `src/repair.rs` (existing `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/repair.rs`:

```rust
#[test]
fn repair_manager_accepts_trait_object() {
    use crate::provider::{DebridProvider, MockProvider};
    let provider: std::sync::Arc<dyn DebridProvider> =
        std::sync::Arc::new(MockProvider::default());
    let _manager = RepairManager::new(provider);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib repair_manager_accepts_trait_object`
Expected: FAIL to compile — `expected struct \`Arc<RealDebridClient>\`, found \`Arc<dyn DebridProvider>\``.

- [ ] **Step 3: Change the import, field, and constructor**

In `src/repair.rs`:

- Line 1: change
  ```rust
  use crate::rd_client::{RealDebridClient, TorrentInfo};
  ```
  to
  ```rust
  use crate::provider::DebridProvider;
  use crate::rd_client::TorrentInfo;
  ```

- Line 35 (the field inside `pub struct RepairManager`): change
  ```rust
      rd_client: Arc<RealDebridClient>,
  ```
  to
  ```rust
      rd_client: Arc<dyn DebridProvider>,
  ```

- Line 42 (constructor): change
  ```rust
      pub fn new(rd_client: Arc<RealDebridClient>) -> Self {
  ```
  to
  ```rust
      pub fn new(rd_client: Arc<dyn DebridProvider>) -> Self {
  ```

All existing `self.rd_client.<method>(...)` call sites (lines 64, 171, 184, 216, 230, 340, 361, 388, 451) are unchanged — every method they call (`delete_torrent`, `add_magnet`, `get_torrent_info`, `select_files`) is on the trait.

- [ ] **Step 4: Run the test and the repair suite to verify they pass**

Run: `cargo test --lib repair`
Expected: PASS, including all pre-existing repair unit tests (they construct `RepairManager` — see Step 5 note).

> If any pre-existing repair test constructs `RepairManager::new(Arc::new(RealDebridClient::new(...)))`, it still compiles: `Arc<RealDebridClient>` coerces to `Arc<dyn DebridProvider>` at the call site because `RealDebridClient: DebridProvider`. No test changes needed. If a test stored the client in a `let` typed as `Arc<RealDebridClient>` and reused it elsewhere expecting inherent methods, leave that as-is — inherent methods still work on the concrete type.

- [ ] **Step 5: Commit**

```bash
git add src/repair.rs
git commit -m "refactor: RepairManager depends on Arc<dyn DebridProvider>"
```

---

## Task 7: Migrate `DebridFileSystem` and `ProxiedMediaFile` to `Arc<dyn DebridProvider>`

**Files:**
- Modify: `src/dav_fs.rs:1` (import), `:21` (`DebridFileSystem.rd_client` field), `:28` (`new` param), and the `rd_client` field on the `ProxiedMediaFile` struct (locate it — see Step 3)

- [ ] **Step 1: Write the failing test**

Add a new `#[cfg(test)] mod tests` block at the end of `src/dav_fs.rs` (or extend an existing one if present):

```rust
#[cfg(test)]
mod provider_abstraction_tests {
    use super::*;
    use crate::provider::{DebridProvider, MockProvider};
    use crate::repair::RepairManager;
    use crate::vfs::DebridVfs;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[test]
    fn debrid_filesystem_accepts_trait_object() {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let vfs = Arc::new(RwLock::new(DebridVfs::new()));
        let repair = Arc::new(RepairManager::new(provider.clone()));
        let http = reqwest::Client::new();
        let _fs = DebridFileSystem::new(provider, vfs, repair, http);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib debrid_filesystem_accepts_trait_object`
Expected: FAIL to compile — `DebridFileSystem::new` expects `Arc<RealDebridClient>`.

- [ ] **Step 3: Change the import and both struct field types**

In `src/dav_fs.rs`:

- Line 1: change
  ```rust
  use crate::rd_client::RealDebridClient;
  ```
  to
  ```rust
  use crate::provider::DebridProvider;
  ```

- Line 21 (field in `pub struct DebridFileSystem`): change
  ```rust
      rd_client: Arc<RealDebridClient>,
  ```
  to
  ```rust
      rd_client: Arc<dyn DebridProvider>,
  ```

- Line 28 (`new` parameter): change
  ```rust
      pub fn new(
          rd_client: Arc<RealDebridClient>,
  ```
  to
  ```rust
      pub fn new(
          rd_client: Arc<dyn DebridProvider>,
  ```

- Locate the `ProxiedMediaFile` struct definition (search `struct ProxiedMediaFile` in `src/dav_fs.rs`; it is the struct constructed at lines 94-106 and therefore has a field `rd_client: Arc<RealDebridClient>`). Change that one field declaration from
  ```rust
      rd_client: Arc<RealDebridClient>,
  ```
  to
  ```rust
      rd_client: Arc<dyn DebridProvider>,
  ```

No call-site changes are needed: `unrestrict_link` (lines 256, 293) and `invalidate_unrestrict_cache` (lines 289, 409) are both trait methods. The 503 check `should_repair_on_unrestrict_error` (lines 228-230) inspects a `reqwest::StatusCode` and is unaffected (the trait still returns `reqwest::Error`).

- [ ] **Step 4: Run test and dav_fs suite to verify they pass**

Run: `cargo test --lib dav_fs` then `cargo test --lib debrid_filesystem_accepts_trait_object`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/dav_fs.rs
git commit -m "refactor: DebridFileSystem and ProxiedMediaFile use Arc<dyn DebridProvider>"
```

---

## Task 8: Migrate `ScanConfig` to `Arc<dyn DebridProvider>`

**Files:**
- Modify: `src/tasks.rs:2` (import), `:17` (`ScanConfig.rd_client` field)
- Test: `src/tasks.rs` (`#[cfg(test)] mod tests` — create if none exists)

- [ ] **Step 1: Write the failing test**

Add (or create) a `#[cfg(test)] mod tests` block at the end of `src/tasks.rs`:

```rust
#[cfg(test)]
mod provider_abstraction_tests {
    use super::*;
    use crate::provider::{DebridProvider, MockProvider};
    use crate::repair::RepairManager;
    use crate::tmdb_client::TmdbClient;
    use crate::vfs::DebridVfs;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    #[test]
    fn scan_config_holds_trait_object() {
        let provider: Arc<dyn DebridProvider> = Arc::new(MockProvider::default());
        let db = Arc::new(redb::Database::builder().create_with_backend(
            redb::backends::InMemoryBackend::new()).unwrap());
        let _config = ScanConfig {
            rd_client: provider.clone(),
            tmdb_client: Arc::new(TmdbClient::new("k".to_string())),
            vfs: Arc::new(RwLock::new(DebridVfs::new())),
            db,
            repair_manager: Arc::new(RepairManager::new(provider)),
            interval_secs: 60,
            jellyfin_client: None,
        };
    }
}
```

> If `redb::backends::InMemoryBackend` is unavailable in this redb version, replace the `db` line with a temp-file database:
> ```rust
> let dir = std::env::temp_dir().join(format!("dmm-test-{}.redb", std::process::id()));
> let db = Arc::new(redb::Database::create(&dir).unwrap());
> ```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test --lib scan_config_holds_trait_object`
Expected: FAIL to compile — `rd_client` field type mismatch.

- [ ] **Step 3: Change the import and field type**

In `src/tasks.rs`:

- Line 2: change
  ```rust
  use crate::rd_client::RealDebridClient;
  ```
  to
  ```rust
  use crate::provider::DebridProvider;
  ```

- Line 17 (field in `pub struct ScanConfig`): change
  ```rust
      pub rd_client: Arc<RealDebridClient>,
  ```
  to
  ```rust
      pub rd_client: Arc<dyn DebridProvider>,
  ```

The destructure at lines 27-35 and the calls `rd_client.get_torrents()` (102), `rd_client.get_torrent_info(...)` (156, 239), and `rd_client.delete_torrent(...)` (127) all remain valid — these are trait methods. References to `crate::rd_client::TorrentInfo` (lines 38, 48) are unchanged; that type still lives in `rd_client`.

- [ ] **Step 4: Run test and tasks suite to verify they pass**

Run: `cargo test --lib tasks` then `cargo test --lib scan_config_holds_trait_object`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/tasks.rs
git commit -m "refactor: ScanConfig holds Arc<dyn DebridProvider>"
```

---

## Task 9: Wire provider selection into `main.rs`

**Files:**
- Modify: `src/main.rs:1-7` (imports), `:41-44` (token read → selection), `:63` (client construction)

- [ ] **Step 1: Update imports**

In `src/main.rs`, keep `use debridmoviemapper::rd_client::RealDebridClient;` (line 3) and add below it:

```rust
use debridmoviemapper::provider::{choose_provider, DebridProvider, ProviderKind};
```

- [ ] **Step 2: Replace the RD token read with provider selection**

Replace lines 41-44:

```rust
    let api_token = std::env::var("RD_API_TOKEN")
        .expect("RD_API_TOKEN must be set")
        .trim()
        .to_string();
```

with:

```rust
    let (provider_kind, provider_token) = choose_provider(
        std::env::var("RD_API_TOKEN").ok(),
        std::env::var("TORBOX_API_KEY").ok(),
    )
    .unwrap_or_else(|e| {
        eprintln!("Configuration error: {}", e);
        std::process::exit(1);
    });
```

- [ ] **Step 3: Replace the client construction**

Replace line 63:

```rust
    let rd_client = Arc::new(RealDebridClient::new(api_token)?);
```

with:

```rust
    let provider: Arc<dyn DebridProvider> = match provider_kind {
        ProviderKind::RealDebrid => Arc::new(RealDebridClient::new(provider_token)?),
        ProviderKind::TorBox => {
            eprintln!("TorBox support is not yet available in this build");
            std::process::exit(1);
        }
    };
```

Then update the three downstream uses of the old `rd_client` variable to use `provider`:

- Line 66: `RepairManager::new(rd_client.clone())` → `RepairManager::new(provider.clone())`
- Lines 93 (`ScanConfig { rd_client: rd_client.clone(), ...}`) → `rd_client: provider.clone(),`
- Lines 108-113 (`DebridFileSystem::new(rd_client.clone(), ...)`) → `DebridFileSystem::new(provider.clone(), ...)`

(Search `rd_client` in `src/main.rs` to confirm no other references remain.)

- [ ] **Step 4: Verify the build and full test suite**

Run: `cargo build`
Expected: builds with no errors and no warnings.

Run: `cargo test`
Expected: all unit tests PASS.

- [ ] **Step 5: Manual sanity checks**

Run each and confirm the described behaviour (no real tokens needed):

```bash
# Neither token → clear error, exit 1
env -u RD_API_TOKEN -u TORBOX_API_KEY TMDB_API_KEY=x cargo run 2>&1 | head -3
# Expected: "Configuration error: Set one of RD_API_TOKEN or TORBOX_API_KEY"

# Both tokens → clear error, exit 1
RD_API_TOKEN=a TORBOX_API_KEY=b TMDB_API_KEY=x cargo run 2>&1 | head -3
# Expected: "Configuration error: Set only one of RD_API_TOKEN or TORBOX_API_KEY, not both"

# TorBox only → "not yet available", exit 1
env -u RD_API_TOKEN TORBOX_API_KEY=b TMDB_API_KEY=x cargo run 2>&1 | head -3
# Expected: "TorBox support is not yet available in this build"
```

- [ ] **Step 6: Commit**

```bash
git add src/main.rs
git commit -m "feat: select debrid provider at startup from RD_API_TOKEN/TORBOX_API_KEY"
```

---

## Task 10: Update documentation for Phase 1

**Files:**
- Modify: `CLAUDE.md` (module table; env vars; design decisions)
- Modify: `README.md` (env var documentation)

- [ ] **Step 1: Update `CLAUDE.md`**

- In the module-responsibilities table, add a row:
  | `provider.rs` | `DebridProvider` trait abstracting the debrid backend; startup provider selection (`choose_provider`); test-only `MockProvider` |
- In the "Optional" env vars list (or a new "Provider selection" note), document: exactly one of `RD_API_TOKEN` or `TORBOX_API_KEY` must be set; setting both is a startup error. Note that `TORBOX_API_KEY` is recognised but TorBox is not yet functional (lands in a later phase).
- Under "Key Design Decisions", add a bullet: "Provider abstraction: all components depend on `Arc<dyn DebridProvider>`; `RealDebridClient` is one implementation. One provider is active per deployment, chosen at startup."

- [ ] **Step 2: Update `README.md`**

Document the `TORBOX_API_KEY` env var alongside `RD_API_TOKEN`, and state the "exactly one provider" rule.

- [ ] **Step 3: Verify the pre-commit gate**

Run the documented gate:

```bash
cargo test
```

Expected: PASS. (Integration tests are unaffected by Phase 1 and need not be re-run for a docs-only change, but running `cargo test` confirms the unit suite is green.)

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: document provider abstraction and TORBOX_API_KEY (Phase 1)"
```

---

## Self-review

**Spec coverage (Phase 1 portion of the spec):**
- Provider abstraction via trait + `Arc<dyn DebridProvider>` → Tasks 3, 6, 7, 8. ✓
- `RealDebridClient` implements the trait → Task 3. ✓
- `MockProvider` for TDD → Task 4. ✓
- Single-provider selection; both-set is a hard error; neither is an error → Tasks 5, 9. ✓
- `async_trait` dependency → Task 1. ✓
- Docs in sync (CLAUDE.md/README.md) → Task 10. ✓
- Deferred by design (documented in "Phase 1 scope"): canonical `FileLocator`, `resolve_url`/`check_cached`/`add_by_hash`, moving model types into `provider.rs`, durable catalogue, generalised re-acquire, TorBox client. These are Phases 2–4.

**Placeholder scan:** No `TBD`/`TODO`/"add error handling" — every code step contains complete code. The single conditional note (Task 8 redb in-memory fallback; Task 3 `AppError` import cleanup) gives an explicit alternative, not a placeholder.

**Type consistency:** Trait name `DebridProvider`; method names match `RealDebridClient`'s inherent methods exactly (`get_torrents`, `get_torrent_info`, `unrestrict_link`, `add_magnet`, `select_files`, `delete_torrent`, `invalidate_unrestrict_cache`, `evict_expired_cache`) so the delegating impl compiles. `choose_provider(Option<String>, Option<String>) -> Result<(ProviderKind, String), AppError>` is used identically in its tests (Task 5) and in `main.rs` (Task 9). `ProviderKind::{RealDebrid, TorBox}` consistent across Tasks 5 and 9. Field type `Arc<dyn DebridProvider>` consistent across `RepairManager`, `DebridFileSystem`, `ProxiedMediaFile`, `ScanConfig`.
