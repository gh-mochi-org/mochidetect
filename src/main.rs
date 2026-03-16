mod diff;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use diff::{ChangeKind, DiffOptions, DiffResult, compute_diff, compute_diff_async};
use std::path::Path;
use std::sync::mpsc;

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
Launches an interactive TUI by default. Use --plain for plain output.\n\
\n\
Examples:\n\
  mochidetect ./v1 ./v2\n\
  mochidetect ./v1 ./v2 --plain\n\
  mochidetect ./v1 ./v2 -I '*.lock' -I 'dist/**'\n\
  mochidetect ./v1 ./v2 --gitignore\n\
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

    /// Ignore glob patterns (repeatable): -I '*.lock' -I 'dist/**'
    #[arg(short = 'I', long = "ignore", value_name = "PATTERN")]
    ignore: Vec<String>,

    /// Respect .gitignore / .ignore files found in the trees
    #[arg(short, long)]
    gitignore: bool,
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
    };

    if cli.plain || cli.summary {
        // Plain mode: blocking diff then print — no TUI needed
        let result = compute_diff(old_path, new_path, &opts)
            .with_context(|| format!("Failed to diff '{}' and '{}'", cli.old, cli.new))?;
        print_plain(&result, &cli);
    } else {
        // TUI mode: open terminal immediately, diff runs in background thread,
        // results stream into the UI as they arrive.
        let (tx, rx) = mpsc::channel();
        let old_owned = old_path.to_path_buf();
        let new_owned = new_path.to_path_buf();
        let old_str = cli.old.clone();
        let new_str = cli.new.clone();

        std::thread::spawn(move || {
            compute_diff_async(old_owned, new_owned, opts, tx);
        });

        tui::run_tui(rx, old_str, new_str)?;
    }

    Ok(())
}

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

        let sym = file.kind.symbol();
        let label = file.kind.label();
        let path = file.rel_path.display();
        let binary_note = if file.is_binary { " [binary]" } else { "" };
        let size_note = match (&file.old_size, &file.new_size) {
            (Some(a), Some(b)) if a != b => format!(" ({} → {} bytes)", a, b),
            (None, Some(b)) => format!(" ({} bytes)", b),
            (Some(a), None) => format!(" ({} bytes)", a),
            _ => String::new(),
        };

        println!(
            "  {} [{:8}] {}{}{}",
            sym, label, path, binary_note, size_note
        );
    }

    println!();
}


