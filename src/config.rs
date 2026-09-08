use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SortMode {
    #[default]
    DateDesc, // 更新日時: 新しい順 (デフォルト)
    DateAsc,  // 更新日時: 古い順
    NameAsc,  // ファイル名: 昇順 (A-Z / あ-ん)
    NameDesc, // ファイル名: 降順 (Z-A / ん-あ)
    PathAsc,  // パス名: 昇順
    PathDesc, // パス名: 降順
}

/// 結果一覧に出す種別のフィルタ。ツールバーのトグルで切り替える。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum KindFilter {
    #[default]
    Both, // ファイル＋フォルダ
    FilesOnly, // ファイルのみ
    DirsOnly,  // フォルダのみ
}

impl KindFilter {
    pub fn accepts(&self, is_dir: bool) -> bool {
        match self {
            KindFilter::Both => true,
            KindFilter::FilesOnly => !is_dir,
            KindFilter::DirsOnly => is_dir,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bookmark {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub last_target_dir: String,
    pub max_results: usize,
    pub favorites: Vec<Bookmark>,
    pub recent_folders: Vec<String>,
    pub show_sidebar: bool,
    #[serde(default)]
    pub sort_mode: SortMode,
    #[serde(default)]
    pub kind_filter: KindFilter,
    /// 左ペイン（ファイル名＋更新日時）の幅。境界ドラッグで変わる。
    #[serde(default = "default_name_pane_width")]
    pub name_pane_width: f32,
}

fn default_name_pane_width() -> f32 {
    420.0
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            last_target_dir: String::new(),
            max_results: 500,
            favorites: Vec::new(),
            recent_folders: Vec::new(),
            show_sidebar: true,
            sort_mode: SortMode::DateDesc,
            kind_filter: KindFilter::Both,
            name_pane_width: default_name_pane_width(),
        }
    }
}

impl AppConfig {
    fn get_config_path() -> Option<PathBuf> {
        if let Some(proj_dirs) = ProjectDirs::from("com", "Antigravity", "FileSearchApp") {
            let config_dir = proj_dirs.config_dir();
            let _ = fs::create_dir_all(config_dir);
            Some(config_dir.join("config.json"))
        } else {
            Some(PathBuf::from("config.json"))
        }
    }

    pub fn load() -> Self {
        if let Some(path) = Self::get_config_path() {
            if path.exists() {
                if let Ok(content) = fs::read_to_string(path) {
                    if let Ok(config) = serde_json::from_str(&content) {
                        return config;
                    }
                }
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        if let Some(path) = Self::get_config_path() {
            if let Ok(json) = serde_json::to_string_pretty(self) {
                let _ = fs::write(path, json);
            }
        }
    }

    pub fn add_recent_folder(&mut self, path: &str) {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            return;
        }
        self.recent_folders.retain(|p| p != trimmed);
        self.recent_folders.insert(0, trimmed.to_string());
        if self.recent_folders.len() > 15 {
            self.recent_folders.truncate(15);
        }
    }

    pub fn add_favorite(&mut self, name: Option<String>, path: &str) {
        let trimmed_path = path.trim();
        if trimmed_path.is_empty() {
            return;
        }

        let default_name = Path::new(trimmed_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| trimmed_path.to_string());

        let final_name = name
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(default_name);

        self.favorites.retain(|b| b.path != trimmed_path);
        self.favorites.push(Bookmark {
            name: final_name,
            path: trimmed_path.to_string(),
        });
    }

    pub fn remove_favorite(&mut self, index: usize) {
        if index < self.favorites.len() {
            self.favorites.remove(index);
        }
    }

    pub fn clear_recents(&mut self) {
        self.recent_folders.clear();
    }
}
