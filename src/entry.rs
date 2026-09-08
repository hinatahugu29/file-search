use chrono::{DateTime, Local};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub file_name: String,
    pub path: String,
    pub is_dir: bool,
    #[allow(dead_code)]
    pub size: u64,
    pub modified: Option<SystemTime>,
    // 検索高速化用にあらかじめ小文字化・正規化しておいたキャッシュ
    pub name_lower: String,
    pub path_lower: String,
}

impl FileEntry {
    pub fn new(
        file_name: String,
        path: String,
        is_dir: bool,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Self {
        let name_lower = file_name.to_lowercase();
        let path_lower = path.to_lowercase().replace('/', "\\");
        Self {
            file_name,
            path,
            is_dir,
            size,
            modified,
            name_lower,
            path_lower,
        }
    }

    #[allow(dead_code)]
    pub fn formatted_size(&self) -> String {
        if self.is_dir {
            return "<DIR>".to_string();
        }
        const KB: u64 = 1024;
        const MB: u64 = KB * 1024;
        const GB: u64 = MB * 1024;

        if self.size >= GB {
            format!("{:.2} GB", self.size as f64 / GB as f64)
        } else if self.size >= MB {
            format!("{:.2} MB", self.size as f64 / MB as f64)
        } else if self.size >= KB {
            format!("{:.1} KB", self.size as f64 / KB as f64)
        } else {
            format!("{} B", self.size)
        }
    }

    pub fn formatted_date(&self) -> String {
        match self.modified {
            Some(time) => {
                let dt: DateTime<Local> = time.into();
                dt.format("%Y/%m/%d %H:%M:%S").to_string()
            }
            None => "-".to_string(),
        }
    }
}
