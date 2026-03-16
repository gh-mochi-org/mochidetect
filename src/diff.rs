use anyhow::Result;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq)]
pub enum ChangeKind {
    Added,
    Removed,
    Modified,
    Unchanged,
}

impl ChangeKind {
    pub fn symbol(&self) -> &str {
        match self {
            ChangeKind::Added => "+",
            ChangeKind::Removed => "-",
            ChangeKind::Modified => "~",
            ChangeKind::Unchanged => "=",
        }
    }

    pub fn label(&self) -> &str {
        match self {
            ChangeKind::Added => "ADDED",
            ChangeKind::Removed => "REMOVED",
            ChangeKind::Modified => "MODIFIED",
            ChangeKind::Unchanged => "UNCHANGED",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub rel_path: PathBuf,
    pub kind: ChangeKind,
    pub old_path: Option<PathBuf>,
    pub new_path: Option<PathBuf>,
    pub is_binary: bool,
    pub old_size: Option<u64>,
    pub new_size: Option<u64>,
}

impl FileDiff {
    pub fn extension(&self) -> String {
        self.rel_path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
    }

    pub fn is_text(&self) -> bool {
        !self.is_binary
    }
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag: LineTag,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LineTag {
    Equal,
    Insert,
    Delete,
    Header,
}

#[derive(Debug)]
pub struct DiffResult {
    pub old_path: String,
    pub new_path: String,
    pub files: Vec<FileDiff>,
    pub stats: DiffStats,
}

#[derive(Debug, Default, Clone)]
pub struct DiffStats {
    pub added: usize,
    pub removed: usize,
    pub modified: usize,
    pub unchanged: usize,
}

impl DiffStats {
    pub fn total_changes(&self) -> usize {
        self.added + self.removed + self.modified
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(hex::encode(hasher.finalize()))
}

fn is_binary(bytes: &[u8]) -> bool {
    let check_len = bytes.len().min(8000);
    bytes[..check_len].contains(&0)
}

fn collect_files(root: &Path) -> Result<HashMap<PathBuf, (PathBuf, u64)>> {
    let mut map = HashMap::new();
    if root.is_file() {
        let meta = fs::metadata(root)?;
        let filename = root.file_name().unwrap_or_default();
        map.insert(PathBuf::from(filename), (root.to_path_buf(), meta.len()));
        return Ok(map);
    }
    for entry in WalkDir::new(root)
        .follow_links(true)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let full = entry.path().to_path_buf();
        let rel = full.strip_prefix(root)?.to_path_buf();
        // skip hidden and common non-source dirs
        if should_skip(&rel) {
            continue;
        }
        let meta = fs::metadata(&full)?;
        map.insert(rel, (full, meta.len()));
    }
    Ok(map)
}

fn should_skip(rel: &Path) -> bool {
    for component in rel.components() {
        let s = component.as_os_str().to_string_lossy();
        if s.starts_with('.')
            || s == "node_modules"
            || s == "target"
            || s == "__pycache__"
            || s == ".git"
            || s == "dist"
            || s == "build"
            || s == ".next"
            || s == "vendor"
        {
            return true;
        }
    }
    false
}

pub fn compute_diff(old_root: &Path, new_root: &Path) -> Result<DiffResult> {
    let old_files = collect_files(old_root)?;
    let new_files = collect_files(new_root)?;

    let old_keys: HashSet<&PathBuf> = old_files.keys().collect();
    let new_keys: HashSet<&PathBuf> = new_files.keys().collect();

    let mut files: Vec<FileDiff> = Vec::new();
    let mut stats = DiffStats::default();

    // Added files (in new but not old)
    for key in new_keys.difference(&old_keys) {
        let (path, size) = &new_files[*key];
        let bytes = fs::read(path).unwrap_or_default();
        let binary = is_binary(&bytes);
        files.push(FileDiff {
            rel_path: (*key).clone(),
            kind: ChangeKind::Added,
            old_path: None,
            new_path: Some(path.clone()),
            is_binary: binary,
            old_size: None,
            new_size: Some(*size),
        });
        stats.added += 1;
    }

    // Removed files (in old but not new)
    for key in old_keys.difference(&new_keys) {
        let (path, size) = &old_files[*key];
        let bytes = fs::read(path).unwrap_or_default();
        let binary = is_binary(&bytes);
        files.push(FileDiff {
            rel_path: (*key).clone(),
            kind: ChangeKind::Removed,
            old_path: Some(path.clone()),
            new_path: None,
            is_binary: binary,
            old_size: Some(*size),
            new_size: None,
        });
        stats.removed += 1;
    }

    // Present in both — check if modified
    for key in old_keys.intersection(&new_keys) {
        let (old_path, old_size) = &old_files[*key];
        let (new_path, new_size) = &new_files[*key];

        let old_hash = hash_file(old_path).unwrap_or_default();
        let new_hash = hash_file(new_path).unwrap_or_default();

        let bytes = fs::read(old_path).unwrap_or_default();
        let binary = is_binary(&bytes);

        let kind = if old_hash == new_hash {
            stats.unchanged += 1;
            ChangeKind::Unchanged
        } else {
            stats.modified += 1;
            ChangeKind::Modified
        };

        files.push(FileDiff {
            rel_path: (*key).clone(),
            kind,
            old_path: Some(old_path.clone()),
            new_path: Some(new_path.clone()),
            is_binary: binary,
            old_size: Some(*old_size),
            new_size: Some(*new_size),
        });
    }

    // Sort: changes first, then alphabetical
    files.sort_by(|a, b| {
        let order = |k: &ChangeKind| match k {
            ChangeKind::Modified => 0,
            ChangeKind::Added => 1,
            ChangeKind::Removed => 2,
            ChangeKind::Unchanged => 3,
        };
        order(&a.kind)
            .cmp(&order(&b.kind))
            .then_with(|| a.rel_path.cmp(&b.rel_path))
    });

    Ok(DiffResult {
        old_path: old_root.display().to_string(),
        new_path: new_root.display().to_string(),
        files,
        stats,
    })
}

pub fn get_file_diff_lines(file: &FileDiff) -> Vec<DiffLine> {
    if file.is_binary {
        return vec![DiffLine {
            tag: LineTag::Header,
            old_lineno: None,
            new_lineno: None,
            content: "  Binary file — cannot show text diff".to_string(),
        }];
    }

    match &file.kind {
        ChangeKind::Added => {
            if let Some(new_path) = &file.new_path {
                let content = fs::read_to_string(new_path).unwrap_or_default();
                let mut lines = vec![DiffLine {
                    tag: LineTag::Header,
                    old_lineno: None,
                    new_lineno: None,
                    content: format!("  New file: {}", new_path.display()),
                }];
                for (i, line) in content.lines().enumerate() {
                    lines.push(DiffLine {
                        tag: LineTag::Insert,
                        old_lineno: None,
                        new_lineno: Some(i + 1),
                        content: line.to_string(),
                    });
                }
                return lines;
            }
        }
        ChangeKind::Removed => {
            if let Some(old_path) = &file.old_path {
                let content = fs::read_to_string(old_path).unwrap_or_default();
                let mut lines = vec![DiffLine {
                    tag: LineTag::Header,
                    old_lineno: None,
                    new_lineno: None,
                    content: format!("  Deleted file: {}", old_path.display()),
                }];
                for (i, line) in content.lines().enumerate() {
                    lines.push(DiffLine {
                        tag: LineTag::Delete,
                        old_lineno: Some(i + 1),
                        new_lineno: None,
                        content: line.to_string(),
                    });
                }
                return lines;
            }
        }
        ChangeKind::Unchanged => {
            return vec![DiffLine {
                tag: LineTag::Header,
                old_lineno: None,
                new_lineno: None,
                content: "  No changes in this file.".to_string(),
            }];
        }
        ChangeKind::Modified => {
            if let (Some(old_path), Some(new_path)) = (&file.old_path, &file.new_path) {
                let old_content = fs::read_to_string(old_path).unwrap_or_default();
                let new_content = fs::read_to_string(new_path).unwrap_or_default();
                return build_diff_lines(&old_content, &new_content);
            }
        }
    }

    vec![]
}

fn build_diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(old, new);

    let mut result = Vec::new();
    let mut old_lineno = 0usize;
    let mut new_lineno = 0usize;

    for group in diff.grouped_ops(3) {
        // Hunk header
        let first = group.first().unwrap();
        let last = group.last().unwrap();
        let old_start = first.old_range().start + 1;
        let new_start = first.new_range().start + 1;
        let old_end = last.old_range().end;
        let new_end = last.new_range().end;
        result.push(DiffLine {
            tag: LineTag::Header,
            old_lineno: None,
            new_lineno: None,
            content: format!(
                "@@ -{},{} +{},{} @@",
                old_start,
                old_end.saturating_sub(old_start - 1),
                new_start,
                new_end.saturating_sub(new_start - 1)
            ),
        });

        for op in &group {
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Delete => {
                        old_lineno += 1;
                        LineTag::Delete
                    }
                    ChangeTag::Insert => {
                        new_lineno += 1;
                        LineTag::Insert
                    }
                    ChangeTag::Equal => {
                        old_lineno += 1;
                        new_lineno += 1;
                        LineTag::Equal
                    }
                };

                let content = change.value().trim_end_matches('\n').to_string();

                result.push(DiffLine {
                    tag: tag.clone(),
                    old_lineno: if tag == LineTag::Insert {
                        None
                    } else {
                        Some(old_lineno)
                    },
                    new_lineno: if tag == LineTag::Delete {
                        None
                    } else {
                        Some(new_lineno)
                    },
                    content,
                });
            }
        }
    }

    if result.is_empty() {
        result.push(DiffLine {
            tag: LineTag::Header,
            old_lineno: None,
            new_lineno: None,
            content: "  (no textual changes detected)".to_string(),
        });
    }

    result
}
