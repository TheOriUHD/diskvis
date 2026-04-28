use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use chrono::{DateTime, Local};
use rayon::prelude::*;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    #[serde(serialize_with = "serialize_path")]
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    #[serde(serialize_with = "serialize_modified", skip_serializing_if = "Option::is_none")]
    pub modified: Option<SystemTime>,
    /// Unix permission bits (lower 12 bits significant: rwx + setuid/setgid/sticky).
    /// Populated when scanning so renderers can flag unusual perms.
    pub mode: Option<u32>,
    pub children: Vec<Node>,
}

fn serialize_path<S: serde::Serializer>(p: &PathBuf, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&p.display().to_string())
}

fn serialize_modified<S: serde::Serializer>(
    t: &Option<SystemTime>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match t {
        Some(st) => {
            let dt: DateTime<Local> = (*st).into();
            s.serialize_str(&dt.format("%Y-%m-%d %H:%M:%S").to_string())
        }
        None => s.serialize_none(),
    }
}

impl Node {
    fn new(
        path: PathBuf,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
        mode: Option<u32>,
    ) -> Self {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        Node {
            path,
            name,
            size,
            is_dir,
            modified,
            mode,
            children: Vec::new(),
        }
    }

    /// Returns true when the node carries permission bits that warrant a
    /// visual highlight (world-writable file/dir, or setuid/setgid).
    pub fn has_unusual_perms(&self) -> bool {
        let Some(m) = self.mode else { return false };
        // world-writable: o+w. SUID / SGID bits.
        m & 0o002 != 0 || m & 0o4000 != 0 || m & 0o2000 != 0
    }

    pub fn file_count(&self) -> usize {
        if !self.is_dir {
            return 1;
        }
        self.children.iter().map(|c| c.file_count()).sum()
    }

    pub fn dir_count(&self) -> usize {
        if !self.is_dir {
            return 0;
        }
        1 + self
            .children
            .iter()
            .filter(|c| c.is_dir)
            .map(|c| c.dir_count())
            .sum::<usize>()
    }

    pub fn all_files(&self) -> Vec<&Node> {
        let mut out = Vec::new();
        self.collect_files(&mut out);
        out
    }

    fn collect_files<'a>(&'a self, out: &mut Vec<&'a Node>) {
        if !self.is_dir {
            out.push(self);
            return;
        }
        for c in &self.children {
            c.collect_files(out);
        }
    }
}

pub struct WalkOptions<'a> {
    pub excludes: &'a [String],
    pub on_progress: Option<&'a (dyn Fn(&Path) + Sync)>,
}

/// Default virtual-filesystem excludes (absolute paths). These are
/// pseudo-filesystems that should not be traversed by default.
pub fn vfs_excludes() -> Vec<&'static str> {
    vec!["/proc", "/sys", "/dev", "/run", "/tmp"]
}

pub struct ScanResult {
    pub root: Node,
    /// Warnings collected during the walk (permission errors, IO failures, etc.).
    /// Buffered instead of printed so the TUI can render them without corrupting
    /// the terminal.
    pub warnings: Vec<String>,
}

pub fn build_tree(root: &Path, opts: &WalkOptions) -> std::io::Result<ScanResult> {
    let meta = std::fs::symlink_metadata(root)?;
    let mode = Some(meta.mode());
    if !meta.is_dir() {
        return Ok(ScanResult {
            root: Node::new(
                root.to_path_buf(),
                false,
                meta.len(),
                meta.modified().ok(),
                mode,
            ),
            warnings: Vec::new(),
        });
    }
    let mut node = Node::new(root.to_path_buf(), true, 0, meta.modified().ok(), mode);
    let mut warnings = Vec::new();
    build_dir(&mut node, opts, &mut warnings);
    Ok(ScanResult {
        root: node,
        warnings,
    })
}

fn build_dir(node: &mut Node, opts: &WalkOptions, warnings: &mut Vec<String>) {
    if let Some(cb) = opts.on_progress {
        cb(&node.path);
    }

    let entries = match std::fs::read_dir(&node.path) {
        Ok(e) => e,
        Err(err) => {
            warnings.push(format!("{}: {}", node.path.display(), err));
            return;
        }
    };

    let mut leaves: Vec<Node> = Vec::new();
    let mut subdirs: Vec<Node> = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                warnings.push(format!("{}: {}", node.path.display(), err));
                continue;
            }
        };
        let path = entry.path();
        let name_os = entry.file_name();
        let name = name_os.to_string_lossy();
        if is_excluded_entry(&name, &path, opts.excludes) {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(err) => {
                warnings.push(format!("{}: {}", path.display(), err));
                continue;
            }
        };

        let modified = meta.modified().ok();
        let mode = Some(meta.mode());

        if meta.file_type().is_symlink() {
            let size = meta.len();
            leaves.push(Node::new(path, false, size, modified, mode));
        } else if meta.is_dir() {
            subdirs.push(Node::new(path, true, 0, modified, mode));
        } else if meta.is_file() {
            let size = meta.len();
            leaves.push(Node::new(path, false, size, modified, mode));
        }
    }

    // Recurse into subdirs in parallel. `WalkOptions` (including the progress
    // callback) is `Sync`, so it can be shared across rayon worker threads.
    let warn_box: Mutex<Vec<String>> = Mutex::new(Vec::new());
    subdirs.par_iter_mut().for_each(|child| {
        let mut local = Vec::new();
        build_dir(child, opts, &mut local);
        if !local.is_empty() {
            warn_box.lock().unwrap().extend(local);
        }
    });
    if let Ok(mut w) = warn_box.into_inner() {
        warnings.append(&mut w);
    }

    let leaf_total: u64 = leaves.iter().map(|n| n.size).sum();
    let dir_total: u64 = subdirs.iter().map(|n| n.size).sum();
    node.size = leaf_total + dir_total;
    node.children = leaves;
    node.children.extend(subdirs);
}

/// Match an entry against the exclude patterns.
/// - Patterns starting with `/` are matched against the entry's full canonical
///   absolute path (so `/proc` skips that whole virtual filesystem).
/// - All other patterns are matched (glob) against the file's basename.
pub fn is_excluded_entry(name: &str, path: &Path, patterns: &[String]) -> bool {
    let abs_str = path.to_string_lossy();
    patterns.iter().any(|p| {
        if p.starts_with('/') {
            // Treat as exact path or path-prefix match: `/proc` matches `/proc`
            // and `/proc/anything`.
            let trimmed = p.trim_end_matches('/');
            abs_str == trimmed || abs_str.starts_with(&format!("{}/", trimmed))
        } else {
            glob_match(p, name)
        }
    })
}

/// Backwards-compatible name-based check (used by callers that only have a basename).
#[allow(dead_code)]
pub fn is_excluded(name: &str, patterns: &[String]) -> bool {
    patterns
        .iter()
        .filter(|p| !p.starts_with('/'))
        .any(|p| glob_match(p, name))
}

pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    glob_inner(&p, 0, &t, 0)
}

fn glob_inner(p: &[char], pi: usize, t: &[char], ti: usize) -> bool {
    if pi == p.len() {
        return ti == t.len();
    }
    match p[pi] {
        '*' => {
            if glob_inner(p, pi + 1, t, ti) {
                return true;
            }
            if ti < t.len() {
                return glob_inner(p, pi, t, ti + 1);
            }
            false
        }
        '?' => {
            if ti < t.len() {
                glob_inner(p, pi + 1, t, ti + 1)
            } else {
                false
            }
        }
        c => {
            if ti < t.len() && t[ti] == c {
                glob_inner(p, pi + 1, t, ti + 1)
            } else {
                false
            }
        }
    }
}

