use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ─── Public types ────────────────────────────────────────────────────────────

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
    pub fn is_changed(&self) -> bool {
        !matches!(self, ChangeKind::Unchanged)
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
    pub skipped: usize,
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

// ─── Diff options ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Extra ignore patterns (glob-style, e.g. "*.log", "build/")
    pub ignore_patterns: Vec<String>,
    /// Whether to respect .gitignore / .ignore files
    pub use_gitignore: bool,
}

impl DiffOptions {
    pub fn build_globset(&self) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for pat in &self.ignore_patterns {
            builder.add(Glob::new(pat)?);
        }
        Ok(builder.build()?)
    }
}

// ─── Internals ────────────────────────────────────────────────────────────────

fn hash_file(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => {
            let mut h = Sha256::new();
            h.update(&bytes);
            hex::encode(h.finalize())
        }
        Err(_) => String::new(),
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8000)].contains(&0)
}

/// Classify a path component as always-skipped (regardless of gitignore).
fn is_always_skip(name: &str) -> bool {
    matches!(name, ".git" | ".hg" | ".svn")
}

/// Walk a directory tree, respecting .gitignore when requested and applying
/// user ignore patterns, returning a map of relative_path → (absolute_path, size).
fn collect_files(
    root: &Path,
    opts: &DiffOptions,
    globs: &GlobSet,
) -> Result<HashMap<PathBuf, (PathBuf, u64)>> {
    // Single-file shortcut
    if root.is_file() {
        let meta = fs::metadata(root)?;
        let filename = root.file_name().unwrap_or_default();
        let mut map = HashMap::new();
        map.insert(PathBuf::from(filename), (root.to_path_buf(), meta.len()));
        return Ok(map);
    }

    let map: Arc<Mutex<HashMap<PathBuf, (PathBuf, u64)>>> = Arc::new(Mutex::new(HashMap::new()));

    // The `ignore` crate handles .gitignore, .ignore, global git excludes, etc.
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(opts.use_gitignore)
        .git_global(false)
        .git_exclude(false)
        .require_git(false) // honour .gitignore outside of git repos too
        .ignore(opts.use_gitignore)
        .follow_links(true)
        .threads(rayon::current_num_threads().min(8))
        .build_parallel();

    let map_clone = Arc::clone(&map);
    let root_owned = root.to_path_buf();
    let globs_clone = globs.clone();

    walker.run(move || {
        let map_ref = Arc::clone(&map_clone);
        let root_ref = root_owned.clone();
        let globs_ref = globs_clone.clone();

        Box::new(move |entry_result| {
            use ignore::WalkState;
            let entry = match entry_result {
                Ok(e) => e,
                Err(_) => return WalkState::Continue,
            };

            let path = entry.path();

            // Skip VCS dirs always
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if is_always_skip(name) {
                    return WalkState::Skip;
                }
                // Skip hidden files/dirs (but not the root itself)
                if name.starts_with('.') && path != root_ref {
                    return WalkState::Continue;
                }
            }

            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return WalkState::Continue;
            }

            let rel = match path.strip_prefix(&root_ref) {
                Ok(r) => r.to_path_buf(),
                Err(_) => return WalkState::Continue,
            };

            // User glob patterns
            if globs_ref.is_match(&rel) {
                return WalkState::Continue;
            }

            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            map_ref
                .lock()
                .unwrap()
                .insert(rel, (path.to_path_buf(), size));
            WalkState::Continue
        })
    });

    let result = Arc::try_unwrap(map)
        .expect("arc still shared")
        .into_inner()
        .expect("mutex poisoned");

    Ok(result)
}

// ─── Public API ───────────────────────────────────────────────────────────────

pub fn compute_diff(old_root: &Path, new_root: &Path, opts: &DiffOptions) -> Result<DiffResult> {
    let globs = opts.build_globset()?;

    // Collect file listings in parallel
    let (old_files_res, new_files_res) = rayon::join(
        || collect_files(old_root, opts, &globs),
        || collect_files(new_root, opts, &globs),
    );
    let old_files = old_files_res?;
    let new_files = new_files_res?;

    let old_keys: HashSet<&PathBuf> = old_files.keys().collect();
    let new_keys: HashSet<&PathBuf> = new_files.keys().collect();

    // Collect all work items
    let added_keys: Vec<&PathBuf> = new_keys.difference(&old_keys).copied().collect();
    let removed_keys: Vec<&PathBuf> = old_keys.difference(&new_keys).copied().collect();
    let common_keys: Vec<&PathBuf> = old_keys.intersection(&new_keys).copied().collect();

    // Process added/removed (read binary flag only, no hashing needed)
    let added: Vec<FileDiff> = added_keys
        .par_iter()
        .map(|key| {
            let (path, size) = &new_files[*key];
            let bytes = fs::read(path).unwrap_or_default();
            FileDiff {
                rel_path: (*key).clone(),
                kind: ChangeKind::Added,
                old_path: None,
                new_path: Some(path.clone()),
                is_binary: is_binary(&bytes),
                old_size: None,
                new_size: Some(*size),
            }
        })
        .collect();

    let removed: Vec<FileDiff> = removed_keys
        .par_iter()
        .map(|key| {
            let (path, size) = &old_files[*key];
            let bytes = fs::read(path).unwrap_or_default();
            FileDiff {
                rel_path: (*key).clone(),
                kind: ChangeKind::Removed,
                old_path: Some(path.clone()),
                new_path: None,
                is_binary: is_binary(&bytes),
                old_size: Some(*size),
                new_size: None,
            }
        })
        .collect();

    // Process common files — hash both sides in parallel pairs
    let common: Vec<FileDiff> = common_keys
        .par_iter()
        .map(|key| {
            let (old_path, old_size) = &old_files[*key];
            let (new_path, new_size) = &new_files[*key];

            // Quick size check — if sizes differ we know it's modified without full hash
            let size_changed = old_size != new_size;

            let (old_hash, new_hash) = if size_changed {
                // Still hash to be sure (size can rarely be same after modification)
                rayon::join(|| hash_file(old_path), || hash_file(new_path))
            } else {
                rayon::join(|| hash_file(old_path), || hash_file(new_path))
            };

            // Detect binary from old side
            let bytes_peek = fs::read(old_path).unwrap_or_default();
            let binary = is_binary(&bytes_peek);

            let kind = if old_hash == new_hash {
                ChangeKind::Unchanged
            } else {
                ChangeKind::Modified
            };

            FileDiff {
                rel_path: (*key).clone(),
                kind,
                old_path: Some(old_path.clone()),
                new_path: Some(new_path.clone()),
                is_binary: binary,
                old_size: Some(*old_size),
                new_size: Some(*new_size),
            }
        })
        .collect();

    // Build stats
    let mut stats = DiffStats {
        added: added.len(),
        removed: removed.len(),
        ..Default::default()
    };
    for f in &common {
        match f.kind {
            ChangeKind::Modified => stats.modified += 1,
            ChangeKind::Unchanged => stats.unchanged += 1,
            _ => {}
        }
    }

    // Merge and sort: modified → added → removed → unchanged
    let mut files: Vec<FileDiff> = added.into_iter().chain(removed).chain(common).collect();
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
        skipped: 0,
        files,
        stats,
    })
}

// ─── Diff line rendering ──────────────────────────────────────────────────────

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
                    tag,
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
