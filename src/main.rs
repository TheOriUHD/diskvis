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
    // Slight UI priority boost. Best-effort: requires CAP_SYS_NICE for
    // negative values; a failure simply leaves us at the default niceness.
    #[cfg(unix)]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, -5);
    }
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

    // Run the scan. In interactive mode we drive a centered splash if the
    // scan takes longer than a brief threshold; otherwise fall back to a
    // simple stderr spinner.
    let scan = if interactive {
        match scan_with_splash(&path, &active_excludes, Some(cfg.depth)) {
            Ok(s) => s,
            Err(e) => {
                if should_log_stderr(cli.verbose) {
                    eprintln!("error: {}", e);
                }
                process::exit(1);
            }
        }
    } else {
        let pb = if std::io::stderr().is_terminal() {
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
            max_depth: Some(cfg.depth),
        };
        let result = walker::build_tree(&path, &opts);
        if let Some(pb) = pb.as_ref() {
            pb.finish_and_clear();
        }
        match result {
            Ok(n) => n,
            Err(e) => {
                if should_log_stderr(cli.verbose) {
                    eprintln!("error: {}", e);
                }
                process::exit(1);
            }
        }
    };
    let root = scan.root;
    let warnings = scan.warnings;

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

/// Run the scan in a background thread, showing a centered splash screen if
/// the scan takes more than ~300ms. The splash is rendered with raw
/// crossterm (no alt screen) so the TUI's own enter-alt-screen still works
/// cleanly afterwards.
fn scan_with_splash(
    path: &std::path::Path,
    excludes: &[String],
    max_depth: Option<usize>,
) -> std::io::Result<walker::ScanResult> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::thread;

    let current = Arc::new(Mutex::new(path.to_path_buf()));
    let count = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));

    let scan_path = path.to_path_buf();
    let excludes_owned: Vec<String> = excludes.to_vec();
    let cp_thread = current.clone();
    let cnt_thread = count.clone();
    let done_thread = done.clone();

    let handle = thread::spawn(move || -> std::io::Result<walker::ScanResult> {
        let cb_cp = cp_thread.clone();
        let cb_cnt = cnt_thread.clone();
        let on_progress = move |p: &std::path::Path| {
            if let Ok(mut g) = cb_cp.lock() {
                *g = p.to_path_buf();
            }
            cb_cnt.fetch_add(1, Ordering::Relaxed);
        };
        let opts = WalkOptions {
            excludes: &excludes_owned,
            on_progress: Some(&on_progress),
            max_depth,
        };
        let r = walker::build_tree(&scan_path, &opts);
        done_thread.store(true, Ordering::Relaxed);
        r
    });

    // Wait briefly without rendering — fast scans should not flash the splash.
    let start = std::time::Instant::now();
    while start.elapsed() < Duration::from_millis(300) {
        if done.load(Ordering::Relaxed) {
            break;
        }
        thread::sleep(Duration::from_millis(15));
    }

    if !done.load(Ordering::Relaxed) {
        render_splash_loop(path, &current, &count, &done);
    }

    match handle.join() {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "scanner thread panicked",
        )),
    }
}

fn render_splash_loop(
    root: &std::path::Path,
    current: &std::sync::Mutex<std::path::PathBuf>,
    count: &std::sync::atomic::AtomicUsize,
    done: &std::sync::atomic::AtomicBool,
) {
    use std::sync::atomic::Ordering;
    use std::thread;
    use crossterm::cursor;
    use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
    use crossterm::terminal::{Clear, ClearType};
    use crossterm::{queue, execute};
    use std::io::Write;

    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, Clear(ClearType::All), cursor::Hide);

    let mut tick: u64 = 0;
    while !done.load(Ordering::Relaxed) {
        let (cols, rows) = ::crossterm::terminal::size().unwrap_or((80, 24));
        let box_w: u16 = 50;
        let box_h: u16 = 9;
        if cols >= box_w + 2 && rows >= box_h + 2 {
            let x = (cols.saturating_sub(box_w)) / 2;
            let y = (rows.saturating_sub(box_h)) / 2;
            let inner_w = box_w as usize - 2;

            let cur_full = current
                .lock()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| root.display().to_string());
            let label_prefix = " Scanning ";
            let avail_for_path = inner_w.saturating_sub(label_prefix.len() + 1);
            let cur_short = if cur_full.len() > avail_for_path {
                let take = avail_for_path.saturating_sub(3);
                if take == 0 {
                    "...".to_string()
                } else {
                    format!("...{}", &cur_full[cur_full.len() - take..])
                }
            } else {
                cur_full
            };

            // Bar must fit between the box border, padding, and the trailing
            // "  NNNNN dirs" counter. Layout for the bar row is:
            //   `│ ` + bar(bar_w) + `  ` + 5-char counter + ` dirs` + ` │`
            // Total inner width consumed: bar_w + 1 + 2 + 5 + 5 + 1 = bar_w + 14.
            // Cap bar_w to leave room and never exceed inner_w - 4.
            let counter_overhead = 1 + 2 + 5 + 5 + 1; // padding + counter
            let bar_max = inner_w.saturating_sub(4);
            let bar_w = inner_w
                .saturating_sub(counter_overhead)
                .min(bar_max)
                .max(1);
            let span = (bar_w as u64) * 2;
            let pos_raw = (tick * 2) % span.max(1);
            let head = if pos_raw < bar_w as u64 {
                pos_raw as usize
            } else {
                (span - pos_raw) as usize
            };
            let mut bar = String::with_capacity(bar_w);
            for i in 0..bar_w {
                let d = if i >= head { i - head } else { head - i };
                if d <= 3 {
                    bar.push('█');
                } else {
                    bar.push('░');
                }
            }
            let dirs = count.load(Ordering::Relaxed);
            let version = env!("CARGO_PKG_VERSION");

            let lines: [String; 9] = [
                format!("╭{}╮", "─".repeat(inner_w)),
                format!("│{:^width$}│", "", width = inner_w),
                format!("│{:^width$}│", "d i s k v i s", width = inner_w),
                format!("│{:^width$}│", format!("v{}", version), width = inner_w),
                format!("│{:^width$}│", "", width = inner_w),
                format!(
                    "│{:width$}│",
                    format!("{}{}", label_prefix, cur_short),
                    width = inner_w,
                ),
                format!(
                    "│ {:bw$}  {:>5} dirs │",
                    bar,
                    dirs,
                    bw = bar_w,
                ),
                format!("│{:^width$}│", "", width = inner_w),
                format!("╰{}╯", "─".repeat(inner_w)),
            ];

            let _ = queue!(stdout, Clear(ClearType::All));
            for (i, line) in lines.iter().enumerate() {
                let _ = queue!(
                    stdout,
                    cursor::MoveTo(x, y + i as u16),
                    SetForegroundColor(Color::Cyan),
                    Print(line),
                    ResetColor,
                );
            }
            let _ = stdout.flush();
        }
        thread::sleep(Duration::from_millis(80));
        tick = tick.wrapping_add(1);
    }

    let _ = execute!(stdout, Clear(ClearType::All), cursor::Show);
}

#[allow(dead_code)]
fn _unused_pathbuf_ref() -> PathBuf {
    PathBuf::new()
}
