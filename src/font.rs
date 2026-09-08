use eframe::egui::{FontData, FontDefinitions, FontFamily};
use std::fs;
use std::path::Path;

/// Windowsの日本語フォントをeguiに読み込んで文字化けを防止する
pub fn setup_japanese_fonts(ctx: &eframe::egui::Context) {
    let mut fonts = FontDefinitions::default();

    // Windows標準の日本語フォント候補（優先度順）
    let font_candidates = [
        "C:\\Windows\\Fonts\\meiryo.ttc",
        "C:\\Windows\\Fonts\\YuGothM.ttc",
        "C:\\Windows\\Fonts\\msgothic.ttc",
        "C:\\Windows\\Fonts\\yugothib.ttf",
    ];

    let mut loaded = false;
    for font_path in &font_candidates {
        if Path::new(font_path).exists() {
            if let Ok(font_bytes) = fs::read(font_path) {
                fonts
                    .font_data
                    .insert("japanese_font".to_owned(), FontData::from_owned(font_bytes));

                // Proportional（通常テキスト）に日本語フォントを追加
                fonts
                    .families
                    .entry(FontFamily::Proportional)
                    .or_default()
                    .insert(0, "japanese_font".to_owned());

                // Monospace（等幅テキスト）にも追加
                fonts
                    .families
                    .entry(FontFamily::Monospace)
                    .or_default()
                    .push("japanese_font".to_owned());

                loaded = true;
                break;
            }
        }
    }

    if loaded {
        ctx.set_fonts(fonts);
    }
}
