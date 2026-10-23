//! 検索タブ1枚分の状態。検索対象フォルダ・クエリ・結果・選択状態など、
//! タブごとに独立させたいものはすべてここに集約する。
//! 「再検索を減らす」ため、タブを切り替えても cached_entries/filtered_indices は
//! 保持されたままになり、再スキャン・再検索は発生しない。

use crate::config::{AppConfig, KindFilter, SortMode};
use crate::entry::FileEntry;
use crate::query::SearchQuery;
use crate::scanner::{new_scan_thread_pool, start_scan, ScanControl, ScanMessage};
use crate::search_worker::SearchWorker;
use eframe::egui;
use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCAN_RESULTS_UPDATE_INTERVAL: Duration = Duration::from_millis(150);
const LARGE_SCAN_RESULTS_UPDATE_INTERVAL: Duration = Duration::from_millis(300);
const HUGE_SCAN_RESULTS_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
/// Enter キーで一度に開ける件数の上限
pub const OPEN_AT_ONCE_LIMIT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStatus {
    Idle,     // 待機/完了
    Scanning, // 走査中
    Paused,   // 一時停止中
    Warning,  // 完了（一部読込失敗）
    Failed,   // 走査失敗
}

/// タブ側で起きた、アプリ（設定の保存など）に伝えるべきイベント。
pub enum TabEvent {
    /// パス検証完了。最終的な検索対象フォルダ（履歴・直近フォルダに積む対象）。
    Validated(String),
}

pub struct TabState {
    // タブを識別するための連番（egui の push_id 用。タブを閉じても再利用しない）。
    pub id: u64,

    // 検索対象フォルダ
    pub target_dir: String,

    // 検索クエリとソート・種別フィルタ（タブごとに独立）
    pub query_text: String,
    pub parsed_query: SearchQuery,
    pub sort_mode: SortMode,
    pub kind_filter: KindFilter,

    // キャッシュデータ（全件）
    pub cached_entries: Vec<Arc<FileEntry>>,
    // 検索結果インデックス（クエリ/ソート変更時・データ到着時のみ計算）
    pub filtered_indices: Vec<usize>,
    is_dirty: bool,
    force_results_update: bool,
    last_results_update_at: Instant,
    search_worker: SearchWorker,
    next_search_request_id: u64,
    latest_search_request_id: u64,

    // スキャン状態と制御
    pub scan_status: ScanStatus,
    pub status_message: String,
    pub last_scan_duration_ms: Option<u128>,

    // スレッド通信＆一時停止/再開/キャンセル制御
    tx: Sender<ScanMessage>,
    rx: Receiver<ScanMessage>,
    scan_control: Option<ScanControl>,
    // このタブ専用の走査用スレッドプール。他タブと共有しないことで、
    // 複数タブを同時スキャンしても互いのワーカーを奪い合わない。
    scan_pool: Arc<rayon::ThreadPool>,

    // 選択状態。selected_path が実際の選択集合、selected_path は
    // Shift+クリックの範囲選択の起点（アンカー）を兼ねた「最後に触った行」。
    pub selected_path: Option<String>,
    pub selected_paths: HashSet<String>,

    pub has_focused_search: bool,
}

impl TabState {
    pub fn new(
        ctx: egui::Context,
        id: u64,
        target_dir: String,
        kind_filter: KindFilter,
        sort_mode: SortMode,
    ) -> Self {
        let (tx, rx) = channel();
        let search_worker = SearchWorker::new(ctx);

        let mut tab = Self {
            id,
            target_dir: target_dir.clone(),
            query_text: String::new(),
            parsed_query: {
                let mut query = SearchQuery::parse("");
                query.kind = kind_filter;
                query
            },
            sort_mode,
            kind_filter,
            cached_entries: Vec::new(),
            filtered_indices: Vec::new(),
            is_dirty: false,
            force_results_update: false,
            last_results_update_at: Instant::now(),
            search_worker,
            next_search_request_id: 0,
            latest_search_request_id: 0,
            scan_status: ScanStatus::Idle,
            status_message: "準備完了".to_string(),
            last_scan_duration_ms: None,
            tx,
            rx,
            scan_control: None,
            scan_pool: new_scan_thread_pool(),
            selected_path: None,
            selected_paths: HashSet::new(),
            has_focused_search: false,
        };

        if !target_dir.trim().is_empty() {
            tab.trigger_scan();
        }

        tab
    }

    /// タブ見出しに出す短い表示名。
    pub fn title(&self) -> String {
        let trimmed = self.target_dir.trim();
        if trimmed.is_empty() {
            return "新規タブ".to_string();
        }
        Path::new(trimmed)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| trimmed.to_string())
    }

    /// 最初から再スキャン（新規開始）
    pub fn trigger_scan(&mut self) {
        let dir = self.target_dir.trim().to_string();
        if dir.is_empty() {
            self.status_message = "フォルダパスを指定してください。".to_string();
            return;
        }

        // 既存のスキャンを完全キャンセル
        if let Some(control) = &self.scan_control {
            control.cancel();
        }

        let (tx, rx) = channel();
        self.tx = tx;
        self.rx = rx;
        let control = ScanControl::new();
        self.scan_control = Some(control.clone());

        self.scan_status = ScanStatus::Scanning;
        self.last_scan_duration_ms = None;
        self.cached_entries.clear();
        self.filtered_indices.clear();
        self.selected_path = None;
        self.selected_paths.clear();
        self.search_worker.reset();
        self.is_dirty = true;
        self.force_results_update = true;
        self.status_message = format!("スキャン中... ({})", dir);

        start_scan(dir, self.tx.clone(), control, Arc::clone(&self.scan_pool));
    }

    /// 走査の一時停止（Pause）
    pub fn pause_scan(&mut self) -> Option<String> {
        if let Some(control) = &self.scan_control {
            control.pause();
            self.scan_status = ScanStatus::Paused;
            self.status_message =
                format!("⏸ 一時停止中 ({} 件読み込み済)", self.cached_entries.len());
            return Some("スキャンを一時停止しました（検索は可能です）".to_string());
        }
        None
    }

    /// 走査の再開（Resume）
    pub fn resume_scan(&mut self) -> Option<String> {
        if let Some(control) = &self.scan_control {
            control.resume();
            self.scan_status = ScanStatus::Scanning;
            self.status_message = format!(
                "スキャン再開中... ({} 件読み込み済)",
                self.cached_entries.len()
            );
            return Some("スキャンを再開しました".to_string());
        }
        None
    }

    pub fn check_background_messages(&mut self) -> Vec<TabEvent> {
        let mut events = Vec::new();
        let mut received_new_items = false;

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                ScanMessage::Validated(path) => {
                    events.push(TabEvent::Validated(path));
                }
                ScanMessage::Batch(batch) => {
                    self.search_worker.add_batch(batch.clone());
                    self.cached_entries.extend(batch);
                    received_new_items = true;
                    if self.scan_status == ScanStatus::Scanning {
                        self.status_message =
                            format!("スキャン中... ({} 件読み込み済)", self.cached_entries.len());
                    }
                }
                ScanMessage::Complete {
                    total_count,
                    duration_ms,
                    warning_count,
                    first_warning,
                } => {
                    self.scan_status = if warning_count == 0 {
                        ScanStatus::Idle
                    } else {
                        ScanStatus::Warning
                    };
                    self.last_scan_duration_ms = Some(duration_ms);
                    self.status_message = if warning_count == 0 {
                        format!(
                            "スキャン完了: 全 {} 件 (所要時間: {} ms)",
                            total_count, duration_ms
                        )
                    } else {
                        format!(
                            "スキャン完了（警告あり）: {} 件 / 読込失敗 {} 件{}",
                            total_count,
                            warning_count,
                            first_warning
                                .map(|warning| format!(" - {}", warning))
                                .unwrap_or_default()
                        )
                    };
                    self.is_dirty = true;
                    self.force_results_update = true;
                }
                ScanMessage::Error(err) => {
                    self.scan_status = ScanStatus::Failed;
                    self.last_scan_duration_ms = None;
                    self.status_message = format!("エラー: {}", err);
                }
            }
        }

        if received_new_items {
            self.is_dirty = true;
        }

        events
    }

    pub fn update_search_results_if_needed(&mut self, max_results: usize) {
        if !self.is_dirty {
            return;
        }

        // 走査バッチのたびに全件を検索・ソートすると、走査スレッドとCPUを奪い合う。
        // ユーザー操作は即時反映し、走査による更新だけを短い間隔でまとめる。
        let update_interval = match self.cached_entries.len() {
            0..20_000 => SCAN_RESULTS_UPDATE_INTERVAL,
            20_000..40_000 => LARGE_SCAN_RESULTS_UPDATE_INTERVAL,
            _ => HUGE_SCAN_RESULTS_UPDATE_INTERVAL,
        };
        let interval_elapsed = self.last_results_update_at.elapsed() >= update_interval;
        if self.scan_status == ScanStatus::Scanning
            && !self.force_results_update
            && !interval_elapsed
        {
            return;
        }

        self.next_search_request_id = self.next_search_request_id.wrapping_add(1);
        self.latest_search_request_id = self.next_search_request_id;
        self.search_worker.search(
            self.latest_search_request_id,
            self.parsed_query.clone(),
            self.sort_mode,
            max_results,
        );
        self.is_dirty = false;
        self.force_results_update = false;
        self.last_results_update_at = Instant::now();
    }

    pub fn check_search_results(&mut self) {
        while let Ok(result) = self.search_worker.try_recv() {
            if result.request_id == self.latest_search_request_id {
                self.filtered_indices = result.indices;
            }
        }
    }

    pub fn set_query_text(&mut self, text: String) {
        self.query_text = text;
        self.parsed_query = SearchQuery::parse(&self.query_text);
        self.parsed_query.kind = self.kind_filter;
        self.is_dirty = true;
        self.force_results_update = true;
    }

    pub fn set_kind_filter(&mut self, kind: KindFilter) {
        if self.kind_filter == kind {
            return;
        }
        self.kind_filter = kind;
        self.parsed_query.kind = kind;
        self.is_dirty = true;
        self.force_results_update = true;
    }

    pub fn set_sort_mode(&mut self, mode: SortMode) {
        self.sort_mode = mode;
        self.is_dirty = true;
        self.force_results_update = true;
    }

    /// クリック修飾キーに応じて選択集合を更新する。
    /// Ctrl で個別にトグル、Shift でアンカーからの範囲選択。
    pub fn apply_selection(&mut self, path: String, row: usize, ctrl: bool, shift: bool) {
        if shift {
            if let Some(anchor_row) = self
                .selected_path
                .clone()
                .and_then(|anchor| self.row_of_path(&anchor))
            {
                let (from, to) = if anchor_row <= row {
                    (anchor_row, row)
                } else {
                    (row, anchor_row)
                };
                if !ctrl {
                    self.selected_paths.clear();
                }
                for target_row in from..=to {
                    if let Some(entry) = self
                        .filtered_indices
                        .get(target_row)
                        .and_then(|&index| self.cached_entries.get(index))
                    {
                        self.selected_paths.insert(entry.path.clone());
                    }
                }
                // 連続して範囲を伸ばせるため、アンカーは動かさない。
                return;
            }
        }

        if ctrl {
            if !self.selected_paths.remove(&path) {
                self.selected_paths.insert(path.clone());
            }
        } else {
            self.selected_paths.clear();
            self.selected_paths.insert(path.clone());
        }
        self.selected_path = Some(path);
    }

    /// 現在の表示順における行番号を返す（検索やソートで順番が変わるため都度引く）
    fn row_of_path(&self, path: &str) -> Option<usize> {
        self.filtered_indices.iter().position(|&index| {
            self.cached_entries
                .get(index)
                .is_some_and(|entry| entry.path == path)
        })
    }

    /// 選択中の項目を既定のアプリで開く。Enter キーからの実行に使う。
    /// 戻り値はトースト表示用メッセージ（必要な場合のみ）。
    pub fn open_selected(&mut self) -> Option<String> {
        // 表示順を保った選択一覧。1件も選択が無いときは何もしない。
        let targets: Vec<String> = self
            .filtered_indices
            .iter()
            .filter_map(|&index| self.cached_entries.get(index))
            .filter(|entry| self.selected_paths.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect();

        if targets.is_empty() {
            return None;
        }
        // 大量に選択したまま Enter を押したときに、アプリが何十個も
        // 立ち上がるのを防ぐ。
        if targets.len() > OPEN_AT_ONCE_LIMIT {
            return Some(format!(
                "{} 件選択されています（Enter で一度に開けるのは {} 件までです）",
                targets.len(),
                OPEN_AT_ONCE_LIMIT
            ));
        }

        for path in &targets {
            let _ = open::that(path);
        }
        if targets.len() > 1 {
            return Some(format!("{} 件を開きました", targets.len()));
        }
        None
    }

    pub fn is_favorite(&self, config: &AppConfig) -> bool {
        let trimmed = self.target_dir.trim();
        config.favorites.iter().any(|b| b.path == trimmed)
    }
}
