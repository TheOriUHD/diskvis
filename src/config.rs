use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::cli::{Mode, SortBy, SortOrder};
use crate::walker;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Theme {
    Default,
    HighContrast,
    Monochrome,
}

impl Default for Theme {
    fn default() -> Self {
        Theme::Default
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewOptions {
    pub show_hidden: bool,
    pub show_empty: bool,
    pub show_percent: bool,
    pub show_modified: bool,
}

impl Default for ViewOptions {
    fn default() -> Self {
        ViewOptions {
            show_hidden: true,
            show_empty: false,
            show_percent: true,
            show_modified: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: Theme,
    pub depth: usize,
    pub show_files: bool,
    pub show_modified: bool,
    pub excludes: Vec<String>,
    pub sort: SortBy,
    pub sort_order: SortOrder,
    pub mode: Mode,
    /// Per-exclude pattern enable flag (parallel to `excludes`).
    pub excludes_enabled: Vec<bool>,
    #[serde(default)]
    pub view: ViewOptions,
}

impl Default for Config {
    fn default() -> Self {
        let excludes = default_excludes();
        let n = excludes.len();
        Config {
            theme: Theme::Default,
            depth: 2,
            show_files: false,
            show_modified: false,
            excludes,
            sort_order: SortOrder::Desc,
            sort: SortBy::Size,
            mode: Mode::Tree,
            excludes_enabled: vec![true; n],
            view: ViewOptions::default(),
        }
    }
}

pub fn default_excludes() -> Vec<String> {
    let mut v = vec![
        "target".to_string(),
        ".git".to_string(),
        "node_modules".to_string(),
        ".DS_Store".to_string(),
    ];
    for p in walker::vfs_excludes() {
        v.push(p.to_string());
    }
    v
}

pub fn config_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(home.join(".config").join("diskvis").join("config.toml"))
}

impl Config {
    pub fn load() -> Self {
        if let Some(path) = config_path() {
            if let Ok(text) = fs::read_to_string(&path) {
                match toml::from_str::<Config>(&text) {
                    Ok(mut cfg) => {
                        // Heal mismatched lengths.
                        if cfg.excludes_enabled.len() != cfg.excludes.len() {
                            cfg.excludes_enabled = vec![true; cfg.excludes.len()];
                        }
                        // Migrate: ensure the dangerous virtual-FS excludes are
                        // present in older configs.
                        for required in default_excludes() {
                            if required.starts_with('/')
                                && !cfg.excludes.iter().any(|e| e == &required)
                            {
                                cfg.excludes.push(required);
                                cfg.excludes_enabled.push(true);
                            }
                        }
                        return cfg;
                    }
                    Err(e) => {
                        eprintln!("warning: failed to parse config: {}", e);
                    }
                }
            }
        }
        Config::default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        if let Some(path) = config_path() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let text = toml::to_string_pretty(self)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
            fs::write(path, text)?;
        }
        Ok(())
    }

    /// Return effective excludes (only enabled ones).
    pub fn active_excludes(&self) -> Vec<String> {
        self.excludes
            .iter()
            .zip(self.excludes_enabled.iter())
            .filter_map(|(p, en)| if *en { Some(p.clone()) } else { None })
            .collect()
    }
}
