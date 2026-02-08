use std::collections::HashMap;
use serde::{Deserialize, Serialize};
use crate::rd_client::TorrentInfo;
use regex::Regex;

#[derive(Debug, Clone)]
pub enum VfsNode {
    Directory {
        name: String,
        children: HashMap<String, VfsNode>,
    },
    File {
        name: String,
        size: u64,
        rd_torrent_id: String,
        rd_link: String,
    },
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
}

impl DebridVfs {
    pub fn new() -> Self {
        let mut children = HashMap::new();
        children.insert("Movies".to_string(), VfsNode::Directory {
            name: "Movies".to_string(),
            children: HashMap::new(),
        });
        children.insert("Shows".to_string(), VfsNode::Directory {
            name: "Shows".to_string(),
            children: HashMap::new(),
        });

        Self {
            root: VfsNode::Directory {
                name: "".to_string(),
                children,
            },
        }
    }

    pub fn update(&mut self, torrents: Vec<(TorrentInfo, MediaMetadata)>) {
        let mut movies_nodes = HashMap::new();
        let mut shows_nodes = HashMap::new();

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

            let base_name = metadata.title.clone();
            
            let (used_names, nodes) = match metadata.media_type {
                MediaType::Movie => (&mut used_movie_names, &mut movies_nodes),
                MediaType::Show => (&mut used_show_names, &mut shows_nodes),
            };

            let folder_name = if let Some(id) = &metadata.external_id {
                format!("{} [id={}]", base_name, id)
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

            match metadata.media_type {
                MediaType::Movie => {
                    let mut children = HashMap::new();
                    // For movies, only take the largest torrent to avoid duplicates
                    if let Some(torrent) = torrents.first() {
                        self.add_torrent_files(&mut children, torrent, None);
                    }
                    if !children.is_empty() {
                        nodes.insert(folder_name.clone(), VfsNode::Directory {
                            name: folder_name,
                            children,
                        });
                    }
                }
                MediaType::Show => {
                    let mut show_children = HashMap::new();
                    let season_regex = Regex::new(r"(?i)s(\d+)|season\s*(\d+)|(\d+)x\d+").unwrap();

                    // For shows, we process all torrents (e.g. different seasons)
                    // They are already sorted by size, so larger files will overwrite smaller ones if paths match
                    for torrent in torrents {
                        let mut link_idx = 0;
                        for file in &torrent.files {
                            if file.selected == 1 {
                                if is_video_file(&file.path) {
                                    if let Some(link) = torrent.links.get(link_idx) {
                                        let filename = file.path.split('/').last().unwrap_or(&file.path);
                                        let season = season_regex.captures(filename)
                                            .and_then(|cap| {
                                                cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3))
                                            })
                                            .and_then(|m| m.as_str().parse::<u32>().ok())
                                            .unwrap_or(1);

                                        let season_name = format!("Season {:02}", season);
                                        let season_dir = show_children.entry(season_name.clone()).or_insert_with(|| VfsNode::Directory {
                                            name: season_name.clone(),
                                            children: HashMap::new(),
                                        });

                                        if let VfsNode::Directory { children: season_children, .. } = season_dir {
                                            self.add_path_to_tree(season_children, filename, file.bytes, torrent.id.clone(), link.clone());
                                        }
                                    }
                                }
                                link_idx += 1;
                            }
                        }
                    }
                    if !show_children.is_empty() {
                        nodes.insert(folder_name.clone(), VfsNode::Directory {
                            name: folder_name,
                            children: show_children,
                        });
                    }
                }
            }
        }

        if let VfsNode::Directory { ref mut children, .. } = self.root {
            children.insert("Movies".to_string(), VfsNode::Directory {
                name: "Movies".to_string(),
                children: movies_nodes,
            });
            children.insert("Shows".to_string(), VfsNode::Directory {
                name: "Shows".to_string(),
                children: shows_nodes,
            });
        }
    }

    fn add_torrent_files(&self, destination: &mut HashMap<String, VfsNode>, torrent: &TorrentInfo, path_prefix: Option<&str>) {
        let mut link_idx = 0;
        for file in &torrent.files {
            if file.selected == 1 {
                if is_video_file(&file.path) {
                    if let Some(link) = torrent.links.get(link_idx) {
                        let filename = file.path.split('/').last().unwrap_or(&file.path);
                        let path = if let Some(prefix) = path_prefix {
                            format!("{}/{}", prefix, filename.trim_start_matches('/'))
                        } else {
                            filename.to_string()
                        };
                        self.add_path_to_tree(destination, &path, file.bytes, torrent.id.clone(), link.clone());
                    }
                }
                link_idx += 1;
            }
        }
    }

    fn add_path_to_tree(&self, root: &mut HashMap<String, VfsNode>, path: &str, size: u64, torrent_id: String, link: String) {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let mut current_children = root;

        for i in 0..parts.len() {
            let part = parts[i].to_string();
            if i == parts.len() - 1 {
                let mut final_part = part.clone();
                let mut counter = 1;
                while current_children.contains_key(&final_part) {
                    if let Some(pos) = part.rfind('.') {
                        let (base, ext) = part.split_at(pos);
                        final_part = format!("{} ({}){}", base, counter, ext);
                    } else {
                        final_part = format!("{} ({})", part, counter);
                    }
                    counter += 1;
                }
                current_children.insert(final_part.clone(), VfsNode::File {
                    name: final_part,
                    size,
                    rd_torrent_id: torrent_id.clone(),
                    rd_link: link.clone(),
                });
            } else {
                let entry = current_children.entry(part.clone()).or_insert_with(|| VfsNode::Directory {
                    name: part.clone(),
                    children: HashMap::new(),
                });
                if let VfsNode::Directory { children, .. } = entry {
                    current_children = children;
                } else {
                    // This should not happen if paths are consistent
                    return;
                }
            }
        }
    }
}

pub fn is_video_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    if lower.contains("sample") || lower.contains("trailer") || lower.contains("extra") || 
       lower.contains("bonus") || lower.contains("featurette") {
        return false;
    }
    lower.ends_with(".mkv") || lower.ends_with(".mp4") || lower.ends_with(".avi") || 
    lower.ends_with(".m4v") || lower.ends_with(".mov") || lower.ends_with(".wmv") || 
    lower.ends_with(".flv") || lower.ends_with(".ts") || lower.ends_with(".m2ts")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rd_client::{TorrentInfo, TorrentFile};

    #[test]
    fn test_vfs_update() {
        let mut vfs = DebridVfs::new();
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

        vfs.update(torrents);

        if let VfsNode::Directory { children, .. } = &vfs.root {
            let movies = children.get("Movies").expect("Movies directory missing");
            let shows = children.get("Shows").expect("Shows directory missing");

            if let VfsNode::Directory { children: movie_children, .. } = movies {
                assert!(movie_children.contains_key("Movie"), "Movie folder not found");
                let movie_dir = movie_children.get("Movie").unwrap();
                if let VfsNode::Directory { children: files, .. } = movie_dir {
                    assert!(files.contains_key("Movie.2023.1080p.mkv"), "Movie file not found");
                }
            } else {
                panic!("Movies should be a directory");
            }

            if let VfsNode::Directory { children: show_children, .. } = shows {
                assert!(show_children.contains_key("Show"), "Show folder not found");
                let show_dir = show_children.get("Show").unwrap();
                if let VfsNode::Directory { children: seasons, .. } = show_dir {
                    assert!(seasons.contains_key("Season 01"), "Season 01 not found");
                }
            } else {
                panic!("Shows should be a directory");
            }
        }
    }

    #[test]
    fn test_vfs_conflicts() {
        let mut vfs = DebridVfs::new();
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

        vfs.update(torrents);

        if let VfsNode::Directory { children, .. } = &vfs.root {
            let movies = children.get("Movies").unwrap();
            if let VfsNode::Directory { children: movie_children, .. } = movies {
                assert!(movie_children.contains_key("Same Title [id=tmdb:1]"));
                assert!(movie_children.contains_key("Same Title [id=tmdb:2]"));
            }
        }
    }

    #[test]
    fn test_vfs_duplicates() {
        let mut vfs = DebridVfs::new();
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

        vfs.update(torrents);

        if let VfsNode::Directory { children, .. } = &vfs.root {
            let movies = children.get("Movies").unwrap();
            if let VfsNode::Directory { children: movie_children, .. } = movies {
                let folder = movie_children.get("Duplicate Movie [id=tmdb:123]").expect("Folder missing");
                if let VfsNode::Directory { children: files, .. } = folder {
                    let file = files.get("Movie.mkv").expect("File missing");
                    if let VfsNode::File { size, rd_torrent_id, .. } = file {
                        assert_eq!(*size, 5000, "Should have picked the larger file");
                        assert_eq!(rd_torrent_id, "large", "Should have picked the large torrent");
                    } else {
                        panic!("Should be a file");
                    }
                }
            }
        }
    }
}
