use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use similar::{ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ─── Public types ─────────────────────────────────────────────────────────────

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

// ─── Streaming update type ────────────────────────────────────────────────────

/// Messages streamed from the background diff thread to the TUI.
pub enum DiffUpdate {
    /// A single file result — arrives as soon as it's known.
    File(FileDiff),
    /// All files have been processed. List is now complete.
    Done,
    /// A fatal error occurred.
    Error(String),
}

// ─── Diff options ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Extra ignore patterns — supports glob syntax and `|` separation.
    /// e.g. "*.log", "dist/**", "*.log|*.lock|build/"
    pub ignore_patterns: Vec<String>,
    /// Whether to respect .gitignore / .ignore files
    pub use_gitignore: bool,
}

impl DiffOptions {
    pub fn build_globset(&self) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for raw in &self.ignore_patterns {
            for pat in raw
                .split(['|', ' ', '\t'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                builder.add(Glob::new(pat)?);

                // Auto-expand bare names (no `/` or `*`) so that e.g. "target"
                // matches the directory itself AND everything inside it at any
                // depth — same behaviour as .gitignore bare-name rules.
                //
                //   "target"  →  target/**      (contents of top-level target/)
                //                **/target/**   (contents of nested target/)
                //                **/target      (the dir node itself when nested)
                //
                // Patterns that already contain `/` or `*` are intentional glob
                // expressions and are left untouched.
                if !pat.contains('/') && !pat.contains('*') {
                    builder.add(Glob::new(&format!("{}/**", pat))?);
                    builder.add(Glob::new(&format!("**/{}/**", pat))?);
                    builder.add(Glob::new(&format!("**/{}", pat))?);
                }
            }
        }
        Ok(builder.build()?)
    }
}

// ─── Fast I/O helpers ─────────────────────────────────────────────────────────

/// Peek at the first 512 bytes to decide if it's binary — fast, avoids MB of I/O.
fn peek_binary(path: &Path) -> bool {
    let mut buf = [0u8; 512];
    match File::open(path) {
        Ok(mut f) => {
            let n = f.read(&mut buf).unwrap_or(0);
            buf[..n].contains(&0)
        }
        Err(_) => false,
    }
}

/// SHA-256 hash a file in 64 KB chunks. Only called when sizes match.
fn hash_file(path: &Path) -> String {
    match File::open(path) {
        Ok(mut f) => {
            let mut hasher = Sha256::new();
            let mut buf = [0u8; 65536];
            loop {
                match f.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => hasher.update(&buf[..n]),
                    Err(_) => return String::new(),
                }
            }
            hex::encode(hasher.finalize())
        }
        Err(_) => String::new(),
    }
}

// ─── File collection ──────────────────────────────────────────────────────────

fn is_always_skip(name: &str) -> bool {
    matches!(name, ".git" | ".hg" | ".svn")
}

/// Walk a directory tree and return relative_path → (absolute_path, size).
fn collect_files(
    root: &Path,
    opts: &DiffOptions,
    globs: &GlobSet,
) -> Result<HashMap<PathBuf, (PathBuf, u64)>> {
    if root.is_file() {
        let meta = fs::metadata(root)?;
        let filename = root.file_name().unwrap_or_default();
        let mut map = HashMap::new();
        map.insert(PathBuf::from(filename), (root.to_path_buf(), meta.len()));
        return Ok(map);
    }

    let map: Arc<Mutex<HashMap<PathBuf, (PathBuf, u64)>>> = Arc::new(Mutex::new(HashMap::new()));

    let walker = WalkBuilder::new(root)
        .hidden(false)
        .git_ignore(opts.use_gitignore)
        .git_global(false)
        .git_exclude(false)
        .require_git(false)
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
            let is_dir  = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if is_always_skip(name) {
                    return WalkState::Skip; // prune .git/.hg/.svn trees
                }
                if name.starts_with('.') && path != root_ref {
                    // Hidden dirs → prune whole subtree; hidden files → skip
                    return if is_dir { WalkState::Skip } else { WalkState::Continue };
                }
            }

            // ── For directories: check globs and prune whole subtree if matched.
            // This is what makes `-I target` as fast as find's -prune — we never
            // descend into ignored directories at all.
            if is_dir {
                if let Ok(rel) = path.strip_prefix(&root_ref) {
                    // Check relative path ("target", "src/target") or bare name
                    if globs_ref.is_match(rel)
                        || path
                            .file_name()
                            .map(|n| globs_ref.is_match(n))
                            .unwrap_or(false)
                    {
                        return WalkState::Skip;
                    }
                }
                return WalkState::Continue;
            }

            if !is_file {
                return WalkState::Continue;
            }

            let rel = match path.strip_prefix(&root_ref) {
                Ok(r) => r.to_path_buf(),
                Err(_) => return WalkState::Continue,
            };

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

    Ok(Arc::try_unwrap(map)
        .expect("arc still shared")
        .into_inner()
        .expect("mutex poisoned"))
}

// ─── Synchronous API (used by --plain / --summary) ───────────────────────────

pub fn compute_diff(old_root: &Path, new_root: &Path, opts: &DiffOptions) -> Result<DiffResult> {
    let globs = opts.build_globset()?;

    let (old_res, new_res) = rayon::join(
        || collect_files(old_root, opts, &globs),
        || collect_files(new_root, opts, &globs),
    );
    let old_files = old_res?;
    let new_files = new_res?;

    let old_keys: HashSet<&PathBuf> = old_files.keys().collect();
    let new_keys: HashSet<&PathBuf> = new_files.keys().collect();

    let added_keys: Vec<&PathBuf> = new_keys.difference(&old_keys).copied().collect();
    let removed_keys: Vec<&PathBuf> = old_keys.difference(&new_keys).copied().collect();
    let common_keys: Vec<&PathBuf> = old_keys.intersection(&new_keys).copied().collect();

    let added: Vec<FileDiff> = added_keys
        .par_iter()
        .map(|key| {
            let (path, size) = &new_files[*key];
            FileDiff {
                rel_path: (*key).clone(),
                kind: ChangeKind::Added,
                old_path: None,
                new_path: Some(path.clone()),
                is_binary: peek_binary(path),
                old_size: None,
                new_size: Some(*size),
            }
        })
        .collect();

    let removed: Vec<FileDiff> = removed_keys
        .par_iter()
        .map(|key| {
            let (path, size) = &old_files[*key];
            FileDiff {
                rel_path: (*key).clone(),
                kind: ChangeKind::Removed,
                old_path: Some(path.clone()),
                new_path: None,
                is_binary: peek_binary(path),
                old_size: Some(*size),
                new_size: None,
            }
        })
        .collect();

    let common: Vec<FileDiff> = common_keys
        .par_iter()
        .map(|key| {
            let (old_path, old_size) = &old_files[*key];
            let (new_path, new_size) = &new_files[*key];

            let kind = if old_size != new_size {
                ChangeKind::Modified
            } else {
                let (old_hash, new_hash) =
                    rayon::join(|| hash_file(old_path), || hash_file(new_path));
                if old_hash == new_hash {
                    ChangeKind::Unchanged
                } else {
                    ChangeKind::Modified
                }
            };

            let binary = if kind == ChangeKind::Unchanged {
                false
            } else {
                peek_binary(old_path)
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

// ─── Streaming / async API (used by TUI) ─────────────────────────────────────
//
// Strategy:
//   1. Walk both trees in parallel (fast — just filesystem metadata, no I/O)
//   2. Added/Removed are known immediately → sent right away
//   3. Common files are hashed in parallel via rayon → each result sent the
//      moment it's ready, so the TUI fills up live instead of all at once
//
// This means the TUI opens instantly, shows added/removed files within
// milliseconds, and modified/unchanged trickle in as SHA-256 completes.

pub fn compute_diff_async(
    old_root: PathBuf,
    new_root: PathBuf,
    opts: DiffOptions,
    tx: std::sync::mpsc::Sender<DiffUpdate>,
) {
    let globs = match opts.build_globset() {
        Ok(g) => g,
        Err(e) => {
            tx.send(DiffUpdate::Error(e.to_string())).ok();
            return;
        }
    };

    // ── Phase 1: parallel filesystem walk (metadata only, very fast) ─────────
    let (old_res, new_res) = rayon::join(
        || collect_files(&old_root, &opts, &globs),
        || collect_files(&new_root, &opts, &globs),
    );

    let old_files = match old_res {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tx.send(DiffUpdate::Error(e.to_string())).ok();
            return;
        }
    };
    let new_files = match new_res {
        Ok(m) => Arc::new(m),
        Err(e) => {
            tx.send(DiffUpdate::Error(e.to_string())).ok();
            return;
        }
    };

    let old_set: HashSet<PathBuf> = old_files.keys().cloned().collect();
    let new_set: HashSet<PathBuf> = new_files.keys().cloned().collect();

    let added_keys: Vec<PathBuf> = new_set.difference(&old_set).cloned().collect();
    let removed_keys: Vec<PathBuf> = old_set.difference(&new_set).cloned().collect();
    let common_keys: Vec<PathBuf> = old_set.intersection(&new_set).cloned().collect();

    // ── Phase 2: send Added immediately (no hashing needed) ──────────────────
    for key in added_keys {
        let (path, size) = &new_files[&key];
        let file = FileDiff {
            is_binary: peek_binary(path),
            rel_path: key,
            kind: ChangeKind::Added,
            old_path: None,
            new_path: Some(path.clone()),
            old_size: None,
            new_size: Some(*size),
        };
        if tx.send(DiffUpdate::File(file)).is_err() {
            return; // TUI closed
        }
    }

    // ── Phase 2: send Removed immediately (no hashing needed) ────────────────
    for key in removed_keys {
        let (path, size) = &old_files[&key];
        let file = FileDiff {
            is_binary: peek_binary(path),
            rel_path: key,
            kind: ChangeKind::Removed,
            old_path: Some(path.clone()),
            new_path: None,
            old_size: Some(*size),
            new_size: None,
        };
        if tx.send(DiffUpdate::File(file)).is_err() {
            return;
        }
    }

    // ── Phase 3: hash common files in parallel, stream each result live ───────
    // Clone tx before for_each_with consumes it, so we can send Done afterward.
    let tx_done = tx.clone();
    let of = Arc::clone(&old_files);
    let nf = Arc::clone(&new_files);

    common_keys
        .into_par_iter()
        .for_each_with(tx, move |tx, key| {
            let (old_path, old_size) = match of.get(&key) {
                Some(v) => v,
                None => return,
            };
            let (new_path, new_size) = match nf.get(&key) {
                Some(v) => v,
                None => return,
            };

            let kind = if old_size != new_size {
                // Size mismatch → definitely modified, skip hashing entirely
                ChangeKind::Modified
            } else {
                let old_hash = hash_file(old_path);
                let new_hash = hash_file(new_path);
                if old_hash == new_hash {
                    ChangeKind::Unchanged
                } else {
                    ChangeKind::Modified
                }
            };

            // Only peek binary for changed files — unchanged ones rarely need it
            let binary = kind != ChangeKind::Unchanged && peek_binary(old_path);

            tx.send(DiffUpdate::File(FileDiff {
                rel_path: key,
                kind,
                old_path: Some(old_path.clone()),
                new_path: Some(new_path.clone()),
                is_binary: binary,
                old_size: Some(*old_size),
                new_size: Some(*new_size),
            }))
            .ok();
        });

    // All workers finished — signal done
    tx_done.send(DiffUpdate::Done).ok();
}

// ─── Diff line rendering (lazy, reads from disk on demand) ───────────────────

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
            if let Some(p) = &file.new_path {
                let content = fs::read_to_string(p).unwrap_or_default();
                let mut lines = vec![DiffLine {
                    tag: LineTag::Header,
                    old_lineno: None,
                    new_lineno: None,
                    content: format!("  New file: {}", p.display()),
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
            if let Some(p) = &file.old_path {
                let content = fs::read_to_string(p).unwrap_or_default();
                let mut lines = vec![DiffLine {
                    tag: LineTag::Header,
                    old_lineno: None,
                    new_lineno: None,
                    content: format!("  Deleted file: {}", p.display()),
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
            if let (Some(old_p), Some(new_p)) = (&file.old_path, &file.new_path) {
                let old_c = fs::read_to_string(old_p).unwrap_or_default();
                let new_c = fs::read_to_string(new_p).unwrap_or_default();
                return build_diff_lines(&old_c, &new_c);
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
                new_end.saturating_sub(new_start - 1),
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

