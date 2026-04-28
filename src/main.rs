mod cli;
mod config;
mod display;
mod editor;
mod tui;
mod walker;

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};

use crate::cli::Cli;
use crate::config::Config;
use crate::display::RenderOptions;
use crate::walker::WalkOptions;

fn main() {
    human_panic::setup_panic!();
    let cli = Cli::parse();

    if cli.no_color {
        colored::control::set_override(false);
        std::env::set_var("NO_COLOR", "1");
    }

    if !cli.path.exists() {
        if should_log_stderr(cli.verbose) {
            eprintln!("error: path does not exist: {}", cli.path.display());
        }
        process::exit(1);
    }

    let mut cfg = Config::load();
    if let Some(d) = cli.depth {
        cfg.depth = d;
    }
    if let Some(s) = cli.sort {
        cfg.sort = s;
    }
    if let Some(m) = cli.mode {
        cfg.mode = m;
    }
    if cli.show_files {
        cfg.show_files = true;
    }
    if cli.modified {
        cfg.show_modified = true;
    }
    if !cli.exclude.is_empty() {
        for pat in cli.exclude {
            if !cfg.excludes.iter().any(|e| e == &pat) {
                cfg.excludes.push(pat);
                cfg.excludes_enabled.push(true);
            }
        }
    }

    let path = match cli.path.canonicalize() {
        Ok(p) => p,
        Err(_) => cli.path.clone(),
    };

    let active_excludes = cfg.active_excludes();

    let interactive = !cli.json && std::io::stdout().is_terminal() && !cli.no_color;

    // Scanning progress
    let pb = if interactive || std::io::stderr().is_terminal() {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::with_template("{spinner:.cyan} scanning {wide_msg}")
                .unwrap_or(ProgressStyle::default_spinner()),
        );
        pb.enable_steady_tick(Duration::from_millis(80));
        Some(pb)
    } else {
        None
    };

    let pb_for_cb = pb.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let on_progress = move |p: &std::path::Path| {
        if let Some(pb) = pb_for_cb.as_ref() {
            pb.set_message(p.display().to_string());
        }
        let _ = stop;
    };

    let opts = WalkOptions {
        excludes: &active_excludes,
        on_progress: Some(&on_progress),
    };

    let scan = match walker::build_tree(&path, &opts) {
        Ok(n) => n,
        Err(e) => {
            if let Some(pb) = pb.as_ref() {
                pb.finish_and_clear();
            }
            if should_log_stderr(cli.verbose) {
                eprintln!("error: {}", e);
            }
            process::exit(1);
        }
    };
    let root = scan.root;
    let warnings = scan.warnings;

    if let Some(pb) = pb.as_ref() {
        pb.finish_and_clear();
    }

    if cli.json {
        match serde_json::to_string_pretty(&root) {
            Ok(s) => {
                let stdout = std::io::stdout();
                let mut h = stdout.lock();
                let _ = writeln!(h, "{}", s);
            }
            Err(e) => {
                if should_log_stderr(cli.verbose) {
                    eprintln!("error: failed to serialize: {}", e);
                }
                process::exit(1);
            }
        }
        return;
    }

    if interactive {
        let app = tui::App::new(root, path, cfg, cli.min_size, warnings);
        if let Err(e) = tui::run(app) {
            if should_log_stderr(cli.verbose) {
                eprintln!("error: {}", e);
            }
            process::exit(1);
        }
    } else {
        // Plain (piped or no-color) text rendering.
        let width = term_width();
        let opts = RenderOptions {
            theme: cfg.theme,
            depth: cfg.depth,
            min_size: cli.min_size,
            sort: cfg.sort,
            sort_order: cfg.sort_order,
            show_files: cfg.show_files,
            show_modified: cfg.show_modified,
            width,
            height: 24,
            hidden_summary: display::compute_hidden_summary(&root, cfg.view.show_hidden),
        };
        let rows: Vec<display::Row> = match cfg.mode {
            cli::Mode::Tree => display::tree::render(&root, &opts),
            cli::Mode::Bars => display::bars::render(&root, &opts),
            cli::Mode::Treemap => display::treemap::render(&root, &opts),
            cli::Mode::Flat => display::flat::render(&root, &opts),
        };
        display::print_rows(&rows, !cli.no_color);
        println!();
        println!(
            "Total: {} ({} bytes)  files: {}  dirs: {}{}",
            display::human_size(root.size),
            root.size,
            root.file_count(),
            root.dir_count(),
            if warnings.is_empty() {
                String::new()
            } else {
                format!("  ⚠ {} skipped", warnings.len())
            },
        );
        // In plain mode, print warnings to stderr (TUI is not active).
        if should_log_stderr(cli.verbose) {
            for w in &warnings {
                eprintln!("warning: {}", w);
            }
        }
    }
}

/// Whether non-fatal warnings/errors should be written to stderr.
fn should_log_stderr(verbose: bool) -> bool {
    let _ = verbose;
    true
}

fn term_width() -> u16 {
    if let Some((w, _)) = ::crossterm::terminal::size().ok() {
        w
    } else {
        80
    }
}

#[allow(dead_code)]
fn _unused_pathbuf_ref() -> PathBuf {
    PathBuf::new()
}
