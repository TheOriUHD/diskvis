use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, Local};
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
    fn new(path: PathBuf, is_dir: bool, size: u64, modified: Option<SystemTime>) -> Self {
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
            children: Vec::new(),
        }
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

/// Default platform-specific virtual-filesystem excludes (absolute paths).
/// On Unix these are pseudo-filesystems that should not be traversed by
/// default; on Windows there is no equivalent.
#[cfg(unix)]
pub fn vfs_excludes() -> Vec<&'static str> {
    vec!["/proc", "/sys", "/dev", "/run", "/tmp"]
}

#[cfg(windows)]
pub fn vfs_excludes() -> Vec<&'static str> {
    vec![]
}

#[cfg(not(any(unix, windows)))]
pub fn vfs_excludes() -> Vec<&'static str> {
    vec![]
}

/// On Windows, enumerate available drive letters by probing `A:\`..`Z:\`.
/// Returns the drive root as a `PathBuf` (e.g. `C:\`).
#[cfg(windows)]
pub fn enumerate_drives() -> Vec<PathBuf> {
    let mut out = Vec::new();
    for letter in b'A'..=b'Z' {
        let p = PathBuf::from(format!("{}:\\", letter as char));
        if p.exists() {
            out.push(p);
        }
    }
    out
}

/// Sentinel path representing the synthetic Windows "This PC" root.
#[cfg(windows)]
pub fn this_pc_sentinel() -> PathBuf {
    PathBuf::from(r"\\?\ThisPC")
}

/// Build a synthetic [`Node`] tree representing "This PC" — a virtual root
/// whose children are each available drive (Windows-only).
#[cfg(windows)]
pub fn build_this_pc_node() -> Node {
    let drives = enumerate_drives();
    let children: Vec<Node> = drives
        .into_iter()
        .map(|p| {
            let name = p.to_string_lossy().into_owned();
            Node {
                path: p,
                name,
                size: 0,
                is_dir: true,
                modified: None,
                children: Vec::new(),
            }
        })
        .collect();
    Node {
        path: this_pc_sentinel(),
        name: "[This PC]".to_string(),
        size: 0,
        is_dir: true,
        modified: None,
        children,
    }
}

#[cfg(windows)]
pub fn is_this_pc(p: &Path) -> bool {
    p == this_pc_sentinel().as_path()
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
    if !meta.is_dir() {
        return Ok(ScanResult {
            root: Node::new(
                root.to_path_buf(),
                false,
                meta.len(),
                meta.modified().ok(),
            ),
            warnings: Vec::new(),
        });
    }
    let mut node = Node::new(root.to_path_buf(), true, 0, meta.modified().ok());
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

        if meta.file_type().is_symlink() {
            let size = meta.len();
            node.size += size;
            node.children.push(Node::new(path, false, size, modified));
        } else if meta.is_dir() {
            let mut child = Node::new(path, true, 0, modified);
            build_dir(&mut child, opts, warnings);
            node.size += child.size;
            node.children.push(child);
        } else if meta.is_file() {
            let size = meta.len();
            node.size += size;
            node.children.push(Node::new(path, false, size, modified));
        }
    }
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

