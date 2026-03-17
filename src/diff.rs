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
            ChangeKind::Added     => "+",
            ChangeKind::Removed   => "-",
            ChangeKind::Modified  => "~",
            ChangeKind::Unchanged => "=",
        }
    }
    pub fn label(&self) -> &str {
        match self {
            ChangeKind::Added     => "ADDED",
            ChangeKind::Removed   => "REMOVED",
            ChangeKind::Modified  => "MODIFIED",
            ChangeKind::Unchanged => "UNCHANGED",
        }
    }
    pub fn is_changed(&self) -> bool {
        !matches!(self, ChangeKind::Unchanged)
    }
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    pub rel_path:  PathBuf,
    pub kind:      ChangeKind,
    pub old_path:  Option<PathBuf>,
    pub new_path:  Option<PathBuf>,
    pub is_binary: bool,
    pub old_size:  Option<u64>,
    pub new_size:  Option<u64>,
}

impl FileDiff {
    pub fn extension(&self) -> String {
        self.rel_path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase()
    }
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub tag:        LineTag,
    pub old_lineno: Option<usize>,
    pub new_lineno: Option<usize>,
    pub content:    String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum LineTag { Equal, Insert, Delete, Header }

#[derive(Debug)]
pub struct DiffResult {
    pub old_path: String,
    pub new_path: String,
    pub files:    Vec<FileDiff>,
    pub stats:    DiffStats,
    pub skipped:  usize,
}

#[derive(Debug, Default, Clone)]
pub struct DiffStats {
    pub added:     usize,
    pub removed:   usize,
    pub modified:  usize,
    pub unchanged: usize,
}

impl DiffStats {
    pub fn total_changes(&self) -> usize { self.added + self.removed + self.modified }
}

/// Messages streamed from the background diff thread to the TUI.
pub enum DiffUpdate {
    /// A single file result — arrives as soon as it's computed.
    File(FileDiff),
    /// All files have been processed — list is complete.
    Done,
    /// A fatal error occurred.
    Error(String),
    /// Filesystem watcher detected a change — TUI should rescan.
    WatchEvent,
}

// ─── Diff options ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Default)]
pub struct DiffOptions {
    /// Glob ignore patterns. Supports `|` and space separation.
    pub ignore_patterns:    Vec<String>,
    /// Respect .gitignore / .ignore files.
    pub use_gitignore:      bool,
    /// Treat files that differ only in whitespace as Unchanged.
    pub ignore_whitespace:  bool,
}

impl DiffOptions {
    pub fn build_globset(&self) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        for raw in &self.ignore_patterns {
            for pat in raw.split(['|', ' ', '\t']).map(str::trim).filter(|s| !s.is_empty()) {
                builder.add(Glob::new(pat)?);
                // Bare names (no / or *) → also match as directory prefix
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

/// Read first 512 bytes — fast binary check without loading the whole file.
fn peek_binary(path: &Path) -> bool {
    let mut buf = [0u8; 512];
    match File::open(path) {
        Ok(mut f) => { let n = f.read(&mut buf).unwrap_or(0); buf[..n].contains(&0) }
        Err(_)    => false,
    }
}

/// Hash a file in 64 KB chunks.
fn hash_file(path: &Path) -> String {
    match File::open(path) {
        Ok(mut f) => {
            let mut h   = Sha256::new();
            let mut buf = [0u8; 65536];
            loop {
                match f.read(&mut buf) {
                    Ok(0)  => break,
                    Ok(n)  => h.update(&buf[..n]),
                    Err(_) => return String::new(),
                }
            }
            hex::encode(h.finalize())
        }
        Err(_) => String::new(),
    }
}

/// Hash file content with trailing whitespace stripped per line.
/// Used for --ignore-whitespace: two files that differ only in spacing
/// will produce the same hash and be treated as Unchanged.
fn hash_file_normalized(path: &Path) -> String {
    let content = match fs::read_to_string(path) {
        Ok(s)  => s,
        Err(_) => return String::new(),
    };
    let mut h = Sha256::new();
    for line in content.lines() {
        h.update(line.trim_end().as_bytes());
        h.update(b"\n");
    }
    hex::encode(h.finalize())
}

// ─── File collection ──────────────────────────────────────────────────────────

fn is_always_skip(name: &str) -> bool {
    matches!(name, ".git" | ".hg" | ".svn")
}

fn collect_files(root: &Path, opts: &DiffOptions, globs: &GlobSet)
    -> Result<HashMap<PathBuf, (PathBuf, u64)>>
{
    if root.is_file() {
        let meta = fs::metadata(root)?;
        let filename = root.file_name().unwrap_or_default();
        let mut map = HashMap::new();
        map.insert(PathBuf::from(filename), (root.to_path_buf(), meta.len()));
        return Ok(map);
    }

    let map: Arc<Mutex<HashMap<PathBuf, (PathBuf, u64)>>> =
        Arc::new(Mutex::new(HashMap::new()));

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

    let map_clone   = Arc::clone(&map);
    let root_owned  = root.to_path_buf();
    let globs_clone = globs.clone();

    walker.run(move || {
        let map_ref   = Arc::clone(&map_clone);
        let root_ref  = root_owned.clone();
        let globs_ref = globs_clone.clone();

        Box::new(move |entry_result| {
            use ignore::WalkState;
            let entry = match entry_result { Ok(e) => e, Err(_) => return WalkState::Continue };
            let path   = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let is_file= entry.file_type().map(|t| t.is_file()).unwrap_or(false);

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if is_always_skip(name) { return WalkState::Skip; }
                if name.starts_with('.') && path != root_ref {
                    return if is_dir { WalkState::Skip } else { WalkState::Continue };
                }
            }

            if is_dir {
                if let Ok(rel) = path.strip_prefix(&root_ref) {
                    if globs_ref.is_match(rel)
                        || path.file_name().map(|n| globs_ref.is_match(n)).unwrap_or(false)
                    {
                        return WalkState::Skip;
                    }
                }
                return WalkState::Continue;
            }

            if !is_file { return WalkState::Continue; }

            let rel = match path.strip_prefix(&root_ref) {
                Ok(r)  => r.to_path_buf(),
                Err(_) => return WalkState::Continue,
            };
            if globs_ref.is_match(&rel) { return WalkState::Continue; }

            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            map_ref.lock().unwrap().insert(rel, (path.to_path_buf(), size));
            WalkState::Continue
        })
    });

    Ok(Arc::try_unwrap(map).expect("arc").into_inner().expect("mutex"))
}

// ─── Sync API (--plain / --summary) ──────────────────────────────────────────

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

    let added_keys:   Vec<&PathBuf> = new_keys.difference(&old_keys).copied().collect();
    let removed_keys: Vec<&PathBuf> = old_keys.difference(&new_keys).copied().collect();
    let common_keys:  Vec<&PathBuf> = old_keys.intersection(&new_keys).copied().collect();

    let added: Vec<FileDiff> = added_keys.par_iter().map(|key| {
        let (path, size) = &new_files[*key];
        FileDiff { rel_path: (*key).clone(), kind: ChangeKind::Added,
            old_path: None, new_path: Some(path.clone()),
            is_binary: peek_binary(path), old_size: None, new_size: Some(*size) }
    }).collect();

    let removed: Vec<FileDiff> = removed_keys.par_iter().map(|key| {
        let (path, size) = &old_files[*key];
        FileDiff { rel_path: (*key).clone(), kind: ChangeKind::Removed,
            old_path: Some(path.clone()), new_path: None,
            is_binary: peek_binary(path), old_size: Some(*size), new_size: None }
    }).collect();

    let common: Vec<FileDiff> = common_keys.par_iter().map(|key| {
        classify_common(key, &old_files, &new_files, opts)
    }).collect();

    let mut stats = DiffStats { added: added.len(), removed: removed.len(), ..Default::default() };
    for f in &common {
        match f.kind { ChangeKind::Modified => stats.modified += 1, ChangeKind::Unchanged => stats.unchanged += 1, _ => {} }
    }

    let mut files: Vec<FileDiff> = added.into_iter().chain(removed).chain(common).collect();
    sort_files(&mut files);

    Ok(DiffResult { old_path: old_root.display().to_string(), new_path: new_root.display().to_string(), skipped: 0, files, stats })
}

// ─── Streaming async API (TUI) ────────────────────────────────────────────────

pub fn compute_diff_async(
    old_root: PathBuf,
    new_root: PathBuf,
    opts: DiffOptions,
    tx: std::sync::mpsc::Sender<DiffUpdate>,
) {
    let globs = match opts.build_globset() {
        Ok(g) => g,
        Err(e) => { tx.send(DiffUpdate::Error(e.to_string())).ok(); return; }
    };

    let (old_res, new_res) = rayon::join(
        || collect_files(&old_root, &opts, &globs),
        || collect_files(&new_root, &opts, &globs),
    );

    let old_files = Arc::new(match old_res { Ok(m) => m, Err(e) => { tx.send(DiffUpdate::Error(e.to_string())).ok(); return; } });
    let new_files = Arc::new(match new_res { Ok(m) => m, Err(e) => { tx.send(DiffUpdate::Error(e.to_string())).ok(); return; } });

    let old_set: HashSet<PathBuf> = old_files.keys().cloned().collect();
    let new_set: HashSet<PathBuf> = new_files.keys().cloned().collect();

    let added_keys:   Vec<PathBuf> = new_set.difference(&old_set).cloned().collect();
    let removed_keys: Vec<PathBuf> = old_set.difference(&new_set).cloned().collect();
    let common_keys:  Vec<PathBuf> = old_set.intersection(&new_set).cloned().collect();

    // Phase 1 — Added/Removed: no hashing, send immediately
    for key in added_keys {
        let (path, size) = &new_files[&key];
        if tx.send(DiffUpdate::File(FileDiff {
            rel_path: key, kind: ChangeKind::Added,
            old_path: None, new_path: Some(path.clone()),
            is_binary: peek_binary(path), old_size: None, new_size: Some(*size),
        })).is_err() { return; }
    }
    for key in removed_keys {
        let (path, size) = &old_files[&key];
        if tx.send(DiffUpdate::File(FileDiff {
            rel_path: key, kind: ChangeKind::Removed,
            old_path: Some(path.clone()), new_path: None,
            is_binary: peek_binary(path), old_size: Some(*size), new_size: None,
        })).is_err() { return; }
    }

    // Phase 2 — Common: hash in parallel, stream each result the moment it's ready
    let tx_done = tx.clone();
    let of = Arc::clone(&old_files);
    let nf = Arc::clone(&new_files);

    common_keys.into_par_iter().for_each_with(tx, move |tx, key| {
        let file = classify_common(&key, &of, &nf, &opts);
        tx.send(DiffUpdate::File(file)).ok();
    });

    tx_done.send(DiffUpdate::Done).ok();
}

// ─── Shared classification logic ──────────────────────────────────────────────

fn classify_common(
    key: &PathBuf,
    old_files: &HashMap<PathBuf, (PathBuf, u64)>,
    new_files:  &HashMap<PathBuf, (PathBuf, u64)>,
    opts: &DiffOptions,
) -> FileDiff {
    let (old_path, old_size) = &old_files[key];
    let (new_path, new_size) = &new_files[key];

    let kind = if old_size != new_size && !opts.ignore_whitespace {
        // Sizes differ → definitely modified (skip hashing unless whitespace mode)
        ChangeKind::Modified
    } else {
        // Same size OR whitespace mode → must compare content
        let (old_hash, new_hash) = if opts.ignore_whitespace {
            rayon::join(|| hash_file_normalized(old_path), || hash_file_normalized(new_path))
        } else {
            rayon::join(|| hash_file(old_path), || hash_file(new_path))
        };
        if old_hash == new_hash { ChangeKind::Unchanged } else { ChangeKind::Modified }
    };

    // Only peek binary for changed files
    let binary = kind != ChangeKind::Unchanged && peek_binary(old_path);

    FileDiff {
        rel_path:  key.clone(),
        kind,
        old_path:  Some(old_path.clone()),
        new_path:  Some(new_path.clone()),
        is_binary: binary,
        old_size:  Some(*old_size),
        new_size:  Some(*new_size),
    }
}

fn sort_files(files: &mut Vec<FileDiff>) {
    files.sort_by(|a, b| {
        let order = |k: &ChangeKind| match k {
            ChangeKind::Modified  => 0u8,
            ChangeKind::Added     => 1,
            ChangeKind::Removed   => 2,
            ChangeKind::Unchanged => 3,
        };
        order(&a.kind).cmp(&order(&b.kind)).then_with(|| a.rel_path.cmp(&b.rel_path))
    });
}

// ─── Diff line rendering (lazy — reads from disk only when file is selected) ──

pub fn get_file_diff_lines(file: &FileDiff) -> Vec<DiffLine> {
    if file.is_binary {
        return vec![DiffLine { tag: LineTag::Header, old_lineno: None, new_lineno: None,
            content: "  Binary file — cannot show text diff".to_string() }];
    }
    match &file.kind {
        ChangeKind::Added => {
            if let Some(p) = &file.new_path {
                let content = fs::read_to_string(p).unwrap_or_default();
                let mut lines = vec![DiffLine { tag: LineTag::Header, old_lineno: None,
                    new_lineno: None, content: format!("  New file: {}", p.display()) }];
                for (i, line) in content.lines().enumerate() {
                    lines.push(DiffLine { tag: LineTag::Insert, old_lineno: None, new_lineno: Some(i + 1), content: line.to_string() });
                }
                return lines;
            }
        }
        ChangeKind::Removed => {
            if let Some(p) = &file.old_path {
                let content = fs::read_to_string(p).unwrap_or_default();
                let mut lines = vec![DiffLine { tag: LineTag::Header, old_lineno: None,
                    new_lineno: None, content: format!("  Deleted file: {}", p.display()) }];
                for (i, line) in content.lines().enumerate() {
                    lines.push(DiffLine { tag: LineTag::Delete, old_lineno: Some(i + 1), new_lineno: None, content: line.to_string() });
                }
                return lines;
            }
        }
        ChangeKind::Unchanged => {
            return vec![DiffLine { tag: LineTag::Header, old_lineno: None, new_lineno: None,
                content: "  No changes in this file.".to_string() }];
        }
        ChangeKind::Modified => {
            if let (Some(op), Some(np)) = (&file.old_path, &file.new_path) {
                return build_diff_lines(
                    &fs::read_to_string(op).unwrap_or_default(),
                    &fs::read_to_string(np).unwrap_or_default(),
                );
            }
        }
    }
    vec![]
}

fn build_diff_lines(old: &str, new: &str) -> Vec<DiffLine> {
    let diff = TextDiff::from_lines(old, new);
    let mut result = Vec::new();
    let mut old_no = 0usize;
    let mut new_no = 0usize;

    for group in diff.grouped_ops(3) {
        let first = group.first().unwrap();
        let last  = group.last().unwrap();
        let os = first.old_range().start + 1;
        let ns = first.new_range().start + 1;
        let oe = last.old_range().end;
        let ne = last.new_range().end;
        result.push(DiffLine { tag: LineTag::Header, old_lineno: None, new_lineno: None,
            content: format!("@@ -{},{} +{},{} @@", os, oe.saturating_sub(os-1), ns, ne.saturating_sub(ns-1)) });

        for op in &group {
            for change in diff.iter_changes(op) {
                let tag = match change.tag() {
                    ChangeTag::Delete => { old_no += 1; LineTag::Delete }
                    ChangeTag::Insert => { new_no += 1; LineTag::Insert }
                    ChangeTag::Equal  => { old_no += 1; new_no += 1; LineTag::Equal }
                };
                let content = change.value().trim_end_matches('\n').to_string();
                result.push(DiffLine {
                    old_lineno: if tag == LineTag::Insert { None } else { Some(old_no) },
                    new_lineno: if tag == LineTag::Delete { None } else { Some(new_no) },
                    tag, content,
                });
            }
        }
    }

    if result.is_empty() {
        result.push(DiffLine { tag: LineTag::Header, old_lineno: None, new_lineno: None,
            content: "  (no textual changes detected)".to_string() });
    }
    result
}

