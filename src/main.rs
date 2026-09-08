#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod config;
mod entry;
mod font;
mod query;
mod scanner;
mod search_worker;
pub mod drag_drop;

use app::FileSearchApp;
use eframe::egui::Vec2;

/// ウィンドウ・タスクバー用アイコン。
/// build.rs が src/icon.png から生成した RGBA 生データを埋め込む。
fn window_icon() -> eframe::egui::IconData {
    const SIZE: u32 = 64;
    let rgba = include_bytes!(concat!(env!("OUT_DIR"), "/window_icon.rgba")).to_vec();
    eframe::egui::IconData {
        rgba,
        width: SIZE,
        height: SIZE,
    }
}

/// Windows のタイトルバー（非クライアント領域）をダークにする。
/// DWM に直接投げるだけなので追加クレートは不要。
#[cfg(target_os = "windows")]
fn enable_dark_titlebar(cc: &eframe::CreationContext<'_>) {
    use eframe::egui::ViewportCommand;
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};

    #[link(name = "dwmapi")]
    extern "system" {
        fn DwmSetWindowAttribute(
            hwnd: isize,
            attribute: u32,
            value: *const core::ffi::c_void,
            value_size: u32,
        ) -> i32;
    }

    // 20 = DWMWA_USE_IMMERSIVE_DARK_MODE（Windows 10 2004 以降）。
    // それ以前のビルドでは 19 が使われていたので、失敗したら順に試す。
    const ATTRIBUTES: [u32; 2] = [20, 19];

    let Ok(handle) = cc.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };

    let enabled: i32 = 1;
    for attribute in ATTRIBUTES {
        let result = unsafe {
            DwmSetWindowAttribute(
                win32.hwnd.get(),
                attribute,
                &enabled as *const i32 as *const core::ffi::c_void,
                std::mem::size_of::<i32>() as u32,
            )
        };
        if result == 0 {
            break;
        }
    }

    // 適用済みの色でタイトルバーを描き直させる。
    cc.egui_ctx
        .send_viewport_cmd(ViewportCommand::Title(TITLE.to_owned()));
}

#[cfg(not(target_os = "windows"))]
fn enable_dark_titlebar(_cc: &eframe::CreationContext<'_>) {}

const TITLE: &str = "高速ファイル検索 - Fast File Search (Rust)";

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size(Vec2::new(960.0, 640.0))
            .with_min_inner_size(Vec2::new(600.0, 400.0))
            .with_icon(window_icon())
            .with_title(TITLE),
        ..Default::default()
    };

    eframe::run_native(
        "FastFileSearchApp",
        native_options,
        Box::new(|cc| {
            enable_dark_titlebar(cc);
            Ok(Box::new(FileSearchApp::new(cc)))
        }),
    )
}
