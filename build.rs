//! `src/icon.png` を唯一の原本として、
//!  - exe に埋め込む Windows アイコン（エクスプローラーやデスクトップに表示される）
//!  - ウィンドウ・タスクバー用の RGBA データ
//! を生成する。PNG を差し替えれば両方が自動で追従する。

use std::path::{Path, PathBuf};

/// exe 埋め込み用アイコンに含めるサイズ。
/// Windows は表示場所（デスクトップ/一覧/タスクバー）に応じて最適なものを選ぶ。
const ICON_SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

/// ウィンドウ・タスクバー用に実行時へ渡すサイズ
const WINDOW_ICON_SIZE: u32 = 64;

fn main() {
    let source = Path::new("src/icon.png");
    println!("cargo:rerun-if-changed=src/icon.png");
    println!("cargo:rerun-if-changed=build.rs");

    if !source.exists() {
        println!("cargo:warning=src/icon.png が見つからないため、アイコンの埋め込みをスキップします");
        return;
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR が設定されていません"));
    let image = image::open(source)
        .expect("src/icon.png を読み込めませんでした")
        .to_rgba8();

    write_window_icon(&image, &out_dir);

    #[cfg(windows)]
    embed_exe_icon(&image, &out_dir);
}

/// ウィンドウ用アイコンを生の RGBA バイト列として書き出す。
/// 実行時に画像デコードを行わずに済ませるため。
fn write_window_icon(image: &image::RgbaImage, out_dir: &Path) {
    let resized = image::imageops::resize(
        image,
        WINDOW_ICON_SIZE,
        WINDOW_ICON_SIZE,
        image::imageops::FilterType::Lanczos3,
    );
    std::fs::write(out_dir.join("window_icon.rgba"), resized.into_raw())
        .expect("ウィンドウアイコンを書き出せませんでした");
}

/// マルチサイズの .ico を生成し、リソースとして exe に埋め込む
#[cfg(windows)]
fn embed_exe_icon(image: &image::RgbaImage, out_dir: &Path) {
    let mut icon_dir = ico::IconDir::new(ico::ResourceType::Icon);

    for size in ICON_SIZES {
        let resized =
            image::imageops::resize(image, size, size, image::imageops::FilterType::Lanczos3);
        let ico_image = ico::IconImage::from_rgba_data(size, size, resized.into_raw());
        icon_dir.add_entry(
            ico::IconDirEntry::encode(&ico_image).expect("アイコンをエンコードできませんでした"),
        );
    }

    let ico_path = out_dir.join("icon.ico");
    let file = std::fs::File::create(&ico_path).expect("icon.ico を作成できませんでした");
    icon_dir
        .write(std::io::BufWriter::new(file))
        .expect("icon.ico を書き出せませんでした");

    let mut resource = winresource::WindowsResource::new();
    resource.set_icon(ico_path.to_str().expect("ico のパスが不正です"));
    resource.set("ProductName", "高速ファイル検索");
    resource.set("FileDescription", "高速ファイル検索 - Fast File Search");

    // リソースコンパイラが無い環境ではビルド自体は通したいので、失敗しても警告に留める。
    if let Err(err) = resource.compile() {
        println!("cargo:warning=アイコンを exe に埋め込めませんでした: {err}");
    }
}
