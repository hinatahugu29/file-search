use crate::config::{AppConfig, KindFilter, SortMode};
use crate::entry::FileEntry;
use crate::font::setup_japanese_fonts;
use crate::tab::{ScanStatus, TabEvent, TabState};
use eframe::egui::{self, Color32, RichText, ScrollArea, Sense};
use std::path::Path;
use std::process::Command;

pub struct FileSearchApp {
    config: AppConfig,

    tabs: Vec<TabState>,
    active_tab: usize,
    // タブに割り振る連番。閉じても使い回さない（egui の push_id を安定させるため）。
    next_tab_id: u64,

    // UI状態
    // 次フレーム終了後に開始する OLE ドラッグの対象パス
    pending_drag: Option<Vec<String>>,
    toast_message: Option<(String, std::time::Instant)>,
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

        let first_tab = TabState::new(cc.egui_ctx.clone(), 0, target_dir, kind_filter, sort_mode);

        Self {
            config,
            tabs: vec![first_tab],
            active_tab: 0,
            next_tab_id: 1,
            pending_drag: None,
            toast_message: None,
            show_sidebar,
        }
    }

    fn active_tab(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab]
    }

    /// 現在のタブの検索対象・設定を引き継いだ新規タブを開く。
    fn open_new_tab(&mut self, ctx: &egui::Context, target_dir: String) {
        let kind_filter = self.config.kind_filter;
        let sort_mode = self.config.sort_mode;
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = TabState::new(ctx.clone(), id, target_dir, kind_filter, sort_mode);
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
    }

    fn close_tab(&mut self, index: usize) {
        if self.tabs.len() <= 1 {
            return; // 最後の1枚は閉じない
        }
        self.tabs.remove(index);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len() - 1;
        } else if self.active_tab > index {
            self.active_tab -= 1;
        }
    }

    fn show_toast(&mut self, text: &str) {
        self.toast_message = Some((text.to_string(), std::time::Instant::now()));
    }

    fn show_toast_opt(&mut self, text: Option<String>) {
        if let Some(text) = text {
            self.show_toast(&text);
        }
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
        self.copy_to_clipboard(&paths.join("\n"));
    }

    fn copy_to_clipboard(&mut self, text: &str) {
        if let Ok(mut clipboard) = arboard::Clipboard::new() {
            if clipboard.set_text(text).is_ok() {
                self.show_toast("クリップボードにコピーしました！");
            }
        }
    }
}

impl eframe::App for FileSearchApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ==========================================
        // バックグラウンド処理の反映（全タブ分）
        // ==========================================
        let mut any_scanning = false;
        let max_results = self.config.max_results;
        for tab in self.tabs.iter_mut() {
            for event in tab.check_background_messages() {
                match event {
                    TabEvent::Validated(path) => {
                        self.config.last_target_dir = path.clone();
                        self.config.add_recent_folder(&path);
                        self.config.save();
                    }
                }
            }
            tab.update_search_results_if_needed(max_results);
            tab.check_search_results();
            if tab.scan_status == ScanStatus::Scanning {
                any_scanning = true;
            }
        }

        if any_scanning {
            ctx.request_repaint_after(std::time::Duration::from_millis(50));
        }

        if let Some((_, created_at)) = &self.toast_message {
            if created_at.elapsed().as_secs_f32() > 2.5 {
                self.toast_message = None;
            }
        }

        // ==========================================
        // タブ操作のショートカット（Ctrl+T: 新規タブ、Ctrl+W: タブを閉じる）
        // ==========================================
        let (new_tab_shortcut, close_tab_shortcut) = ctx.input(|input| {
            (
                input.modifiers.command && input.key_pressed(egui::Key::T),
                input.modifiers.command && input.key_pressed(egui::Key::W),
            )
        });
        if new_tab_shortcut {
            let current_dir = self.tabs[self.active_tab].target_dir.clone();
            self.open_new_tab(ctx, current_dir);
        }
        if close_tab_shortcut {
            self.close_tab(self.active_tab);
        }

        // ==========================================
        // 上部パネル（タブバー＋操作バー）
        // ==========================================
        let mut new_tab_requested = false;
        let mut tab_to_activate: Option<usize> = None;
        let mut tab_to_close: Option<usize> = None;

        egui::TopBottomPanel::top("tab_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ScrollArea::horizontal().show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (index, tab) in self.tabs.iter().enumerate() {
                        ui.push_id(tab.id, |ui| {
                            let is_active = index == self.active_tab;
                            let scanning_marker = if tab.scan_status == ScanStatus::Scanning {
                                "⚡ "
                            } else {
                                ""
                            };
                            let label = format!("{}{}", scanning_marker, tab.title());

                            ui.group(|ui| {
                                ui.horizontal(|ui| {
                                    if ui.selectable_label(is_active, RichText::new(label)).clicked()
                                    {
                                        tab_to_activate = Some(index);
                                    }
                                    if self.tabs.len() > 1
                                        && ui.small_button("✕").on_hover_text("タブを閉じる (Ctrl+W)").clicked()
                                    {
                                        tab_to_close = Some(index);
                                    }
                                });
                            });
                        });
                    }

                    if ui
                        .button(RichText::new("＋").strong())
                        .on_hover_text("新しいタブを開く（現在のフォルダを引き継ぎます） (Ctrl+T)")
                        .clicked()
                    {
                        new_tab_requested = true;
                    }
                });
            });
            ui.add_space(4.0);
        });

        if let Some(index) = tab_to_activate {
            self.active_tab = index;
        }
        if let Some(index) = tab_to_close {
            self.close_tab(index);
        }
        if new_tab_requested {
            let current_dir = self.tabs[self.active_tab].target_dir.clone();
            self.open_new_tab(ctx, current_dir);
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
                    let scan_status = self.active_tab().scan_status;
                    // スキャン制御ボタン
                    match scan_status {
                        ScanStatus::Scanning => {
                            if ui
                                .button("🔄 最初から")
                                .on_hover_text("キャッシュをクリアして最初から再スキャンします")
                                .clicked()
                            {
                                self.active_tab().trigger_scan();
                            }
                            if ui
                                .button("⏸ 一時停止")
                                .on_hover_text("走査を一時停止します（続きから再開可能）")
                                .clicked()
                            {
                                let msg = self.active_tab().pause_scan();
                                self.show_toast_opt(msg);
                            }
                            ui.spinner();
                        }
                        ScanStatus::Paused => {
                            if ui
                                .button("🔄 最初から")
                                .on_hover_text("キャッシュをクリアして最初から再スキャンします")
                                .clicked()
                            {
                                self.active_tab().trigger_scan();
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
                                let msg = self.active_tab().resume_scan();
                                self.show_toast_opt(msg);
                            }
                        }
                        ScanStatus::Idle | ScanStatus::Warning => {
                            if ui
                                .button("🔄 再スキャン")
                                .on_hover_text("フォルダ内を最初から再スキャンします")
                                .clicked()
                            {
                                self.active_tab().trigger_scan();
                            }
                        }
                        ScanStatus::Failed => {
                            if ui.button("🔄 再試行").clicked() {
                                self.active_tab().trigger_scan();
                            }
                        }
                    }

                    // お気に入りボタン
                    let is_fav = self.tabs[self.active_tab].is_favorite(&self.config);
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
                        && !self.active_tab().target_dir.trim().is_empty()
                    {
                        let dir = self.active_tab().target_dir.clone();
                        self.config.add_favorite(None, &dir);
                        self.config.save();
                        self.show_toast("お気に入りに追加しました！");
                    }

                    // 参照ボタン
                    if ui.button("📂 参照...").clicked() {
                        let current_dir = self.active_tab().target_dir.clone();
                        if let Some(folder) = rfd::FileDialog::new()
                            .set_directory(&current_dir)
                            .pick_folder()
                        {
                            let tab = self.active_tab();
                            tab.target_dir = folder.to_string_lossy().to_string();
                            tab.trigger_scan();
                        }
                    }

                    // 残った中央のスペース全域をパス入力欄に充当
                    let dir_edit = ui.add_sized(
                        [ui.available_width(), 24.0],
                        egui::TextEdit::singleline(&mut self.active_tab().target_dir)
                            .hint_text("検索する親フォルダのパス（例: D:\\Projects）"),
                    );
                    if dir_edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        self.active_tab().trigger_scan();
                    }
                });
            });

            ui.add_space(6.0);

            // 2. 検索バー行
            ui.horizontal(|ui| {
                ui.label(RichText::new("🔍 検索:").strong().size(16.0));
                let tab = self.active_tab();
                let mut query_text = tab.query_text.clone();
                let search_input = ui.add_sized(
                    [ui.available_width() - 10.0, 28.0],
                    egui::TextEdit::singleline(&mut query_text)
                        .hint_text("キーワード（スペースでAND、| でOR、! で除外）")
                        .font(egui::TextStyle::Heading)
                        // IMEの候補選択に使うTabで次のウィジェットへ移動させない。
                        .lock_focus(true),
                );

                if !tab.has_focused_search {
                    search_input.request_focus();
                    tab.has_focused_search = true;
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
                        let end = egui::text::CCursor::new(query_text.chars().count());
                        state
                            .cursor
                            .set_char_range(Some(egui::text::CCursorRange::one(end)));
                        state.store(ui.ctx(), search_input.id);
                    }
                }

                if search_input.changed() {
                    tab.set_query_text(query_text);
                }
            });

            ui.add_space(8.0);
        });

        // ==========================================
        // 下部ステータスバー
        // ==========================================
        egui::TopBottomPanel::bottom("bottom_panel").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let tab = self.active_tab();
                let status_color = match tab.scan_status {
                    ScanStatus::Scanning => Color32::from_rgb(255, 190, 80),
                    ScanStatus::Paused => Color32::from_rgb(255, 140, 140),
                    ScanStatus::Warning => Color32::from_rgb(255, 190, 80),
                    ScanStatus::Failed => Color32::from_rgb(255, 100, 100),
                    ScanStatus::Idle => Color32::LIGHT_GRAY,
                };
                ui.label(
                    RichText::new(&tab.status_message)
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
            let active_dir = self.tabs[self.active_tab].target_dir.clone();
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
                                let is_active = active_dir.trim() == bookmark.path.trim();

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
                                let is_active = active_dir.trim() == path_str.trim();
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
            let tab = self.active_tab();
            tab.target_dir = path;
            tab.trigger_scan();
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
        let mut copy_files_request: Option<Vec<String>> = None;
        let mut copy_text_request: Option<String> = None;
        let mut search_in_folder_request: Option<String> = None;
        let mut open_in_explorer_request: Option<String> = None;

        egui::CentralPanel::default().show(ctx, |ui| {
            let active_idx = self.active_tab;
            let tab = &mut self.tabs[active_idx];
            let hit_count = tab.filtered_indices.len();
            let total_count = tab.cached_entries.len();

            // 1. サマリーバー
            ui.horizontal(|ui| {
                let status_prefix = match tab.scan_status {
                    ScanStatus::Scanning => "⚡ 走査中...",
                    ScanStatus::Paused => "⏸ 一時停止中",
                    ScanStatus::Warning => "⚠ 完了（警告あり）",
                    ScanStatus::Failed => "⚠ 走査エラー",
                    ScanStatus::Idle => "✓ 完了",
                };
                let status_color = match tab.scan_status {
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

                if tab.selected_paths.len() > 1 {
                    ui.label(
                        RichText::new(format!("☑ {} 件選択中", tab.selected_paths.len()))
                            .size(11.0)
                            .color(Color32::from_rgb(140, 220, 140)),
                    );
                }

                if let Some(duration) = tab.last_scan_duration_ms {
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

            let mut next_sort_mode: Option<SortMode> = None;
            let mut next_kind_filter: Option<KindFilter> = None;

            ui.horizontal(|ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(pane_width, 22.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.add_sized(
                            [24.0, 20.0],
                            egui::Label::new(RichText::new("\u{1F4CB}").size(11.0)),
                        );

                        let (name_arrow, next_name_mode) = match tab.sort_mode {
                            SortMode::NameAsc => ("\u{25B2}", SortMode::NameDesc),
                            SortMode::NameDesc => ("\u{25BC}", SortMode::NameAsc),
                            _ => ("\u{2195}", SortMode::NameAsc),
                        };
                        let (date_arrow, next_date_mode) = match tab.sort_mode {
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
                                    next_sort_mode = Some(next_date_mode);
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
                                            next_sort_mode = Some(next_name_mode);
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

                let (path_arrow, next_path_mode) = match tab.sort_mode {
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
                    next_sort_mode = Some(next_path_mode);
                }

                // 右端：種別フィルタのトグル
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut kind = tab.kind_filter;
                    // 右詰めレイアウトなので、見た目の並びに合わせて逆順に置く。
                    ui.selectable_value(
                        &mut kind,
                        KindFilter::DirsOnly,
                        "\u{1F4C1} フォルダのみ",
                    );
                    ui.selectable_value(
                        &mut kind,
                        KindFilter::FilesOnly,
                        "\u{1F4C4} ファイルのみ",
                    );
                    ui.selectable_value(&mut kind, KindFilter::Both, "\u{1F5C2} 両方");
                    if kind != tab.kind_filter {
                        next_kind_filter = Some(kind);
                    }
                });
            });

            if let Some(mode) = next_sort_mode {
                tab.set_sort_mode(mode);
                self.config.sort_mode = mode;
                self.config.save();
            }
            if let Some(kind) = next_kind_filter {
                tab.set_kind_filter(kind);
                self.config.kind_filter = kind;
                self.config.save();
            }

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
            let selected_paths = &tab.selected_paths;
            // 表示順を保った選択一覧。まとめて操作するときの対象になる。
            let visible_selected: Vec<&str> = tab
                .filtered_indices
                .iter()
                .filter_map(|&index| tab.cached_entries.get(index))
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
                    tab.filtered_indices.len(),
                    |ui, row_range| {
                        for i in row_range {
                            let Some(&entry_idx) = tab.filtered_indices.get(i) else {
                                continue;
                            };
                            let Some(entry) = tab.cached_entries.get(entry_idx) else {
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
                    tab.apply_selection(path, row, ctrl, shift);
                    // 行を選んだら検索欄のフォーカスを外す。以降の Enter が
                    // 一覧の「開く」として働くようにするため。
                    ctx.memory_mut(|memory| memory.stop_text_input());
                }
                Some(TableAction::Open(path)) => {
                    let _ = open::that(path);
                }
                Some(TableAction::OpenInExplorer(path)) => {
                    open_in_explorer_request = Some(path);
                }
                Some(TableAction::CopyToClipboard(text)) => {
                    copy_text_request = Some(text);
                }
                Some(TableAction::CopyFilesToClipboard(paths)) => {
                    copy_files_request = Some(paths);
                }
                Some(TableAction::SearchInFolder(path)) => {
                    search_in_folder_request = Some(path);
                }
                None => {}
            }

            self.pending_drag = pending_drag;
        });

        // テーブル描画中に発生した副作用（クリップボード・エクスプローラー表示・
        // フォルダ内検索）は、CentralPanel の可変借用が終わった後にまとめて実行する。
        if let Some(path) = open_in_explorer_request {
            self.open_in_explorer(&path);
        }
        if let Some(path) = search_in_folder_request {
            let tab = self.active_tab();
            tab.target_dir = path;
            tab.trigger_scan();
        }
        if let Some(text) = copy_text_request {
            self.copy_to_clipboard(&text);
        }
        if let Some(paths) = copy_files_request {
            self.copy_files_to_clipboard(&paths);
        }

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
            let msg = self.active_tab().open_selected();
            self.show_toast_opt(msg);
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
