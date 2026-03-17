mod diff;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use diff::{ChangeKind, DiffOptions, DiffResult, DiffUpdate, compute_diff, compute_diff_async};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// 🍡 mochidetect — smart diff tool for comparing versions/projects
#[derive(Parser, Debug)]
#[command(
    name = "mochidetect",
    version = "0.1.1",
    about = "Smart diff tool for comparing two project versions or directories",
    long_about = "\
🍡 mochidetect — smart version diff\n\
\n\
Compare two directories, project versions, or files.\n\
Launches an interactive TUI by default with live file-watching.\n\
\n\
Examples:\n\
  mochidetect ./v1 ./v2\n\
  mochidetect ./v1 ./v2 --plain\n\
  mochidetect ./v1 ./v2 -I '*.lock|dist/**'\n\
  mochidetect ./v1 ./v2 --gitignore --ignore-whitespace\n\
  mochidetect file_a.py file_b.py --plain"
)]
struct Cli {
    /// First path (old version)
    old: String,
    /// Second path (new version)
    new: String,

    /// Print plain output instead of launching the TUI
    #[arg(short, long)]
    plain: bool,
    /// Show unchanged files too (hidden by default)
    #[arg(short = 'a', long)]
    all: bool,
    /// Filter by file extension (e.g. rs, py, js)
    #[arg(short, long)]
    ext: Option<String>,
    /// Summary only — counts, no file listing
    #[arg(short, long)]
    summary: bool,
    /// Ignore glob patterns (repeatable). Supports | and spaces:
    ///   -I '*.log|*.lock'   -I 'dist/**'
    #[arg(short = 'I', long = "ignore", value_name = "PATTERN")]
    ignore: Vec<String>,
    /// Respect .gitignore / .ignore files
    #[arg(short, long)]
    gitignore: bool,
    /// Treat files that differ only in whitespace as unchanged
    #[arg(short = 'W', long)]
    ignore_whitespace: bool,
    /// Disable live file watching in TUI mode
    #[arg(long)]
    no_watch: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let old_path = Path::new(&cli.old);
    let new_path = Path::new(&cli.new);

    if !old_path.exists() {
        anyhow::bail!("Path does not exist: {}", cli.old);
    }
    if !new_path.exists() {
        anyhow::bail!("Path does not exist: {}", cli.new);
    }

    let opts = DiffOptions {
        ignore_patterns: cli.ignore.clone(),
        use_gitignore: cli.gitignore,
        ignore_whitespace: cli.ignore_whitespace,
    };

    if cli.plain || cli.summary {
        let result = compute_diff(old_path, new_path, &opts)
            .with_context(|| format!("Failed to diff '{}' and '{}'", cli.old, cli.new))?;
        print_plain(&result, &cli);
        return Ok(());
    }

    // ── TUI mode ─────────────────────────────────────────────────────────────
    // 1. Create the shared sender — watcher will borrow this to send WatchEvent
    //    on any filesystem change. When TUI rescans it swaps in a new sender.
    let (tx, rx) = mpsc::channel::<DiffUpdate>();
    let shared_tx: Arc<Mutex<mpsc::Sender<DiffUpdate>>> = Arc::new(Mutex::new(tx.clone()));

    // 2. Start background diff thread — TUI is up before this finishes
    {
        let old_pb = old_path.to_path_buf();
        let new_pb = new_path.to_path_buf();
        let opts_c = opts.clone();
        let tx_c = tx;
        std::thread::spawn(move || compute_diff_async(old_pb, new_pb, opts_c, tx_c));
    }

    // 3. Start filesystem watcher thread (skip for single-file comparisons)
    let _watcher_handle = if !cli.no_watch && (old_path.is_dir() || new_path.is_dir()) {
        let stx = Arc::clone(&shared_tx);
        let old_pb = old_path.to_path_buf();
        let new_pb = new_path.to_path_buf();
        Some(std::thread::spawn(move || {
            if let Err(e) = run_watcher(old_pb, new_pb, stx) {
                eprintln!("watcher error: {}", e);
            }
        }))
    } else {
        None
    };

    // 4. Run TUI — hands back control when user quits
    tui::run_tui(rx, cli.old, cli.new, opts, shared_tx)
}

// ─── Filesystem watcher ───────────────────────────────────────────────────────

fn run_watcher(
    old_path: std::path::PathBuf,
    new_path: std::path::PathBuf,
    shared_tx: Arc<Mutex<mpsc::Sender<DiffUpdate>>>,
) -> Result<()> {
    let (ntx, nrx) = std::sync::mpsc::channel();

    // OS-native watcher: inotify on Linux, FSEvents on macOS.
    // Zero CPU when files are not changing — no polling at all.
    let mut watcher = RecommendedWatcher::new(ntx, Config::default())?;

    // Watch whichever paths are directories
    if old_path.is_dir() {
        watcher.watch(&old_path, RecursiveMode::Recursive)?;
    }
    if new_path.is_dir() {
        watcher.watch(&new_path, RecursiveMode::Recursive)?;
    }

    // Debounce: only forward one event per 400ms window
    let mut last_event = std::time::Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or(std::time::Instant::now());

    for event_result in nrx {
        let event = match event_result {
            Ok(e) => e,
            Err(_) => continue,
        };

        // Skip noisy events that should never trigger a rescan:
        // editor swap/temp files and git internal writes
        let is_noise = event.paths.iter().all(|p| {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let noisy = name.ends_with(".swp")
                || name.ends_with(".swo")
                || name.ends_with('~')
                || name.starts_with(".#")
                || name == "4913"
                || name.ends_with(".tmp");
            let in_git = p.components().any(|c| c.as_os_str() == ".git");
            noisy || in_git
        });
        if is_noise {
            continue;
        }

        // Debounce: collapse rapid bursts (editor saves multiple files fast)
        let now = std::time::Instant::now();
        if now.duration_since(last_event) < Duration::from_millis(800) {
            continue;
        }
        last_event = now;
        shared_tx.lock().unwrap().send(DiffUpdate::WatchEvent).ok();
    }
    Ok(())
}

// ─── Plain output ─────────────────────────────────────────────────────────────

fn print_plain(result: &DiffResult, cli: &Cli) {
    println!();
    println!("  🍡 mochidetect");
    println!("  {} → {}", result.old_path, result.new_path);
    println!();
    let s = &result.stats;
    println!(
        "  +{} added  -{} removed  ~{} modified  ={} unchanged  ({} total changes)",
        s.added,
        s.removed,
        s.modified,
        s.unchanged,
        s.total_changes()
    );
    println!();
    if cli.summary {
        return;
    }

    for file in &result.files {
        if !cli.all && file.kind == ChangeKind::Unchanged {
            continue;
        }
        if let Some(ref ext) = cli.ext {
            if file.extension() != ext.to_lowercase() {
                continue;
            }
        }
        let size_note = match (&file.old_size, &file.new_size) {
            (Some(a), Some(b)) if a != b => format!(" ({} → {} bytes)", a, b),
            (None, Some(b)) => format!(" ({} bytes)", b),
            (Some(a), None) => format!(" ({} bytes)", a),
            _ => String::new(),
        };
        println!(
            "  {} [{:8}] {}{}{}",
            file.kind.symbol(),
            file.kind.label(),
            file.rel_path.display(),
            if file.is_binary { " [binary]" } else { "" },
            size_note
        );
    }
    println!();
}
