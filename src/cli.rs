use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortBy {
    Size,
    Name,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

impl SortOrder {
    pub fn arrow(self) -> &'static str {
        match self {
            SortOrder::Asc => "↑",
            SortOrder::Desc => "↓",
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Tree,
    Bars,
    Treemap,
}

#[derive(Parser, Debug)]
#[command(
    name = "diskvis",
    about = "Premium terminal disk usage visualizer",
    version
)]
pub struct Cli {
    /// Directory to analyze
    #[arg(default_value = ".")]
    pub path: PathBuf,

    /// Maximum tree depth to display
    #[arg(short = 'd', long = "depth")]
    pub depth: Option<usize>,

    /// Hide entries below N bytes
    #[arg(short = 'm', long = "min-size", default_value_t = 0)]
    pub min_size: u64,

    /// Sort order
    #[arg(short = 's', long = "sort", value_enum)]
    pub sort: Option<SortBy>,

    /// Visualization mode
    #[arg(short = 'M', long = "mode", value_enum)]
    pub mode: Option<Mode>,

    /// Glob patterns to exclude (can be passed multiple times)
    #[arg(short = 'e', long = "exclude")]
    pub exclude: Vec<String>,

    /// Output full tree as JSON to stdout instead of rendering
    #[arg(long = "json")]
    pub json: bool,

    /// Disable colors
    #[arg(long = "no-color")]
    pub no_color: bool,

    /// Show individual files in tree mode
    #[arg(short = 'f', long = "show-files")]
    pub show_files: bool,

    /// Show last modified date next to each entry
    #[arg(long = "modified")]
    pub modified: bool,

    /// Print warnings and non-fatal errors to stderr
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,
}
