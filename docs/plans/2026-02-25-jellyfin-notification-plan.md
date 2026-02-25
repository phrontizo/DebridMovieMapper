# Jellyfin Library Notification — Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Notify Jellyfin of specific changed paths via `POST /Library/Media/Updated` whenever the VFS is updated, so new/removed/changed content is detected immediately without a full library rescan.

**Architecture:** Add a VFS tree diff to `update_vfs` that compares old vs new root nodes. When changes are detected and Jellyfin is configured, fire a single batched HTTP POST with all changed paths. A new `JellyfinClient` handles the HTTP call. The diff and client are fully unit-testable without a real Jellyfin instance.

**Tech Stack:** `reqwest` (already a dependency), `serde_json` (already a dependency), `tracing` (already a dependency)

---

### Task 1: Add `PartialEq` to `VfsNode` and `VfsChange` types

**Files:**
- Modify: `src/vfs.rs:21` (add derive)
- Modify: `src/vfs.rs` (add new types after `VfsNode` enum, around line 37)

**Step 1: Add `PartialEq` derive to `VfsNode`**

In `src/vfs.rs:21`, change:
```rust
#[derive(Debug, Clone)]
pub enum VfsNode {
```
to:
```rust
#[derive(Debug, Clone, PartialEq)]
pub enum VfsNode {
```

**Step 2: Add `VfsChange` and `UpdateType` types**

After the `VfsNode` enum (after line 37), add:
```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateType {
    Created,
    Modified,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VfsChange {
    pub path: String,
    pub update_type: UpdateType,
}
```

**Step 3: Run tests**

Run: `cargo test`
Expected: All existing tests pass (PartialEq is additive, doesn't break anything).

**Step 4: Commit**

```bash
git add src/vfs.rs
git commit -m "feat: add PartialEq to VfsNode and VfsChange types"
```

---

### Task 2: Implement `diff_trees` with TDD

**Files:**
- Modify: `src/vfs.rs` (add `diff_trees` function and tests)

**Step 1: Write failing tests for `diff_trees`**

Add to the `mod tests` block in `src/vfs.rs`:

```rust
#[test]
fn diff_trees_identical_trees_no_changes() {
    let old = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Inception".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Inception.strm".to_string(), VfsNode::StrmFile {
                                strm_content: vec![],
                                rd_link: "http://link1".to_string(),
                                rd_torrent_id: "t1".to_string(),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let changes = diff_trees(&old, &old, "");
    assert!(changes.is_empty(), "Identical trees should produce no changes");
}

#[test]
fn diff_trees_new_directory_created() {
    let old = VfsNode::Directory {
        children: BTreeMap::from([
            ("Shows".to_string(), VfsNode::Directory { children: BTreeMap::new() }),
        ]),
    };
    let new = VfsNode::Directory {
        children: BTreeMap::from([
            ("Shows".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Breaking Bad".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Season 01".to_string(), VfsNode::Directory {
                                children: BTreeMap::from([
                                    ("S01E01.strm".to_string(), VfsNode::StrmFile {
                                        strm_content: vec![],
                                        rd_link: "http://link".to_string(),
                                        rd_torrent_id: "t1".to_string(),
                                    }),
                                ]),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let changes = diff_trees(&old, &new, "");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "Shows/Breaking Bad/Season 01");
    assert_eq!(changes[0].update_type, UpdateType::Created);
}

#[test]
fn diff_trees_directory_deleted() {
    let old = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Inception".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Inception.strm".to_string(), VfsNode::StrmFile {
                                strm_content: vec![],
                                rd_link: "http://link".to_string(),
                                rd_torrent_id: "t1".to_string(),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let new = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory { children: BTreeMap::new() }),
        ]),
    };
    let changes = diff_trees(&old, &new, "");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "Movies/Inception");
    assert_eq!(changes[0].update_type, UpdateType::Deleted);
}

#[test]
fn diff_trees_file_modified() {
    let old = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Inception".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Inception.strm".to_string(), VfsNode::StrmFile {
                                strm_content: vec![],
                                rd_link: "http://old-link".to_string(),
                                rd_torrent_id: "t1".to_string(),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let new = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Inception".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Inception.strm".to_string(), VfsNode::StrmFile {
                                strm_content: vec![],
                                rd_link: "http://new-link".to_string(),
                                rd_torrent_id: "t2".to_string(),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let changes = diff_trees(&old, &new, "");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "Movies/Inception");
    assert_eq!(changes[0].update_type, UpdateType::Modified);
}

#[test]
fn diff_trees_new_episode_in_existing_show() {
    let old = VfsNode::Directory {
        children: BTreeMap::from([
            ("Shows".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Breaking Bad".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Season 01".to_string(), VfsNode::Directory {
                                children: BTreeMap::from([
                                    ("S01E01.strm".to_string(), VfsNode::StrmFile {
                                        strm_content: vec![],
                                        rd_link: "http://link1".to_string(),
                                        rd_torrent_id: "t1".to_string(),
                                    }),
                                ]),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let new = VfsNode::Directory {
        children: BTreeMap::from([
            ("Shows".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Breaking Bad".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Season 01".to_string(), VfsNode::Directory {
                                children: BTreeMap::from([
                                    ("S01E01.strm".to_string(), VfsNode::StrmFile {
                                        strm_content: vec![],
                                        rd_link: "http://link1".to_string(),
                                        rd_torrent_id: "t1".to_string(),
                                    }),
                                    ("S01E02.strm".to_string(), VfsNode::StrmFile {
                                        strm_content: vec![],
                                        rd_link: "http://link2".to_string(),
                                        rd_torrent_id: "t2".to_string(),
                                    }),
                                ]),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let changes = diff_trees(&old, &new, "");
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].path, "Shows/Breaking Bad/Season 01");
    assert_eq!(changes[0].update_type, UpdateType::Modified);
}

#[test]
fn diff_trees_multiple_changes() {
    let old = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("Old Movie".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("old.strm".to_string(), VfsNode::StrmFile {
                                strm_content: vec![],
                                rd_link: "http://old".to_string(),
                                rd_torrent_id: "t1".to_string(),
                            }),
                        ]),
                    }),
                ]),
            }),
            ("Shows".to_string(), VfsNode::Directory { children: BTreeMap::new() }),
        ]),
    };
    let new = VfsNode::Directory {
        children: BTreeMap::from([
            ("Movies".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("New Movie".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("new.strm".to_string(), VfsNode::StrmFile {
                                strm_content: vec![],
                                rd_link: "http://new".to_string(),
                                rd_torrent_id: "t2".to_string(),
                            }),
                        ]),
                    }),
                ]),
            }),
            ("Shows".to_string(), VfsNode::Directory {
                children: BTreeMap::from([
                    ("New Show".to_string(), VfsNode::Directory {
                        children: BTreeMap::from([
                            ("Season 01".to_string(), VfsNode::Directory {
                                children: BTreeMap::from([
                                    ("S01E01.strm".to_string(), VfsNode::StrmFile {
                                        strm_content: vec![],
                                        rd_link: "http://ep".to_string(),
                                        rd_torrent_id: "t3".to_string(),
                                    }),
                                ]),
                            }),
                        ]),
                    }),
                ]),
            }),
        ]),
    };
    let changes = diff_trees(&old, &new, "");
    // Old Movie deleted, New Movie created, New Show/Season 01 created
    assert_eq!(changes.len(), 3);
    let paths: Vec<&str> = changes.iter().map(|c| c.path.as_str()).collect();
    assert!(paths.contains(&"Movies/Old Movie"));
    assert!(paths.contains(&"Movies/New Movie"));
    assert!(paths.contains(&"Shows/New Show/Season 01"));
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test diff_trees`
Expected: FAIL — `diff_trees` function not found.

**Step 3: Implement `diff_trees`**

Add this public function in `src/vfs.rs` (before the `tests` module):

```rust
/// Diff two VFS trees and return the list of changed paths at the deepest
/// meaningful level (e.g. season folder for a new episode, movie folder for
/// a new movie).
pub fn diff_trees(old: &VfsNode, new: &VfsNode, prefix: &str) -> Vec<VfsChange> {
    let mut changes = Vec::new();

    let (old_children, new_children) = match (old, new) {
        (VfsNode::Directory { children: old_c }, VfsNode::Directory { children: new_c }) => {
            (old_c, new_c)
        }
        // Both are the same leaf — compare for equality
        (a, b) if a == b => return changes,
        // Both are leaves but different — parent will report this
        _ => return changes,
    };

    for (name, old_child) in old_children {
        let child_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };

        match new_children.get(name) {
            None => {
                // Entire subtree was removed — report the top-level removed entry
                changes.push(VfsChange {
                    path: child_path,
                    update_type: UpdateType::Deleted,
                });
            }
            Some(new_child) => {
                match (old_child, new_child) {
                    (VfsNode::Directory { .. }, VfsNode::Directory { .. }) => {
                        // Both directories — recurse
                        let sub = diff_trees(old_child, new_child, &child_path);
                        if sub.is_empty() && old_child != new_child {
                            // Children differ but recursion found no deeper change point
                            // — this directory itself is the change point
                            changes.push(VfsChange {
                                path: child_path,
                                update_type: UpdateType::Modified,
                            });
                        } else {
                            changes.extend(sub);
                        }
                    }
                    (a, b) if a != b => {
                        // Leaf node changed — report the parent directory
                        // The parent will be reported by the caller since we return
                        // the fact that something changed
                        changes.push(VfsChange {
                            path: child_path,
                            update_type: UpdateType::Modified,
                        });
                    }
                    _ => {
                        // Identical leaf nodes — no change
                    }
                }
            }
        }
    }

    // Keys only in new — created
    for (name, new_child) in new_children {
        if !old_children.contains_key(name) {
            let child_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            // For new directories, report the deepest new directory
            // (walk down to find the deepest single-child path)
            if let VfsNode::Directory { children } = new_child {
                let deepest = find_deepest_new_dir(&child_path, children);
                changes.push(VfsChange {
                    path: deepest,
                    update_type: UpdateType::Created,
                });
            } else {
                changes.push(VfsChange {
                    path: child_path,
                    update_type: UpdateType::Created,
                });
            }
        }
    }

    changes
}

/// Walk down a newly created directory tree to find the deepest directory.
/// For "Shows/Breaking Bad/Season 01/S01E01.strm", returns "Shows/Breaking Bad/Season 01".
fn find_deepest_new_dir(path: &str, children: &BTreeMap<String, VfsNode>) -> String {
    // If all children are leaves (files), this is the deepest directory
    let dir_children: Vec<(&String, &BTreeMap<String, VfsNode>)> = children
        .iter()
        .filter_map(|(name, node)| {
            if let VfsNode::Directory { children: c } = node {
                Some((name, c))
            } else {
                None
            }
        })
        .collect();

    if dir_children.len() == 1 {
        let (name, sub_children) = dir_children[0];
        let sub_path = format!("{}/{}", path, name);
        find_deepest_new_dir(&sub_path, sub_children)
    } else {
        path.to_string()
    }
}
```

**Step 4: Run tests to verify they pass**

Run: `cargo test diff_trees`
Expected: All 6 `diff_trees` tests pass.

**Step 5: Run full test suite**

Run: `cargo test`
Expected: All tests pass.

**Step 6: Commit**

```bash
git add src/vfs.rs
git commit -m "feat: implement VFS tree diff for change detection"
```

---

### Task 3: Implement `JellyfinClient`

**Files:**
- Create: `src/jellyfin_client.rs`
- Modify: `src/mapper.rs:1` (add module declaration)

**Step 1: Write the failing test**

Create `src/jellyfin_client.rs` with tests first:

```rust
use crate::vfs::{VfsChange, UpdateType};
use tracing::info;

pub struct JellyfinClient {
    url: String,
    api_key: String,
    mount_path: String,
    http: reqwest::Client,
}

impl JellyfinClient {
    pub fn new(url: String, api_key: String, mount_path: String) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            api_key,
            mount_path: mount_path.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    fn build_request_body(&self, changes: &[VfsChange]) -> serde_json::Value {
        let updates: Vec<serde_json::Value> = changes
            .iter()
            .map(|change| {
                let full_path = format!("{}/{}", self.mount_path, change.path);
                let update_type = match change.update_type {
                    UpdateType::Created => "Created",
                    UpdateType::Modified => "Modified",
                    UpdateType::Deleted => "Deleted",
                };
                serde_json::json!({
                    "Path": full_path,
                    "UpdateType": update_type
                })
            })
            .collect();

        serde_json::json!({ "Updates": updates })
    }

    pub async fn notify_changes(&self, changes: &[VfsChange]) {
        if changes.is_empty() {
            return;
        }

        let body = self.build_request_body(changes);
        let url = format!("{}/Library/Media/Updated", self.url);

        info!(
            "Notifying Jellyfin of {} change(s): {}",
            changes.len(),
            changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>().join(", ")
        );

        match self.http
            .post(&url)
            .header("X-Emby-Token", &self.api_key)
            .json(&body)
            .send()
            .await
        {
            Ok(response) => {
                if response.status().is_success() {
                    info!("Jellyfin notified successfully");
                } else {
                    tracing::warn!(
                        "Jellyfin notification returned status {}: {}",
                        response.status(),
                        response.text().await.unwrap_or_default()
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Failed to notify Jellyfin: {}", e);
            }
        }
    }

    /// Try to create a JellyfinClient from environment variables.
    /// Returns None if any of the required env vars are missing.
    pub fn from_env() -> Option<Self> {
        let url = std::env::var("JELLYFIN_URL").ok()?;
        let api_key = std::env::var("JELLYFIN_API_KEY").ok()?;
        let mount_path = std::env::var("JELLYFIN_RCLONE_MOUNT_PATH").ok()?;

        if url.is_empty() || api_key.is_empty() || mount_path.is_empty() {
            return None;
        }

        Some(Self::new(url, api_key, mount_path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_request_body_single_created() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        );
        let changes = vec![VfsChange {
            path: "Shows/Breaking Bad/Season 03".to_string(),
            update_type: UpdateType::Created,
        }];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["Path"], "/mnt/debrid/Shows/Breaking Bad/Season 03");
        assert_eq!(updates[0]["UpdateType"], "Created");
    }

    #[test]
    fn build_request_body_multiple_changes() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        );
        let changes = vec![
            VfsChange {
                path: "Movies/Old Movie".to_string(),
                update_type: UpdateType::Deleted,
            },
            VfsChange {
                path: "Movies/New Movie".to_string(),
                update_type: UpdateType::Created,
            },
            VfsChange {
                path: "Shows/Breaking Bad/Season 01".to_string(),
                update_type: UpdateType::Modified,
            },
        ];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates.len(), 3);
        assert_eq!(updates[0]["UpdateType"], "Deleted");
        assert_eq!(updates[1]["UpdateType"], "Created");
        assert_eq!(updates[2]["Path"], "/mnt/debrid/Shows/Breaking Bad/Season 01");
    }

    #[test]
    fn build_request_body_trims_trailing_slashes() {
        let client = JellyfinClient::new(
            "http://jellyfin:8096/".to_string(),
            "test-key".to_string(),
            "/mnt/debrid/".to_string(),
        );
        let changes = vec![VfsChange {
            path: "Movies/Test".to_string(),
            update_type: UpdateType::Created,
        }];
        let body = client.build_request_body(&changes);
        let updates = body["Updates"].as_array().unwrap();
        assert_eq!(updates[0]["Path"], "/mnt/debrid/Movies/Test");
    }

    #[test]
    fn notify_changes_skips_empty() {
        // Just verify it doesn't panic on empty input
        let client = JellyfinClient::new(
            "http://jellyfin:8096".to_string(),
            "test-key".to_string(),
            "/mnt/debrid".to_string(),
        );
        let body = client.build_request_body(&[]);
        let updates = body["Updates"].as_array().unwrap();
        assert!(updates.is_empty());
    }
}
```

**Step 2: Add module declaration**

In `src/mapper.rs`, add:
```rust
pub mod jellyfin_client;
```

**Step 3: Run tests**

Run: `cargo test jellyfin`
Expected: All 4 tests pass.

**Step 4: Run full test suite**

Run: `cargo test`
Expected: All tests pass.

**Step 5: Commit**

```bash
git add src/jellyfin_client.rs src/mapper.rs
git commit -m "feat: add JellyfinClient for library update notifications"
```

---

### Task 4: Integrate into `update_vfs` and `main.rs`

**Files:**
- Modify: `src/tasks.rs:180-196` (`update_vfs` function)
- Modify: `src/tasks.rs:16-23` (`run_scan_loop` signature)
- Modify: `src/main.rs:55-62` (pass jellyfin client to scan loop)

**Step 1: Update `update_vfs` to diff and notify**

Change `update_vfs` in `src/tasks.rs` (lines 180-196) to:

```rust
async fn update_vfs(
    vfs: &Arc<RwLock<DebridVfs>>,
    current_data: &[(crate::rd_client::TorrentInfo, MediaMetadata)],
    repair_manager: &Arc<RepairManager>,
    jellyfin_client: &Option<Arc<crate::jellyfin_client::JellyfinClient>>,
) {
    let mut filtered = Vec::new();
    for (torrent_info, metadata) in current_data {
        if !repair_manager.should_hide_torrent(&torrent_info.id).await {
            filtered.push((torrent_info.clone(), metadata.clone()));
        }
    }
    // Build VFS without holding the lock to avoid blocking WebDAV reads during scans
    let new_vfs = DebridVfs::build(filtered);
    // Diff old vs new, then swap
    let mut vfs_lock = vfs.write().await;
    let changes = crate::vfs::diff_trees(&vfs_lock.root, &new_vfs.root, "");
    *vfs_lock = new_vfs;
    drop(vfs_lock);

    if !changes.is_empty() {
        if let Some(client) = jellyfin_client {
            let client = client.clone();
            tokio::spawn(async move {
                client.notify_changes(&changes).await;
            });
        }
    }
}
```

**Step 2: Update `run_scan_loop` signature**

Add `jellyfin_client` parameter to `run_scan_loop` in `src/tasks.rs` (line 16):

```rust
pub async fn run_scan_loop(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>,
    db: Arc<redb::Database>,
    repair_manager: Arc<RepairManager>,
    interval_secs: u64,
    jellyfin_client: Option<Arc<crate::jellyfin_client::JellyfinClient>>,
) {
```

**Step 3: Update all `update_vfs` call sites in `run_scan_loop`**

There are two call sites in `tasks.rs`:

Line 160 (inside the stream processing loop):
```rust
update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client).await;
```

Line 164 (the else branch when no new torrents):
```rust
update_vfs(&vfs, &current_data, &repair_manager, &jellyfin_client).await;
```

**Step 4: Update the compile-time signature test**

In `src/tasks.rs` tests module (~line 204), update:
```rust
#[allow(dead_code)]
async fn _assert_run_scan_loop_signature(
    rd_client: Arc<RealDebridClient>,
    tmdb_client: Arc<TmdbClient>,
    vfs: Arc<RwLock<DebridVfs>>,
    db: Arc<redb::Database>,
    repair_manager: Arc<RepairManager>,
) {
    run_scan_loop(rd_client, tmdb_client, vfs, db, repair_manager, 60, None).await;
}
```

**Step 5: Wire up in `main.rs`**

After the `repair_manager` creation (around line 44 in `main.rs`), add:

```rust
let jellyfin_client = debridmoviemapper::jellyfin_client::JellyfinClient::from_env()
    .map(Arc::new);

if jellyfin_client.is_some() {
    info!("Jellyfin notification enabled");
} else {
    info!("Jellyfin notification disabled (set JELLYFIN_URL, JELLYFIN_API_KEY, JELLYFIN_RCLONE_MOUNT_PATH to enable)");
}
```

Update the `tokio::spawn` call (line 55) to pass the client:

```rust
tokio::spawn(debridmoviemapper::tasks::run_scan_loop(
    rd_client.clone(),
    tmdb_client.clone(),
    vfs.clone(),
    db.clone(),
    repair_manager.clone(),
    scan_interval_secs,
    jellyfin_client,
));
```

**Step 6: Run tests**

Run: `cargo test`
Expected: All tests pass.

**Step 7: Commit**

```bash
git add src/tasks.rs src/main.rs
git commit -m "feat: integrate Jellyfin notification into VFS update pipeline"
```

---

### Task 5: Update documentation and compose file

**Files:**
- Modify: `CLAUDE.md`
- Modify: `README.md`
- Modify: `compose.yml`

**Step 1: Update `CLAUDE.md`**

In the module responsibilities table, add a row:

```
| `jellyfin_client.rs` | Optional Jellyfin notification client — notifies Jellyfin of changed paths via `POST /Library/Media/Updated` |
```

In the "Optional" env vars section, add:

```
- `JELLYFIN_URL` — Jellyfin server URL (e.g. `http://jellyfin:8096`)
- `JELLYFIN_API_KEY` — Jellyfin API key
- `JELLYFIN_RCLONE_MOUNT_PATH` — rclone mount path as seen by Jellyfin (e.g. `/media`)
```

**Step 2: Update `README.md`**

Add the three new env vars to the environment variables table:

```markdown
| `JELLYFIN_URL` | No | - | Jellyfin server URL for library update notifications |
| `JELLYFIN_API_KEY` | No | - | Jellyfin API key for authentication |
| `JELLYFIN_RCLONE_MOUNT_PATH` | No | - | rclone mount path as seen by Jellyfin (e.g. `/media`) |
```

In the `.env` example block, add:

```env
# Optional: Jellyfin integration (all three required to enable)
JELLYFIN_URL=http://jellyfin:8096
JELLYFIN_API_KEY=your_jellyfin_api_key
JELLYFIN_RCLONE_MOUNT_PATH=/media
```

In the Features section, add a bullet:

```markdown
- **Jellyfin Notifications**: Optionally notifies Jellyfin when content changes so new episodes and movies appear immediately without waiting for a full library scan.
```

Add a new subsection under "How It Works":

```markdown
### Jellyfin Notifications

When `JELLYFIN_URL`, `JELLYFIN_API_KEY`, and `JELLYFIN_RCLONE_MOUNT_PATH` are all set, the service notifies Jellyfin of specific changed paths after each VFS update. This uses Jellyfin's `POST /Library/Media/Updated` API to trigger targeted scans of only the affected folders (e.g. a single season directory for a new episode), avoiding full library rescans. Changes from all sources — new torrents, deletions, repairs — are detected automatically.
```

In the project structure, add:

```markdown
- `src/jellyfin_client.rs`: Optional Jellyfin notification client for instant library updates.
```

**Step 3: Update `compose.yml`**

Add the Jellyfin env vars to the `debridmoviemapper` service:

```yaml
    environment:
      - RD_API_TOKEN=your_real_debrid_token
      - TMDB_API_KEY=your_tmdb_api_key
      - SCAN_INTERVAL_SECS=60
      # Jellyfin notifications (all three required to enable)
      - JELLYFIN_URL=http://jellyfin:8096
      - JELLYFIN_API_KEY=your_jellyfin_api_key
      - JELLYFIN_RCLONE_MOUNT_PATH=/media
```

**Step 4: Run tests**

Run: `cargo test`
Expected: All tests pass (no code changes, just docs).

**Step 5: Commit**

```bash
git add CLAUDE.md README.md compose.yml
git commit -m "docs: add Jellyfin notification configuration to docs and compose"
```

---

### Task 6: Run full test suite and verify

**Step 1: Run all unit tests**

Run: `cargo test`
Expected: All tests pass.

**Step 2: Build release binary**

Run: `cargo build --release`
Expected: Builds successfully.

**Step 3: Final commit if needed**

If any fixes were needed, commit them.
