use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;
use serde::{Deserialize, Serialize};
use crate::rd_client::TorrentInfo;
use regex::Regex;

/// Fixed size for STRM file metadata. All STRM files report this size in WebDAV
/// metadata (PROPFIND and GET Content-Length). The actual content (URL + padding)
/// is padded to this size so metadata and content are always consistent.
/// Real-Debrid download URLs are typically 100-300 bytes.
pub const STRM_FIXED_SIZE: usize = 1024;

pub const VIDEO_EXTENSIONS: &[&str] = &[
    ".mkv", ".mp4", ".avi", ".m4v", ".mov", ".wmv", ".flv", ".ts", ".m2ts",
];

const ARCHIVE_EXTENSIONS: &[&str] = &[
    ".rar", ".zip", ".7z", ".tar", ".gz", ".bz2",
];

/// RAR split-volume patterns: .r00, .r01, ..., .r99, .s00, etc.
fn is_archive_part(path: &str) -> bool {
    let lower = path.to_lowercase();
    // .r00-.r99, .s00-.s99 (split RAR volumes)
    if lower.len() >= 4 {
        let ext = &lower[lower.len().saturating_sub(4)..];
        if ext.starts_with(".r") || ext.starts_with(".s") {
            return ext[2..].chars().all(|c| c.is_ascii_digit());
        }
    }
    false
}

fn is_archive_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    ARCHIVE_EXTENSIONS.iter().any(|ext| lower.ends_with(ext)) || is_archive_part(&lower)
}

static SEASON_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)s(\d+)|season\s*(\d+)|(\d+)x\d+").unwrap()
});

#[derive(Debug, Clone, PartialEq)]
pub enum VfsNode {
    Directory {
        children: BTreeMap<String, VfsNode>,
    },
    /// STRM file that contains a direct Real-Debrid download URL
    /// Jellyfin will read this tiny file and stream directly from RD
    /// strm_content contains the actual file content (the URL with newline)
    StrmFile {
        strm_content: Vec<u8>, // The actual STRM file content (URL + newline)
        rd_link: String, // The /torrents link that may need unrestricting
        rd_torrent_id: String,
    },
    VirtualFile {
        content: Vec<u8>,
    },
}

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum MediaType {
    Movie,
    Show,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub struct MediaMetadata {
    pub title: String,
    pub year: Option<String>,
    pub media_type: MediaType,
    pub external_id: Option<String>,
}

pub struct DebridVfs {
    pub root: VfsNode,
    pub created_at: std::time::SystemTime,
}

impl Default for DebridVfs {
    fn default() -> Self {
        Self::new()
    }
}

impl DebridVfs {
    pub fn new() -> Self {
        let mut children = BTreeMap::new();
        children.insert("Movies".to_string(), VfsNode::Directory {
            children: BTreeMap::new(),
        });
        children.insert("Shows".to_string(), VfsNode::Directory {
            children: BTreeMap::new(),
        });

        Self {
            root: VfsNode::Directory {
                children,
            },
            created_at: std::time::SystemTime::now(),
        }
    }

    pub fn build(torrents: Vec<(TorrentInfo, MediaMetadata)>) -> Self {
        let mut movies_nodes = BTreeMap::new();
        let mut shows_nodes = BTreeMap::new();

        // Group torrents by media (using title, year, type, and external ID)
        let mut media_groups: HashMap<MediaMetadata, Vec<TorrentInfo>> = HashMap::new();
        for (torrent, metadata) in torrents {
            media_groups.entry(metadata).or_default().push(torrent);
        }

        // Sort groups to ensure deterministic naming for conflicts
        let mut sorted_metadata: Vec<_> = media_groups.keys().cloned().collect();
        sorted_metadata.sort_by(|a, b| {
            a.title.cmp(&b.title)
                .then_with(|| a.year.cmp(&b.year))
                .then_with(|| a.external_id.cmp(&b.external_id))
        });

        let mut used_movie_names: HashMap<String, u32> = HashMap::new();
        let mut used_show_names: HashMap<String, u32> = HashMap::new();

        for metadata in sorted_metadata {
            let mut torrents = media_groups.get(&metadata).unwrap().clone();
            // Sort torrents by size descending to pick the best/largest one
            torrents.sort_by_key(|t| std::cmp::Reverse(t.bytes));

            let base_name = sanitize_filename(&metadata.title);
            
            let (used_names, nodes) = match metadata.media_type {
                MediaType::Movie => (&mut used_movie_names, &mut movies_nodes),
                MediaType::Show => (&mut used_show_names, &mut shows_nodes),
            };

            let folder_name = if let Some(id) = &metadata.external_id {
                if let Some((source, raw_id)) = id.split_once(':') {
                    format!("{} [{}id-{}]", base_name, source, raw_id)
                } else {
                    format!("{} [id={}]", base_name, id)
                }
            } else {
                let count = used_names.entry(base_name.clone()).or_insert(0);
                let name = if *count == 0 {
                    base_name.clone()
                } else {
                    format!("{} ({})", base_name, count)
                };
                *count += 1;
                name
            };

            // Warn about archive-only torrents that can't be streamed
            for torrent in &torrents {
                let has_video = torrent.files.iter().any(|f| f.selected == 1 && is_video_file(&f.path));
                if !has_video {
                    let has_archive = torrent.files.iter().any(|f| f.selected == 1 && is_archive_file(&f.path));
                    if has_archive {
                        tracing::warn!(
                            "Torrent '{}' contains only archive files (RAR/ZIP) and cannot be streamed — \
                             replace with a non-archive version on Real-Debrid",
                            torrent.filename
                        );
                    }
                }
            }

            match metadata.media_type {
                MediaType::Movie => {
                    let mut children = BTreeMap::new();
                    // For movies, only take the largest torrent to avoid duplicates
                    if let Some(torrent) = torrents.first() {
                        Self::add_torrent_files(&mut children, torrent, None);
                    }
                    if !children.is_empty() {
                        let nfo_content = Self::generate_nfo(&metadata);
                        children.insert("movie.nfo".to_string(), VfsNode::VirtualFile {
                            content: nfo_content,
                        });
                        nodes.insert(folder_name, VfsNode::Directory {
                            children,
                        });
                    }
                }
                MediaType::Show => {
                    let mut show_children = BTreeMap::new();

                    // For shows, we process all torrents (e.g. different seasons)
                    // They are already sorted by size, so larger files will overwrite smaller ones if paths match
                    for torrent in torrents {
                        let selected_count = torrent.files.iter().filter(|f| f.selected == 1).count();
                        if selected_count != torrent.links.len() {
                            tracing::warn!(
                                "Torrent '{}': selected file count ({}) != link count ({})",
                                torrent.filename, selected_count, torrent.links.len()
                            );
                        }
                        let mut link_idx = 0;
                        for file in &torrent.files {
                            if file.selected == 1 {
                                if is_video_file(&file.path) {
                                    if let Some(link) = torrent.links.get(link_idx) {
                                        let filename = file.path.split('/').next_back().unwrap_or(&file.path);
                                        let season = SEASON_RE.captures(filename)
                                            .and_then(|cap| {
                                                cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3))
                                            })
                                            .and_then(|m| m.as_str().parse::<u32>().ok())
                                            .unwrap_or(1);

                                        let season_name = format!("Season {:02}", season);
                                        let season_dir = show_children.entry(season_name.clone()).or_insert_with(|| VfsNode::Directory {
                                            children: BTreeMap::new(),
                                        });

                                        if let VfsNode::Directory { children: season_children } = season_dir {
                                            Self::add_path_to_tree(season_children, filename, file.bytes, torrent.id.clone(), link.clone());
                                        }
                                    }
                                }
                                link_idx += 1;
                            }
                        }
                    }
                    if !show_children.is_empty() {
                        let nfo_content = Self::generate_nfo(&metadata);
                        show_children.insert("tvshow.nfo".to_string(), VfsNode::VirtualFile {
                            content: nfo_content,
                        });
                        nodes.insert(folder_name, VfsNode::Directory {
                            children: show_children,
                        });
                    }
                }
            }
        }

        let mut root_children = BTreeMap::new();
        root_children.insert("Movies".to_string(), VfsNode::Directory {
            children: movies_nodes,
        });
        root_children.insert("Shows".to_string(), VfsNode::Directory {
            children: shows_nodes,
        });
        Self {
            root: VfsNode::Directory { children: root_children },
            created_at: std::time::SystemTime::now(),
        }
    }

    fn generate_nfo(metadata: &MediaMetadata) -> Vec<u8> {
        let tag = match metadata.media_type {
            MediaType::Movie => "movie",
            MediaType::Show => "tvshow",
        };

        let mut nfo = format!("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\" ?>\n<{}>\n", tag);

        // Title
        nfo.push_str(&format!("  <title>{}</title>\n", xml_escape(&metadata.title)));

        // Original title (same as title for now)
        nfo.push_str(&format!("  <originaltitle>{}</originaltitle>\n", xml_escape(&metadata.title)));

        // Year
        if let Some(year) = &metadata.year {
            nfo.push_str(&format!("  <year>{}</year>\n", xml_escape(year)));
            // Add full date for better compatibility
            nfo.push_str(&format!("  <premiered>{}-01-01</premiered>\n", xml_escape(year)));
        }

        // External IDs
        if let Some(external_id) = &metadata.external_id {
            if let Some((source, id)) = external_id.split_once(':') {
                nfo.push_str(&format!("  <uniqueid type=\"{}\" default=\"true\">{}</uniqueid>\n", xml_escape(source), xml_escape(id)));
                if source == "tmdb" {
                    let path = match metadata.media_type {
                        MediaType::Movie => "movie",
                        MediaType::Show => "tv",
                    };
                    nfo.push_str(&format!("  <tmdbid>{}</tmdbid>\n", xml_escape(id)));
                    nfo.push_str(&format!("  <url>https://www.themoviedb.org/{}/{}</url>\n", xml_escape(path), xml_escape(id)));
                }
            }
        }

        // Don't include plot/outline - let Jellyfin fetch from TMDB
        // Including placeholder text causes duplication in Jellyfin UI

        // Lockdata set to false allows Jellyfin to fetch full metadata from TMDB
        nfo.push_str("  <lockdata>false</lockdata>\n");

        // Source indicator
        nfo.push_str("  <source>debridmoviemapper</source>\n");

        nfo.push_str(&format!("</{}>\n", tag));
        nfo.into_bytes()
    }

    fn add_torrent_files(destination: &mut BTreeMap<String, VfsNode>, torrent: &TorrentInfo, path_prefix: Option<&str>) {
        let selected_count = torrent.files.iter().filter(|f| f.selected == 1).count();
        if selected_count != torrent.links.len() {
            tracing::warn!(
                "Torrent '{}': selected file count ({}) != link count ({})",
                torrent.filename, selected_count, torrent.links.len()
            );
        }
        let mut link_idx = 0;
        for file in &torrent.files {
            if file.selected == 1 {
                if is_video_file(&file.path) {
                    if let Some(link) = torrent.links.get(link_idx) {
                        let filename = file.path.split('/').next_back().unwrap_or(&file.path);
                        let path = if let Some(prefix) = path_prefix {
                            format!("{}/{}", prefix, filename.trim_start_matches('/'))
                        } else {
                            filename.to_string()
                        };
                        Self::add_path_to_tree(destination, &path, file.bytes, torrent.id.clone(), link.clone());
                    }
                }
                link_idx += 1;
            }
        }
    }

    fn add_path_to_tree(root: &mut BTreeMap<String, VfsNode>, path: &str, _size: u64, torrent_id: String, link: String) {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let mut current_children = root;

        for i in 0..parts.len() {
            let part = parts[i].to_string();
            if i == parts.len() - 1 {
                // This is the video file - convert to .strm
                let strm_name = if let Some(pos) = part.rfind('.') {
                    format!("{}.strm", &part[..pos])
                } else {
                    format!("{}.strm", part)
                };

                let mut final_name = strm_name.clone();
                let mut counter = 1;
                while current_children.contains_key(&final_name) {
                    if let Some(pos) = strm_name.rfind(".strm") {
                        let base = &strm_name[..pos];
                        final_name = format!("{} ({}).strm", base, counter);
                    } else {
                        final_name = format!("{} ({})", strm_name, counter);
                    }
                    counter += 1;
                }

                // Pad to STRM_FIXED_SIZE so PROPFIND metadata matches GET Content-Length.
                // The actual download URL is fetched on-demand by dav_fs::StrmFile::read_bytes.
                let mut strm_content = format!("{}\n", link).into_bytes();
                strm_content.resize(STRM_FIXED_SIZE, b' ');

                current_children.insert(final_name, VfsNode::StrmFile {
                    strm_content,
                    rd_link: link.clone(),
                    rd_torrent_id: torrent_id.clone(),
                });
            } else {
                let entry = current_children.entry(part).or_insert_with(|| VfsNode::Directory {
                    children: BTreeMap::new(),
                });
                if let VfsNode::Directory { children } = entry {
                    current_children = children;
                } else {
                    // This should not happen if paths are consistent
                    return;
                }
            }
        }
    }
}

/// Replace characters that are invalid in filenames or interpreted as path separators.
fn sanitize_filename(name: &str) -> String {
    let replaced: String = name.chars()
        .flat_map(|c| match c {
            '/' | '\\' => vec!['-'],
            ':' => vec![' ', '-'],
            _ => vec![c],
        })
        .collect();
    replaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub fn is_video_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    let filename = lower.rsplit('/').next().unwrap_or(&lower);
    if filename.contains("sample") || filename.contains("trailer") || filename.contains("extra") ||
       filename.contains("bonus") || filename.contains("featurette") {
        return false;
    }
    VIDEO_EXTENSIONS.iter().any(|ext| lower.ends_with(ext))
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// Diff two VFS trees and return the list of changed paths at the deepest
/// meaningful level (e.g. season folder for a new episode, movie folder for
/// a new movie).
///
/// Changes are reported at the directory level, never at the individual file
/// level. When leaf nodes (StrmFile, VirtualFile) inside a directory change,
/// the directory itself is reported as Modified.
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

    // Track whether any non-directory (leaf) children changed. If so, we
    // report this directory as the change point rather than individual files.
    let mut has_leaf_changes = false;

    for (name, old_child) in old_children {
        let child_path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{}/{}", prefix, name)
        };

        match new_children.get(name) {
            None => {
                match old_child {
                    VfsNode::Directory { .. } => {
                        // A child directory was removed — report it as deleted
                        changes.push(VfsChange {
                            path: child_path,
                            update_type: UpdateType::Deleted,
                        });
                    }
                    _ => {
                        // A leaf was removed — flag for parent-level reporting
                        has_leaf_changes = true;
                    }
                }
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
                        // Leaf node changed — flag for parent-level reporting
                        has_leaf_changes = true;
                    }
                    _ => {
                        // Identical leaf nodes — no change
                    }
                }
            }
        }
    }

    // Check for new children
    for (name, new_child) in new_children {
        if !old_children.contains_key(name) {
            let child_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", prefix, name)
            };

            match new_child {
                VfsNode::Directory { children } => {
                    // A new child directory — report the deepest new directory
                    let deepest = find_deepest_new_dir(&child_path, children);
                    changes.push(VfsChange {
                        path: deepest,
                        update_type: UpdateType::Created,
                    });
                }
                _ => {
                    // A new leaf was added — flag for parent-level reporting
                    has_leaf_changes = true;
                }
            }
        }
    }

    // If any leaf-level changes occurred, report this directory as modified.
    // But only if we have a prefix (i.e., we're not at the root level).
    if has_leaf_changes && !prefix.is_empty() {
        changes.push(VfsChange {
            path: prefix.to_string(),
            update_type: UpdateType::Modified,
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::{TorrentInfo, TorrentFile};

    #[test]
    fn broken_link_placeholder_not_present() {
        // Verify the broken-link error placeholder was removed from this file.
        // On unrestrict failure we skip the file rather than inserting a fake STRM.
        // The search string is split so this test does not self-match.
        let placeholder = ["# Error: Failed", " to unrestrict link"].concat();
        let source = include_str!("vfs.rs");
        assert!(
            !source.contains(&placeholder),
            "vfs.rs must not contain the broken-link placeholder — skip the file on error instead"
        );
    }

    #[test]
    fn test_vfs_update() {
        let torrents = vec![
            (
                TorrentInfo {
                    id: "1".to_string(),
                    filename: "Movie.2023.1080p.mkv".to_string(),
                    original_filename: "Movie.2023.1080p.mkv".to_string(),
                    hash: "hash1".to_string(),
                    bytes: 1000,
                    original_bytes: 1000,
                    host: "host1".to_string(),
                    split: 1,
                    progress: 100.0,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.2023.1080p.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://link1".to_string()],
                    ended: Some("2023-01-01".to_string()),
                },
                MediaMetadata {
                    title: "Movie".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Movie,
                    external_id: None,
                }
            ),
            (
                TorrentInfo {
                    id: "2".to_string(),
                    filename: "Show.S01E01.mkv".to_string(),
                    original_filename: "Show.S01E01.mkv".to_string(),
                    hash: "hash2".to_string(),
                    bytes: 500,
                    original_bytes: 500,
                    host: "host2".to_string(),
                    split: 1,
                    progress: 100.0,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Show.S01E01.mkv".to_string(),
                        bytes: 500,
                        selected: 1,
                    }],
                    links: vec!["http://link2".to_string()],
                    ended: Some("2023-01-01".to_string()),
                },
                MediaMetadata {
                    title: "Show".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Show,
                    external_id: None,
                }
            ),
        ];

        let vfs = DebridVfs::build(torrents);

        if let VfsNode::Directory { children } = &vfs.root {
            let movies = children.get("Movies").expect("Movies directory missing");
            let shows = children.get("Shows").expect("Shows directory missing");

            if let VfsNode::Directory { children: movie_children } = movies {
                assert!(movie_children.contains_key("Movie"), "Movie folder not found");
                let movie_dir = movie_children.get("Movie").unwrap();
                if let VfsNode::Directory { children: files } = movie_dir {
                    assert!(files.contains_key("Movie.2023.1080p.strm"), "Movie STRM file not found");
                    assert!(files.contains_key("movie.nfo"), "movie.nfo not found");
                }
            } else {
                panic!("Movies should be a directory");
            }

            if let VfsNode::Directory { children: show_children } = shows {
                assert!(show_children.contains_key("Show"), "Show folder not found");
                let show_dir = show_children.get("Show").unwrap();
                if let VfsNode::Directory { children: seasons } = show_dir {
                    assert!(seasons.contains_key("Season 01"), "Season 01 not found");
                    assert!(seasons.contains_key("tvshow.nfo"), "tvshow.nfo not found");
                }
            } else {
                panic!("Shows should be a directory");
            }
        }
    }

    #[test]
    fn test_nfo_generation() {
        let metadata = MediaMetadata {
            title: "Test Movie".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:12345".to_string()),
        };
        let content = String::from_utf8(DebridVfs::generate_nfo(&metadata)).unwrap();
        assert!(content.contains("<movie>"));
        assert!(content.contains("<title>Test Movie</title>"));
        assert!(content.contains("<originaltitle>Test Movie</originaltitle>"));
        assert!(content.contains("<year>2024</year>"));
        assert!(content.contains("<premiered>2024-01-01</premiered>"));
        assert!(content.contains("<uniqueid type=\"tmdb\" default=\"true\">12345</uniqueid>"));
        assert!(content.contains("<tmdbid>12345</tmdbid>"));
        assert!(content.contains("<url>https://www.themoviedb.org/movie/12345</url>"));
        assert!(content.contains("<lockdata>false</lockdata>"));
        assert!(content.contains("<source>debridmoviemapper</source>"));
    }

    #[test]
    fn test_vfs_conflicts() {
        let torrents = vec![
            (
                TorrentInfo {
                    id: "1".to_string(),
                    filename: "Movie.2023.mkv".to_string(),
                    original_filename: "Movie.2023.mkv".to_string(),
                    hash: "h1".to_string(),
                    bytes: 1000,
                    original_bytes: 1000,
                    host: "h1".to_string(),
                    split: 1,
                    progress: 100.0,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://l1".to_string()],
                    ended: Some("2023".to_string()),
                },
                MediaMetadata {
                    title: "Same Title".to_string(),
                    year: Some("2023".to_string()),
                    media_type: MediaType::Movie,
                    external_id: Some("tmdb:1".to_string()),
                }
            ),
            (
                TorrentInfo {
                    id: "2".to_string(),
                    filename: "Movie.2024.mkv".to_string(),
                    original_filename: "Movie.2024.mkv".to_string(),
                    hash: "h2".to_string(),
                    bytes: 1000,
                    original_bytes: 1000,
                    host: "h2".to_string(),
                    split: 1,
                    progress: 100.0,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile {
                        id: 1,
                        path: "/Movie.mkv".to_string(),
                        bytes: 1000,
                        selected: 1,
                    }],
                    links: vec!["http://l2".to_string()],
                    ended: Some("2024".to_string()),
                },
                MediaMetadata {
                    title: "Same Title".to_string(),
                    year: Some("2024".to_string()),
                    media_type: MediaType::Movie,
                    external_id: Some("tmdb:2".to_string()),
                }
            ),
        ];

        let vfs = DebridVfs::build(torrents);

        if let VfsNode::Directory { children } = &vfs.root {
            let movies = children.get("Movies").unwrap();
            if let VfsNode::Directory { children: movie_children } = movies {
                assert!(movie_children.contains_key("Same Title [tmdbid-1]"));
                assert!(movie_children.contains_key("Same Title [tmdbid-2]"));
            }
        }
    }

    #[test]
    fn strm_error_placeholder_is_not_used() {
        // Verify the broken-link placeholder string has been removed from this file.
        // If this test fails, it means the placeholder was re-introduced.
        // The needle is split so that this test's own source text does not match it.
        let source = include_str!("vfs.rs");
        let needle = concat!("# Error: Failed to unrestrict", " link");
        assert!(
            !source.contains(needle),
            "vfs.rs must not contain the broken-link placeholder string — skip the file instead"
        );
    }

    #[test]
    fn test_nfo_escapes_special_xml_characters() {
        let metadata = MediaMetadata {
            title: "Test & <Movie>".to_string(),
            year: Some("2024".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:123".to_string()),
        };
        let content = String::from_utf8(DebridVfs::generate_nfo(&metadata)).unwrap();
        assert!(content.contains("<title>Test &amp; &lt;Movie&gt;</title>"), "Title should be XML-escaped");
        assert!(content.contains("<originaltitle>Test &amp; &lt;Movie&gt;</originaltitle>"), "Original title should be XML-escaped");
        // Year and IDs should also be escaped (even if unlikely to contain special chars)
        assert!(!content.contains("&<"), "No unescaped special characters should appear");
    }

    #[test]
    fn test_vfs_duplicates() {
        let metadata = MediaMetadata {
            title: "Duplicate Movie".to_string(),
            year: Some("2023".to_string()),
            media_type: MediaType::Movie,
            external_id: Some("tmdb:123".to_string()),
        };

        let torrents = vec![
            (
                TorrentInfo {
                    id: "small".to_string(),
                    filename: "Movie.small.mkv".to_string(),
                    original_filename: "Movie.small.mkv".to_string(),
                    hash: "h1".to_string(),
                    bytes: 1000,
                    original_bytes: 1000,
                    host: "h1".to_string(),
                    split: 1,
                    progress: 100.0,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile { id: 1, path: "/Movie.mkv".to_string(), bytes: 1000, selected: 1 }],
                    links: vec!["http://small".to_string()],
                    ended: Some("2023".to_string()),
                },
                metadata.clone()
            ),
            (
                TorrentInfo {
                    id: "large".to_string(),
                    filename: "Movie.large.mkv".to_string(),
                    original_filename: "Movie.large.mkv".to_string(),
                    hash: "h2".to_string(),
                    bytes: 5000,
                    original_bytes: 5000,
                    host: "h2".to_string(),
                    split: 1,
                    progress: 100.0,
                    status: "downloaded".to_string(),
                    added: "2023-01-01".to_string(),
                    files: vec![TorrentFile { id: 1, path: "/Movie.mkv".to_string(), bytes: 5000, selected: 1 }],
                    links: vec!["http://large".to_string()],
                    ended: Some("2023".to_string()),
                },
                metadata
            ),
        ];

        let vfs = DebridVfs::build(torrents);

        if let VfsNode::Directory { children } = &vfs.root {
            let movies = children.get("Movies").unwrap();
            if let VfsNode::Directory { children: movie_children } = movies {
                let folder = movie_children.get("Duplicate Movie [tmdbid-123]").expect("Folder missing");
                if let VfsNode::Directory { children: files } = folder {
                    let file = files.get("Movie.strm").expect("STRM file missing");
                    if let VfsNode::StrmFile { rd_torrent_id, .. } = file {
                        assert_eq!(rd_torrent_id, "large", "Should have picked the large torrent");
                    } else {
                        panic!("Should be a STRM file");
                    }
                }
            }
        }
    }

    #[test]
    fn is_archive_file_detects_archives() {
        assert!(is_archive_file("movie.rar"));
        assert!(is_archive_file("movie.r00"));
        assert!(is_archive_file("movie.r99"));
        assert!(is_archive_file("movie.zip"));
        assert!(is_archive_file("movie.7z"));
        assert!(is_archive_file("/path/to/movie.RAR"));
        assert!(is_archive_file("movie.s00"));
        assert!(!is_archive_file("movie.mkv"));
        assert!(!is_archive_file("movie.nfo"));
        assert!(!is_archive_file("movie.srt"));
    }

    #[test]
    fn sanitize_filename_replaces_slashes() {
        assert_eq!(sanitize_filename("Alex/October"), "Alex-October");
        assert_eq!(sanitize_filename("Back\\Slash"), "Back-Slash");
        assert_eq!(sanitize_filename("Title: Subtitle"), "Title - Subtitle");
        assert_eq!(sanitize_filename("Normal Title"), "Normal Title");
    }

    #[test]
    fn build_sanitizes_title_with_slash() {
        let torrents = vec![(
            TorrentInfo {
                id: "1".to_string(),
                filename: "Alex.October.mkv".to_string(),
                original_filename: "Alex.October.mkv".to_string(),
                hash: "h".to_string(),
                bytes: 1000,
                original_bytes: 1000,
                host: "h".to_string(),
                split: 1,
                progress: 100.0,
                status: "downloaded".to_string(),
                added: "2023-01-01".to_string(),
                files: vec![TorrentFile { id: 1, path: "/Alex.October.mkv".to_string(), bytes: 1000, selected: 1 }],
                links: vec!["http://link1".to_string()],
                ended: Some("2023-01-01".to_string()),
            },
            MediaMetadata {
                title: "Alex/October".to_string(),
                year: None,
                media_type: MediaType::Movie,
                external_id: Some("tmdb:856721".to_string()),
            },
        )];

        let vfs = DebridVfs::build(torrents);
        if let VfsNode::Directory { children } = &vfs.root {
            let movies = children.get("Movies").unwrap();
            if let VfsNode::Directory { children: movie_children } = movies {
                assert!(
                    movie_children.contains_key("Alex-October [tmdbid-856721]"),
                    "Expected sanitized folder name, got: {:?}",
                    movie_children.keys().collect::<Vec<_>>()
                );
            }
        }
    }

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
}
