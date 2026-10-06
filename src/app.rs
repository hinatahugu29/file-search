use crate::config::{AppConfig, KindFilter, SortMode};
use crate::entry::FileEntry;
use crate::font::setup_japanese_fonts;
use crate::query::SearchQuery;
use crate::scanner::{start_scan, ScanControl, ScanMessage};
use crate::search_worker::SearchWorker;
use eframe::egui::{self, Color32, RichText, ScrollArea, Sense};
use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SCAN_RESULTS_UPDATE_INTERVAL: Duration = Duration::from_millis(150);
const LARGE_SCAN_RESULTS_UPDATE_INTERVAL: Duration = Duration::from_millis(300);
const HUGE_SCAN_RESULTS_UPDATE_INTERVAL: Duration = Duration::from_millis(500);
/// Enter キーで一度に開ける件数の上限
const OPEN_AT_ONCE_LIMIT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanStatus {
    Idle,     // 待機/完了
    Scanning, // 走査中
    Paused,   // 一時停止中
    Warning,  // 完了（一部読込失敗）
    Failed,   // 走査失敗
}

pub struct FileSearchApp {
    // フォルダパスと設定
    target_dir: String,
    config: AppConfig,

    // 検索クエリとソート
    query_text: String,
    parsed_query: SearchQuery,
    sort_mode: SortMode,

    // キャッシュデータ（全件）
    cached_entries: Vec<Arc<FileEntry>>,
    // 検索結果インデックス（クエリ/ソート変更時・データ到着時のみ計算）
    filtered_indices: Vec<usize>,
    is_dirty: bool,
    force_results_update: bool,
    last_results_update_at: Instant,
    search_worker: SearchWorker,
    next_search_request_id: u64,
    latest_search_request_id: u64,

    // スキャン状態と制御
    scan_status: ScanStatus,
    status_message: String,
    last_scan_duration_ms: Option<u128>,

    // スレッド通信＆一時停止/再開/キャンセル制御
    tx: Sender<ScanMessage>,
    rx: Receiver<ScanMessage>,
    scan_control: Option<ScanControl>,

    // UI状態
    // 次フレーム終了後に開始する OLE ドラッグの対象パス
    pending_drag: Option<Vec<String>>,

    // 選択状態。selected_paths が実際の選択集合、selected_path は
    // Shift+クリックの範囲選択の起点（アンカー）を兼ねた「最後に触った行」。
    selected_path: Option<String>,
    selected_paths: HashSet<String>,
    toast_message: Option<(String, std::time::Instant)>,
    has_focused_search: bool,
    show_sidebar: bool,
}

impl FileSearchApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        setup_japanese_fonts(&cc.egui_ctx);

        let mut visuals = egui::Visuals::dark();
        visuals.window_rounding = 8.0.into();
        visuals.panel_fill = Color32::from_rgb(24, 26, 31);
        cc.egui_ctx.set_visuals(visuals);

        let config = AppConfig::load();
        let target_dir = if !config.last_target_dir.is_empty() {
            config.last_target_dir.clone()
        } else {
            directories::UserDirs::new()
                .map(|u| {
                    u.document_dir()
                        .unwrap_or_else(|| u.home_dir())
                        .to_string_lossy()
                        .to_string()
                })
                .unwrap_or_else(|| "C:\\".to_string())
        };

        let show_sidebar = config.show_sidebar;
        let sort_mode = config.sort_mode;
        let kind_filter = config.kind_filter;
        let (tx, rx) = channel();
        let search_worker = SearchWorker::new(cc.egui_ctx.clone());

        let mut app = Self {
            target_dir: target_dir.clone(),
            config,
            query_text: String::new(),
            parsed_query: {
                let mut query = SearchQuery::parse("");
                query.kind = kind_filter;
                query
            },
            sort_mode,
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
            selected_path: None,
            pending_drag: None,
            selected_paths: HashSet::new(),
            toast_message: None,
            has_focused_search: false,
            show_sidebar,
        };

        if !target_dir.is_empty() {
            app.trigger_scan();
        }

        app
    }

    /// 最初から再スキャン（新規開始）
    fn trigger_scan(&mut self) {
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

        start_scan(dir, self.tx.clone(), control);
    }

    /// 走査の一時停止（Pause）
    fn pause_scan(&mut self) {
        if let Some(control) = &self.scan_control {
            control.pause();
            self.scan_status = ScanStatus::Paused;
            self.status_message =
                format!("⏸ 一時停止中 ({} 件読み込み済)", self.cached_entries.len());
            self.show_toast("スキャンを一時停止しました（検索は可能です）");
        }
    }

    /// 走査の再開（Resume）
    fn resume_scan(&mut self) {
        if let Some(control) = &self.scan_control {
            control.resume();
            self.scan_status = ScanStatus::Scanning;
            self.status_message = format!(
                "スキャン再開中... ({} 件読み込み済)",
                self.cached_entries.len()
            );
            self.show_toast("スキャンを再開しました");
        }
    }

    fn check_background_messages(&mut self) {
        let mut received_new_items = false;

        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                ScanMessage::Validated(path) => {
                    self.config.last_target_dir = path.clone();
                    self.config.add_recent_folder(&path);
                    self.config.save();
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
    }

    fn update_search_results_if_needed(&mut self) {
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
            self.config.max_results,
        );
        self.is_dirty = false;
        self.force_results_update = false;
        self.last_results_update_at = Instant::now();
    }

    fn check_search_results(&mut self) {
        while let Ok(result) = self.search_worker.try_recv() {
            if result.request_id == self.latest_search_request_id {
                self.filtered_indices = result.indices;
            }
        }
    }

    fn set_kind_filter(&mut self, kind: KindFilter) {
        if self.config.kind_filter == kind {
            return;
        }
        self.config.kind_filter = kind;
        self.parsed_query.kind = kind;
        self.config.save();
        self.is_dirty = true;
        self.force_results_update = true;
    }

    fn set_sort_mode(&mut self, mode: SortMode) {
        self.sort_mode = mode;
        self.config.sort_mode = mode;
        self.config.save();
        self.is_dirty = true;
        self.force_results_update = true;
    }

    fn show_toast(&mut self, text: &str) {
        self.toast_message = Some((text.to_string(), std::time::Instant::now()));
    }

    /// パスからファイル名（末尾要素）だけを取り出す。
    fn file_name_of(path: &str) -> String {
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    }

    /// ファイルパスから「そのファイルが入っているフォルダ」のパスを返す。
    /// フォルダ自身の場合はそのまま返す（コピペしてそのフォルダを開ける形）。
    fn folder_path_of(path_str: &str, is_dir: bool) -> String {
        if is_dir {
            return path_str.to_string();
        }
        Path::new(path_str)
            .parent()
            .map(|parent| parent.to_string_lossy().to_string())
            .filter(|parent| !parent.is_empty())
            .unwrap_or_else(|| path_str.to_string())
    }

    fn open_in_explorer(&self, path_str: &str) {
        #[cfg(target_os = "windows")]
        {
            let _ = Command::new("explorer")
                .args(["/select,", path_str])
                .spawn();
        }
        #[cfg(not(target_os = "windows"))]
        {
            if let Some(parent) = Path::new(path_str).parent() {
                let _ = open::that(parent);
            }
        }
    }

    /// クリック修飾キーに応じて選択集合を更新する。
    /// Ctrl で個別にトグル、Shift でアンカーからの範囲選択。
    fn apply_selection(&mut self, path: String, row: usize, ctrl: bool, shift: bool) {
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

    /// ファイル本体をクリップボードへ置く（エクスプローラーで Ctrl+C したのと同じ CF_HDROP 形式）。
    /// 貼り付け先ではファイルのコピーや添付として扱われる。
    #[cfg(target_os = "windows")]
    fn copy_files_to_clipboard(&mut self, paths: &[String]) {
        use clipboard_win::{raw, Clipboard};

        // スキャン後に移動・削除されたものは除く。
        let existing: Vec<&str> = paths
            .iter()
            .filter(|path| Path::new(path.as_str()).exists())
            .map(|path| path.as_str())
            .collect();

        if existing.is_empty() {
            self.show_toast("ファイルが見つかりません（移動または削除された可能性があります）");
            return;
        }
        let missing = paths.len() - existing.len();

        // クリップボードは他プロセスと共有のため、開けないことがある。少し粘る。
        let result =
            Clipboard::new_attempts(10).and_then(|_clipboard| raw::set_file_list(&existing));

        match result {
            Ok(()) => {
                let message = if missing > 0 {
                    format!(
                        "{} 件をコピーしました（{} 件は見つからず除外）",
                        existing.len(),
                        missing
                    )
                } else if existing.len() > 1 {
                    format!("{} 件をコピーしました！Ctrl+V で貼り付けられます", existing.len())
                } else {
                    "ファイルをコピーしました！貼り付け先でファイルとして扱われます".to_string()
                };
                self.show_toast(&message);
            }
            Err(err) => {
                self.show_toast(&format!("クリップボードへのコピーに失敗しました: {}", err))
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    fn copy_files_to_clipboard(&mut self, paths: &[String]) {
        // CF_HDROP はWindows固有のため、他OSではパスのコピーで代替する。
        self.copy_to_clipboard(&paths.join("
"));
    }

    /// 選択中の項目を既定のアプリで開く。Enter キーからの実行に使う。
    fn open_selected(&mut self) {
        // 表示順を保った選択一覧。1件も選択が無いときは何もしない。
        let targets: Vec<String> = self
            .filtered_indices
            .iter()
            .filter_map(|&index| self.cached_entries.get(index))
            .filter(|entry| self.selected_paths.contains(&entry.path))
            .map(|entry| entry.path.clone())
            .collect();

        if targets.is_empty() {
            return;
        }
        // 大量に選択したまま Enter を押したときに、アプリが何十個も
        // 立ち上がるのを防ぐ。
        if targets.len() > OPEN_AT_ONCE_LIMIT {
            self.show_toast(&format!(
                "{} 件選択されています（Enter で一度に開けるのは {} 件までです）",
                targets.len(),
                OPEN_AT_ONCE_LIMIT
            ));
            return;
        }

        for path in &targets {
            let _ = open::that(path);
        }
        if targets.len() > 1 {
            self.show_toast(&format!("{} 件を開きました", targets.len()));
        }
    }

    fn copy_to_clipboard(&mut self, text: &str) {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            if clipboard.set_text(text).is_ok() {
                self.show_toast("クリップボードにコピーしました！");
            }
        }
    }

    fn is_current_dir_favorite(&self) -> bool {
        let trimmed = self.target_dir.trim();
        self.config.favorites.iter().any(|b| b.path == trimmed)
    }
}

impl eframe::App for FileSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.check_background_messages();
        self.update_search_results_if_needed();
        self.check_search_results();

        if self.scan_status == ScanStatus::Scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        if let Some((_, created_at)) = &self.toast_message {
            if created_at.elapsed().as_secs_f32() > 2.5 {
                self.toast_message = None;
            }
        }

        // ==========================================
        // 上部パネル（操作バー）
        // ==========================================
        egui::TopBottomPanel::top("top_panel").show(ctx, |ui| {
            ui.add_space(8.0);

            // 1. 検索対象フォルダ指定行（レスポンシブ配置で見切れを完全防止）
            ui.horizontal(|ui| {
                let sidebar_btn_text = if self.show_sidebar {
                    "◀ サイドバー"
                } else {
                    "▶ サイドバー"
                };
                if ui.button(sidebar_btn_text).clicked() {
                    self.show_sidebar = !self.show_sidebar;
                    self.config.show_sidebar = self.show_sidebar;
                    self.config.save();
                }

                ui.separator();
                ui.label(RichText::new("📁 検索対象:").strong());

                // 右側のボタングループを右詰め（right_to_left）で確実に配置し、残余幅をTextEditに割り当てる
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // スキャン制御ボタン
                    match self.scan_status {
                        ScanStatus::Scanning => {
                            if ui
                                .button("🔄 最初から")
                                .on_hover_text("キャッシュをクリアして最初から再スキャンします")
                                .clicked()
                            {
                                self.trigger_scan();
                            }
                            if ui
                                .button("⏸ 一時停止")
                                .on_hover_text("走査を一時停止します（続きから再開可能）")
                                .clicked()
                            {
                                self.pause_scan();
                            }
                            ui.spinner();
                        }
                        ScanStatus::Paused => {
                            if ui
                                .button("🔄 最初から")
                                .on_hover_text("キャッシュをクリアして最初から再スキャンします")
                                .clicked()
                            {
                                self.trigger_scan();
                            }
                            if ui
                                .button(
                                    RichText::new("▶ 再開")
                                        .color(Color32::from_rgb(100, 220, 100))
                                        .strong(),
                                )
                                .on_hover_text("中断した続きから走査を再開します")
                                .clicked()
                            {
                                self.resume_scan();
                            }
                        }
                        ScanStatus::Idle | ScanStatus::Warning => {
                            if ui
                                .button("🔄 再スキャン")
                                .on_hover_text("フォルダ内を最初から再スキャンします")
                                .clicked()
                            {
                                self.trigger_scan();
                            }
                        }
                        ScanStatus::Failed => {
                            if ui.button("🔄 再試行").clicked() {
                                self.trigger_scan();
                            }
                        }
                    }

                    // お気に入りボタン
                    let is_fav = self.is_current_dir_favorite();
                    let fav_icon = if is_fav {
                        "⭐ 登録済"
                    } else {
                        "☆ お気に入り"
                    };
                    if ui
                        .button(fav_icon)
                        .on_hover_text(if is_fav {
                            "お気に入りに登録済みです"
                        } else {
                            "現在のフォルダをお気に入りに追加"
                        })
                        .clicked()
                        && !is_fav
                        && !self.target_dir.trim().is_empty()
                    {
                        self.config.add_favorite(None, &self.target_dir);
                        self.config.save();
                        self.show_toast("お気に入りに追加しました！");
                    }

                    // 参照ボタン
                    if ui.button("📂 参照...").clicked() {
                        if let Some(folder) = rfd::FileDialog::new()
                            .set_directory(&self.target_dir)
                            .pick_folder()
                        {
                            self.target_dir = folder.to_string_lossy().to_string();
                            self.trigger_scan();
                        }
                    }

                    // 残った中央のスペース全域をパス入力欄に充当
                    let dir_edit = ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut self.target_dir)
                            .hint_text("検索する親フォルダのパス（例: D:\\Projects）"),
                    );
                    if dir_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.trigger_scan();
                    }
                });
            });

            ui.add_space(6.0);

            // 2. 検索バー行
            ui.horizontal(|ui| {
                ui.label(RichText::new("🔍 検索:").strong().size(16.0));
                let search_input = ui.add_sized(
                    [ui.available_width() - 10.0, 28.0],
                    egui::TextEdit::singleline(&mut self.query_text)
                        .hint_text("キーワード（スペースでAND、| でOR、! で除外）")
                        .font(egui::TextStyle::Heading)
                        // IMEの候補選択に使うTabで次のウィジェットへ移動させない。
                        .lock_focus(true),
                );

                if !self.has_focused_search {
                    search_input.request_focus();
                    self.has_focused_search = true;
                }

                // singlelineはEnterを受けると標準動作でフォーカスを手放す。
                // IMEの確定Enterでは検索欄を使い続けられるよう、直ちに戻す。
                if search_input.lost_focus()
                    && ui.input(|input| input.key_pressed(egui::Key::Enter))
                {
                    search_input.request_focus();
                    // IME確定直後は変換対象が選択範囲として残ることがある。
                    // そのまま打つと確定した文字を消してしまうので、
                    // キャレットを末尾に畳んでおく。
                    if let Some(mut state) =
                        egui::text_edit::TextEditState::load(ui.ctx(), search_input.id)
                    {
                        let end = egui::text::CCursor::new(self.query_text.chars().count());
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(end)));
                        state.store(ui.ctx(), search_input.id);
                    }
                }

                if search_input.changed() {
                    self.parsed_query = SearchQuery::parse(&self.query_text);
                    self.parsed_query.kind = self.config.kind_filter;
                    self.is_dirty = true;
                    self.force_results_update = true;
                }
            });

            ui.add_space(8.0);
        });

        // ==========================================
        // 下部ステータスバー
        // ==========================================
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let status_color = match self.scan_status {
                    ScanStatus::Scanning => Color32::from_rgb(255, 190, 80),
                    ScanStatus::Paused => Color32::from_rgb(255, 140, 140),
                    ScanStatus::Warning => Color32::from_rgb(255, 190, 80),
                    ScanStatus::Failed => Color32::from_rgb(255, 100, 100),
                    ScanStatus::Idle => Color32::LIGHT_GRAY,
                };
                ui.label(
                    RichText::new(&self.status_message)
                        .size(12.0)
                        .color(status_color),
                );

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some((toast, _)) = &self.toast_message {
                        ui.label(
                            RichText::new(toast)
                                .color(Color32::from_rgb(100, 220, 100))
                                .strong(),
                        );
                    }
                });
            });
        });

        // ==========================================
        // 左ペイン（サイドバー：お気に入り＆検索履歴）
        // ==========================================
        let mut target_to_switch: Option<String> = None;
        let mut fav_to_remove: Option<usize> = None;
        let mut clear_recents_requested = false;

        if self.show_sidebar {
            egui::SidePanel::left("left_sidebar")
                .resizable(true)
                .default_width(240.0)
                .width_range(180.0..=400.0)
                .show(ctx, |ui| {
                    ScrollArea::vertical().show(ui, |ui| {
                        ui.add_space(8.0);

                        // --- ⭐ お気に入りフォルダ ---
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("⭐ お気に入り").strong().size(14.0).color(Color32::from_rgb(255, 215, 100)));
                        });
                        ui.separator();

                        if self.config.favorites.is_empty() {
                            ui.label(RichText::new("登録されたフォルダはありません。\n上部の「☆ お気に入り」から追加できます。").size(11.0).color(Color32::GRAY));
                        } else {
                            for (idx, bookmark) in self.config.favorites.iter().enumerate() {
                                let is_active = self.target_dir.trim() == bookmark.path.trim();

                                ui.horizontal(|ui| {
                                    let btn = ui.selectable_label(
                                        is_active,
                                        RichText::new(format!("📁 {}", bookmark.name)).strong(),
                                    );
                                    if btn.on_hover_text(&bookmark.path).clicked() {
                                        target_to_switch = Some(bookmark.path.clone());
                                    }

                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        if ui.small_button("✕").on_hover_text("お気に入りから削除").clicked() {
                                            fav_to_remove = Some(idx);
                                        }
                                    });
                                });
                            }
                        }

                        ui.add_space(16.0);

                        // --- 🕒 検索履歴 ---
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("🕒 検索履歴").strong().size(14.0).color(Color32::from_rgb(150, 200, 255)));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if !self.config.recent_folders.is_empty()
                                    && ui.small_button("🗑 クリア").clicked() {
                                    clear_recents_requested = true;
                                }
                            });
                        });
                        ui.separator();

                        if self.config.recent_folders.is_empty() {
                            ui.label(RichText::new("履歴はありません。").size(11.0).color(Color32::GRAY));
                        } else {
                            for path_str in &self.config.recent_folders {
                                let is_active = self.target_dir.trim() == path_str.trim();
                                let folder_name = Path::new(path_str)
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                                    .unwrap_or_else(|| path_str.clone());

                                ui.horizontal(|ui| {
                                    let label_text = format!("📂 {}", folder_name);
                                    let btn = ui.selectable_label(is_active, RichText::new(label_text));
                                    if btn.on_hover_text(path_str).clicked() {
                                        target_to_switch = Some(path_str.clone());
                                    }
                                });
                            }
                        }

                        ui.add_space(10.0);
                    });
                });
        }

        if let Some(path) = target_to_switch {
            self.target_dir = path;
            self.trigger_scan();
        }
        if let Some(idx) = fav_to_remove {
            self.config.remove_favorite(idx);
            self.config.save();
            self.show_toast("お気に入りを削除しました。");
        }
        if clear_recents_requested {
            self.config.clear_recents();
            self.config.save();
            self.show_toast("履歴をクリアしました。");
        }

        // ==========================================
        // メイン領域：検索結果一覧（ヘッダーソート＆ワンクリックパスコピー）
        // ==========================================
        egui::CentralPanel::default().show(ctx, |ui| {
            let hit_count = self.filtered_indices.len();
            let total_count = self.cached_entries.len();

            // 1. サマリーバー
            ui.horizontal(|ui| {
                let status_prefix = match self.scan_status {
                    ScanStatus::Scanning => "⚡ 走査中...",
                    ScanStatus::Paused => "⏸ 一時停止中",
                    ScanStatus::Warning => "⚠ 完了（警告あり）",
                    ScanStatus::Failed => "⚠ 走査エラー",
                    ScanStatus::Idle => "✓ 完了",
                };
                let status_color = match self.scan_status {
                    ScanStatus::Scanning => Color32::from_rgb(255, 180, 80),
                    ScanStatus::Paused => Color32::from_rgb(255, 140, 140),
                    ScanStatus::Warning => Color32::from_rgb(255, 190, 80),
                    ScanStatus::Failed => Color32::from_rgb(255, 100, 100),
                    ScanStatus::Idle => Color32::from_rgb(130, 170, 255),
                };

                let count_text = format!(
                    "{} ヒット件数: {} 件 / 読込済: {} 件",
                    status_prefix, hit_count, total_count
                );
                ui.label(RichText::new(count_text).strong().color(status_color));

                if self.selected_paths.len() > 1 {
                    ui.label(
                        RichText::new(format!("☑ {} 件選択中", self.selected_paths.len()))
                            .size(11.0)
                            .color(Color32::from_rgb(140, 220, 140)),
                    );
                }

                if let Some(duration) = self.last_scan_duration_ms {
                    ui.label(
                        RichText::new(format!("(走査時間: {} ms)", duration))
                            .size(11.0)
                            .color(Color32::GRAY),
                    );
                }
            });

            ui.add_space(2.0);

            // 2. テーブルヘッダー（クリックでソート切り替え／境界ドラッグで幅調整）
            // 左ペイン＝ファイル名＋更新日時、右ペイン＝パス。
            // 行もヘッダーも同じ幅で区切るので、パス列の頭が一直線に揃う。
            const DATE_WIDTH: f32 = 132.0;
            const SPLITTER_WIDTH: f32 = 7.0;
            const ROW_HEIGHT: f32 = 24.0;
            let pane_width = self.config.name_pane_width;

            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(pane_width, 22.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add_sized(
                            [24.0, 20.0],
                            egui::Label::new(RichText::new("\u{1F4CB}").size(11.0)),
                        );

                        let (name_arrow, next_name_mode) = match self.sort_mode {
                            SortMode::NameAsc => ("\u{25B2}", SortMode::NameDesc),
                            SortMode::NameDesc => ("\u{25BC}", SortMode::NameAsc),
                            _ => ("\u{2195}", SortMode::NameAsc),
                        };
                        let (date_arrow, next_date_mode) = match self.sort_mode {
                            SortMode::DateDesc => ("\u{25BC}", SortMode::DateAsc),
                            SortMode::DateAsc => ("\u{25B2}", SortMode::DateDesc),
                            _ => ("\u{2195}", SortMode::DateDesc),
                        };

                        // 更新日時は右寄せ。行側の日時列と同じ幅なので位置が揃う。
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                let date_btn = ui.add_sized(
                                    [DATE_WIDTH, 20.0],
                                    egui::Button::new(
                                        RichText::new(format!(
                                            "\u{1F4C5} 更新日時 {}",
                                            date_arrow
                                        ))
                                        .strong(),
                                    ),
                                );
                                if date_btn
                                    .on_hover_text("クリックで日付順（新しい順/古い順）を切り替え")
                                    .clicked()
                                {
                                    self.set_sort_mode(next_date_mode);
                                }

                                ui.with_layout(
                                    egui::Layout::left_to_right(egui::Align::Center),
                                    |ui| {
                                        let name_btn = ui.button(
                                            RichText::new(format!(
                                                "\u{1F4C4} ファイル名 {}",
                                                name_arrow
                                            ))
                                            .strong(),
                                        );
                                        if name_btn
                                            .on_hover_text(
                                                "クリックで名前順（昇順/降順）を切り替え",
                                            )
                                            .clicked()
                                        {
                                            self.set_sort_mode(next_name_mode);
                                        }
                                    },
                                );
                            },
                        );
                    },
                );

                // ペイン境界（ドラッグで左ペインの幅を変更）
                let (splitter_rect, splitter) = ui.allocate_exact_size(
                    egui::vec2(SPLITTER_WIDTH, 22.0),
                    Sense::click_and_drag(),
                );
                if splitter.hovered() || splitter.dragged() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
                }
                if splitter.dragged() {
                    let width = (self.config.name_pane_width + splitter.drag_delta().x)
                        .clamp(180.0, 1400.0);
                    self.config.name_pane_width = width;
                }
                if splitter.drag_stopped() || splitter.double_clicked() {
                    if splitter.double_clicked() {
                        self.config.name_pane_width = 420.0;
                    }
                    self.config.save();
                }
                let splitter_color = if splitter.hovered() || splitter.dragged() {
                    Color32::from_rgb(120, 170, 255)
                } else {
                    Color32::from_rgb(90, 95, 105)
                };
                ui.painter().vline(
                    splitter_rect.center().x,
                    splitter_rect.y_range(),
                    egui::Stroke::new(1.5_f32, splitter_color),
                );
                splitter.on_hover_text("ドラッグで幅を調整（ダブルクリックで既定に戻す）");

                let (path_arrow, next_path_mode) = match self.sort_mode {
                    SortMode::PathAsc => ("\u{25B2}", SortMode::PathDesc),
                    SortMode::PathDesc => ("\u{25BC}", SortMode::PathAsc),
                    _ => ("\u{2195}", SortMode::PathAsc),
                };
                let path_btn =
                    ui.button(RichText::new(format!("\u{1F4C2} パス {}", path_arrow)).strong());
                if path_btn
                    .on_hover_text("クリックでパス順（昇順/降順）を切り替え")
                    .clicked()
                {
                    self.set_sort_mode(next_path_mode);
                }

                // 右端：種別フィルタのトグル
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut next_kind = self.config.kind_filter;
                    // 右詰めレイアウトなので、見た目の並びに合わせて逆順に置く。
                    ui.selectable_value(
                        &mut next_kind,
                        KindFilter::DirsOnly,
                        "\u{1F4C1} フォルダのみ",
                    );
                    ui.selectable_value(
                        &mut next_kind,
                        KindFilter::FilesOnly,
                        "\u{1F4C4} ファイルのみ",
                    );
                    ui.selectable_value(&mut next_kind, KindFilter::Both, "\u{1F5C2} 両方");
                    self.set_kind_filter(next_kind);
                });
            });

            ui.separator();

            enum TableAction {
                Select {
                    path: String,
                    row: usize,
                    ctrl: bool,
                    shift: bool,
                },
                Open(String),
                OpenInExplorer(String),
                CopyToClipboard(String),
                CopyFilesToClipboard(Vec<String>),
                SearchInFolder(String),
            }

            // 行の右クリックメニュー。選択済み行なら選択分をまとめて対象にする。
            fn row_context_menu(
                ui: &mut egui::Ui,
                entry: &FileEntry,
                is_selected: bool,
                visible_selected: &[&str],
            ) -> Option<TableAction> {
                let mut action = None;
                if entry.is_dir {
                    if ui.button("\u{1F50D} このフォルダ内を検索対象に").clicked() {
                        action = Some(TableAction::SearchInFolder(entry.path.clone()));
                        ui.close_menu();
                    }
                    ui.separator();
                }
                if ui.button("\u{25B6} 開く").clicked() {
                    action = Some(TableAction::Open(entry.path.clone()));
                    ui.close_menu();
                }
                if ui.button("\u{1F4C2} エクスプローラーで表示").clicked() {
                    action = Some(TableAction::OpenInExplorer(entry.path.clone()));
                    ui.close_menu();
                }
                if ui
                    .button("\u{1F4CB} フォルダのパスをコピー")
                    .on_hover_text("ファイル名を除いた、置き場所のフォルダパスをコピーします")
                    .clicked()
                {
                    action = Some(TableAction::CopyToClipboard(
                        FileSearchApp::folder_path_of(&entry.path, entry.is_dir),
                    ));
                    ui.close_menu();
                }
                if ui.button("\u{1F4C4} フルパスをコピー").clicked() {
                    action = Some(TableAction::CopyToClipboard(entry.path.clone()));
                    ui.close_menu();
                }
                // 複数選択中なら、選んだ分のファイル名を改行区切りでまとめてコピーする。
                let names_multi = is_selected && visible_selected.len() > 1;
                let name_label = if names_multi {
                    format!("\u{1F4DD} {} 件のファイル名をコピー", visible_selected.len())
                } else {
                    "\u{1F4DD} ファイル名をコピー".to_string()
                };
                if ui
                    .button(name_label)
                    .on_hover_text(
                        "拡張子を含むファイル名（フォルダならフォルダ名）だけをコピーします",
                    )
                    .clicked()
                {
                    let text = if names_multi {
                        visible_selected
                            .iter()
                            .map(|path| FileSearchApp::file_name_of(path))
                            .collect::<Vec<_>>()
                            .join("\r\n")
                    } else {
                        entry.file_name.clone()
                    };
                    action = Some(TableAction::CopyToClipboard(text));
                    ui.close_menu();
                }

                let targets: Vec<String> = if is_selected && visible_selected.len() > 1 {
                    visible_selected.iter().map(|p| p.to_string()).collect()
                } else {
                    vec![entry.path.clone()]
                };
                let copy_file_label = if targets.len() > 1 {
                    format!("\u{1F4CE} {} 件をコピー", targets.len())
                } else if entry.is_dir {
                    "\u{1F4CE} フォルダをコピー".to_string()
                } else {
                    "\u{1F4CE} ファイルをコピー".to_string()
                };
                if ui
                    .button(copy_file_label)
                    .on_hover_text("エクスプローラーやメールに Ctrl+V で貼り付けられます")
                    .clicked()
                {
                    action = Some(TableAction::CopyFilesToClipboard(targets));
                    ui.close_menu();
                }
                action
            }

            // クリック＆ドラッグを単一レスポンスで確実に判定・処理する選択可能ラベル
            fn selectable_item_with_drag(
                ui: &mut egui::Ui,
                selected: bool,
                text: RichText,
            ) -> egui::Response {
                let button_padding = ui.spacing().button_padding;
                let total_extra = button_padding + button_padding;
                let wrap_width = (ui.available_width() - total_extra.x).max(0.0);
                let widget_text: egui::WidgetText = text.into();
                let galley = widget_text.into_galley(ui, None, wrap_width, egui::TextStyle::Button);

                let mut desired_size = total_extra + galley.size();
                desired_size.y = desired_size.y.max(ui.spacing().interact_size.y);
                let (rect, response) = ui.allocate_at_least(desired_size, Sense::click_and_drag());

                if ui.is_rect_visible(response.rect) {
                    let text_pos = ui
                        .layout()
                        .align_size_within_rect(galley.size(), rect.shrink2(button_padding))
                        .min;

                    let visuals = ui.style().interact_selectable(&response, selected);

                    if selected || response.hovered() || response.highlighted() {
                        let rect = rect.expand(visuals.expansion);
                        ui.painter().rect(
                            rect,
                            visuals.rounding,
                            visuals.weak_bg_fill,
                            visuals.bg_stroke,
                        );
                    }

                    ui.painter().galley(text_pos, galley, visuals.text_color());
                }

                response
            }

            let mut pending_action: Option<TableAction> = None;
            // ドラッグ開始はフレーム描画中に実行しない（DoDragDrop がブロッキングのため）。
            // 対象パスだけ記録し、フレーム終了後にまとめて開始する。
            let mut pending_drag: Option<Vec<String>> = None;
            let selected_paths = &self.selected_paths;
            // 表示順を保った選択一覧。まとめて操作するときの対象になる。
            let visible_selected: Vec<&str> = self
                .filtered_indices
                .iter()
                .filter_map(|&index| self.cached_entries.get(index))
                .filter(|entry| selected_paths.contains(&entry.path))
                .map(|entry| entry.path.as_str())
                .collect();

            // 3. 結果テーブル
            // 行高は固定。長いファイル名・長いパスは折り返さず省略し（ホバーで全文）、
            // show_rows による可視行だけの描画を維持する。
            ScrollArea::vertical()
                .auto_shrink([false, false])
                .show_rows(
                    ui,
                    ROW_HEIGHT,
                    self.filtered_indices.len(),
                    |ui, row_range| {
                        for i in row_range {
                            let Some(&entry_idx) = self.filtered_indices.get(i) else {
                                continue;
                            };
                            let Some(entry) = self.cached_entries.get(entry_idx) else {
                                continue;
                            };
                            let is_selected = selected_paths.contains(&entry.path);
                            let folder_path =
                                FileSearchApp::folder_path_of(&entry.path, entry.is_dir);

                            ui.push_id(i, |ui| {
                                // 選択行全体の背景を上品にハイライト
                                if is_selected {
                                    let row_rect = egui::Rect::from_min_size(
                                        ui.cursor().min,
                                        egui::vec2(ui.available_width(), ROW_HEIGHT),
                                    );
                                    ui.painter().rect_filled(
                                        row_rect,
                                        2.0,
                                        Color32::from_rgba_unmultiplied(40, 75, 120, 85),
                                    );
                                }

                                ui.horizontal(|ui| {
                                    let mut row_drag_started = false;
                                    // ===== 左ペイン：コピー / アイコン / ファイル名 / 更新日時 =====
                                    ui.allocate_ui_with_layout(
                                        egui::vec2(pane_width, 20.0),
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            if ui
                                                .small_button("\u{1F4CB}")
                                                .on_hover_text(
                                                    "フォルダのパスをコピー（ファイル名は含みません）",
                                                )
                                                .clicked()
                                            {
                                                pending_action =
                                                    Some(TableAction::CopyToClipboard(
                                                        folder_path.clone(),
                                                    ));
                                            }

                                             let icon = if entry.is_dir {
                                                "\u{1F4C1}"
                                            } else {
                                                "\u{1F4C4}"
                                            };
                                            ui.label(RichText::new(icon).size(13.0));

                                            ui.with_layout(
                                                egui::Layout::right_to_left(egui::Align::Center),
                                                |ui| {
                                                    ui.add_sized(
                                                        [DATE_WIDTH, 20.0],
                                                        egui::Label::new(
                                                            RichText::new(entry.formatted_date())
                                                                .size(11.0)
                                                                .color(Color32::from_rgb(
                                                                    190, 190, 190,
                                                                )),
                                                        ),
                                                    );

                                                    // 残り幅いっぱいをファイル名に。長い名前は省略。
                                                    ui.with_layout(
                                                        egui::Layout::left_to_right(
                                                            egui::Align::Center,
                                                        ),
                                                        |ui| {
                                                            let name_label = selectable_item_with_drag(
                                                                ui,
                                                                is_selected,
                                                                RichText::new(&entry.file_name)
                                                                    .strong()
                                                                    .color(if entry.is_dir {
                                                                        Color32::from_rgb(
                                                                            255, 215, 100,
                                                                        )
                                                                    } else {
                                                                        Color32::WHITE
                                                                    }),
                                                            );

                                                            if name_label.drag_started() {
                                                                row_drag_started = true;
                                                            }

                                                            if name_label.clicked() {
                                                                let modifiers = ui.input(
                                                                    |input| input.modifiers,
                                                                );
                                                                pending_action =
                                                                    Some(TableAction::Select {
                                                                        path: entry.path.clone(),
                                                                        row: i,
                                                                        ctrl: modifiers.command,
                                                                        shift: modifiers.shift,
                                                                    });
                                                            }
                                                            if name_label.double_clicked() {
                                                                pending_action =
                                                                    Some(TableAction::Open(
                                                                        entry.path.clone(),
                                                                    ));
                                                            }
                                                            name_label
                                                                .on_hover_text(&entry.file_name)
                                                                .context_menu(|ui| {
                                                                    if let Some(action) =
                                                                        row_context_menu(
                                                                            ui,
                                                                            entry,
                                                                            is_selected,
                                                                            &visible_selected,
                                                                        )
                                                                    {
                                                                        pending_action =
                                                                            Some(action);
                                                                    }
                                                                });
                                                        },
                                                    );
                                                },
                                            );
                                        },
                                    );

                                    // ===== 境界線 =====
                                    let (line_rect, _) = ui.allocate_exact_size(
                                        egui::vec2(SPLITTER_WIDTH, 20.0),
                                        Sense::hover(),
                                    );
                                    ui.painter().vline(
                                        line_rect.center().x,
                                        line_rect.y_range(),
                                        egui::Stroke::new(1.0_f32, Color32::from_rgb(70, 74, 82)),
                                    );

                                    // ===== 右ペイン：パス（ファイル名を除いたフォルダパス）=====
                                    let path_label = ui
                                        .add(
                                            egui::Label::new(
                                                RichText::new(&folder_path)
                                                    .size(12.0)
                                                    .color(Color32::from_rgb(165, 175, 190)),
                                            )
                                            .truncate()
                                            .sense(Sense::click_and_drag()),
                                        )
                                        .on_hover_text(&entry.path);

                                    if path_label.drag_started() {
                                        row_drag_started = true;
                                    }

                                    if row_drag_started && pending_drag.is_none() {
                                        if is_selected && !visible_selected.is_empty() {
                                            pending_drag = Some(
                                                visible_selected
                                                    .iter()
                                                    .map(|p| p.to_string())
                                                    .collect(),
                                            );
                                        } else {
                                            // 未選択行をドラッグしたときは、その行を選択状態に
                                            // 合わせてからドラッグする（見た目と対象を一致させる）。
                                            pending_drag = Some(vec![entry.path.clone()]);
                                            pending_action = Some(TableAction::Select {
                                                path: entry.path.clone(),
                                                row: i,
                                                ctrl: false,
                                                shift: false,
                                            });
                                        }
                                    }

                                    if path_label.clicked() {
                                        let modifiers = ui.input(|input| input.modifiers);
                                        pending_action = Some(TableAction::Select {
                                            path: entry.path.clone(),
                                            row: i,
                                            ctrl: modifiers.command,
                                            shift: modifiers.shift,
                                        });
                                    }
                                    if path_label.double_clicked() {
                                        pending_action =
                                            Some(TableAction::Open(entry.path.clone()));
                                    }
                                    path_label.context_menu(|ui| {
                                        if let Some(action) = row_context_menu(
                                            ui,
                                            entry,
                                            is_selected,
                                            &visible_selected,
                                        ) {
                                            pending_action = Some(action);
                                        }
                                    });
                                });
                            });
                        }
                    },
                );

            match pending_action {
                Some(TableAction::Select {
                    path,
                    row,
                    ctrl,
                    shift,
                }) => {
                    self.apply_selection(path, row, ctrl, shift);
                    // 行を選んだら検索欄のフォーカスを外す。以降の Enter が
                    // 一覧の「開く」として働くようにするため。
                    ctx.memory_mut(|memory| memory.stop_text_input());
                }
                Some(TableAction::Open(path)) => {
                    let _ = open::that(path);
                }
                Some(TableAction::OpenInExplorer(path)) => {
                    self.open_in_explorer(&path);
                }
                Some(TableAction::CopyToClipboard(path)) => {
                    self.copy_to_clipboard(&path);
                }
                Some(TableAction::CopyFilesToClipboard(paths)) => {
                    self.copy_files_to_clipboard(&paths);
                }
                Some(TableAction::SearchInFolder(path)) => {
                    self.target_dir = path;
                    self.trigger_scan();
                }
                None => {}
            }

            self.pending_drag = pending_drag;
        });

        // 入力欄にフォーカスが無いとき（＝一覧を操作しているとき）だけ、
        // Enter で選択項目を開く。IME の確定 Enter と衝突させないため、
        // 変換イベントが来ているフレームでは無視する。
        let list_enter_pressed = ctx.input(|input| {
            input.key_pressed(egui::Key::Enter)
                && !input
                    .events
                    .iter()
                    .any(|event| matches!(event, egui::Event::Ime(_)))
        });
        if list_enter_pressed && ctx.memory(|memory| memory.focused()).is_none() {
            self.open_selected();
        }

        // フレーム描画が終わってからドラッグを開始する。DoDragDrop は独自の
        // メッセージループでブロックするため、描画中に呼ぶと UI が固まりやすい。
        if let Some(paths) = self.pending_drag.take() {
            let refs: Vec<&str> = paths.iter().map(|p| p.as_str()).collect();
            crate::drag_drop::start_drag(&refs);
            // ドラッグ中のマウス操作は OLE 側が握るため、egui にはボタン解放が
            // 届かない。ポインタ状態を初期化して「押しっぱなし」状態を防ぐ。
            ctx.input_mut(|input| input.pointer = egui::PointerState::default());
            ctx.request_repaint();
        }
    }
}
