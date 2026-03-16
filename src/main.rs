mod diff;
mod tui;

use anyhow::{Context, Result};
use clap::Parser;
use diff::{ChangeKind, DiffResult, compute_diff};
use std::path::Path;

#[derive(Parser, Debug)]
#[command(
    name = "mochidetect",
    version = "0.1.0",
    about = "Smart diff tool for comparing two project versions or directories",
    long_about = "\
🍡 mochidetect — smart version diff\n\
\n\
Compare two directories, project versions, or files and see exactly what changed.\n\
Launches an interactive TUI by default. Use --plain for plain terminal output.\n\
\n\
Examples:\n\
  mochidetect ./v1 ./v2\n\
  mochidetect old_project/ new_project/ --plain\n\
  mochidetect file_a.py file_b.py"
)]
struct Cli {
    /// First path (old version)
    old: String,

    /// Second path (new version)
    new: String,

    /// Print plain output instead of TUI
    #[arg(short, long)]
    plain: bool,

    /// Show unchanged files too
    #[arg(short = 'a', long)]
    all: bool,

    /// Filter by file extension (e.g. rs, py, js)
    #[arg(short, long)]
    ext: Option<String>,

    /// Summary only — no per-file listing
    #[arg(short, long)]
    summary: bool,
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

    let result = compute_diff(old_path, new_path)
        .with_context(|| format!("Failed to diff {} and {}", cli.old, cli.new))?;

    if cli.plain || cli.summary {
        print_plain(&result, &cli);
    } else {
        tui::run_tui(result)?;
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
        if let Some(ref ext) = cli.ext {
            if file.extension() != ext.to_lowercase() {
                continue;
            }
        }
        if !cli.all && file.kind == ChangeKind::Unchanged {
            continue;
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
