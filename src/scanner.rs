use crate::entry::FileEntry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 1回のUI通知にまとめる件数。小さすぎるとチャネル送信と再描画の回数が増える。
const BATCH_SIZE: usize = 1000;

static SCAN_SESSION_LOCK: Mutex<()> = Mutex::new(());

fn scan_thread_pool() -> Arc<rayon::ThreadPool> {
    static SCAN_THREAD_POOL: OnceLock<Arc<rayon::ThreadPool>> = OnceLock::new();

    Arc::clone(SCAN_THREAD_POOL.get_or_init(|| {
        let available_threads = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(2);
        let scan_threads = available_threads.saturating_sub(1).clamp(2, 8);

        Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(scan_threads)
                .thread_name(|index| format!("file-scan-{index}"))
                .build()
                .expect("走査用スレッドプールを作成できませんでした"),
        )
    }))
}

pub enum ScanMessage {
    /// パス検証が完了し、走査を開始できる状態になった通知
    Validated(String),
    /// 走査途中で見つかったファイル群（ストリーミングバッチ）
    Batch(Vec<Arc<FileEntry>>),
    /// スキャン完了通知
    Complete {
        total_count: usize,
        duration_ms: u128,
        warning_count: usize,
        first_warning: Option<String>,
    },
    Error(String),
}

/// スレッドの一時停止・再開・キャンセルを制御する構造体
#[derive(Clone)]
pub struct ScanControl {
    pub pause_state: Arc<(Mutex<bool>, Condvar)>,
    pub is_cancelled: Arc<AtomicBool>,
}

impl ScanControl {
    pub fn new() -> Self {
        Self {
            pause_state: Arc::new((Mutex::new(false), Condvar::new())),
            is_cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 一時停止
    pub fn pause(&self) {
        let (lock, _cvar) = &*self.pause_state;
        if let Ok(mut paused) = lock.lock() {
            *paused = true;
        }
    }

    /// 再開
    pub fn resume(&self) {
        let (lock, cvar) = &*self.pause_state;
        if let Ok(mut paused) = lock.lock() {
            *paused = false;
            cvar.notify_all();
        }
    }

    /// 完全キャンセル
    pub fn cancel(&self) {
        self.is_cancelled.store(true, Ordering::Relaxed);
        // 待機中のスレッドを起こして即終了させる
        self.resume();
    }
}

/// 走査中に全ワーカースレッドで共有する集計・送信状態
struct WalkContext {
    tx: Sender<ScanMessage>,
    control: ScanControl,
    buffer: Mutex<Vec<Arc<FileEntry>>>,
    total_count: AtomicUsize,
    warning_count: AtomicUsize,
    first_error: Mutex<Option<String>>,
    /// UI側の受信が閉じられたら以降の走査を打ち切るためのフラグ
    send_failed: AtomicBool,
}

impl WalkContext {
    fn new(tx: Sender<ScanMessage>, control: ScanControl) -> Self {
        Self {
            tx,
            control,
            buffer: Mutex::new(Vec::with_capacity(BATCH_SIZE)),
            total_count: AtomicUsize::new(0),
            warning_count: AtomicUsize::new(0),
            first_error: Mutex::new(None),
            send_failed: AtomicBool::new(false),
        }
    }

    fn should_stop(&self) -> bool {
        self.control.is_cancelled.load(Ordering::Relaxed)
            || self.send_failed.load(Ordering::Relaxed)
    }

    /// 一時停止中はワーカースレッドを完全に休止させる。
    /// ディレクトリ単位で呼ぶため、エントリ1件ごとのロック取得は発生しない。
    fn wait_while_paused(&self) {
        let (lock, cvar) = &*self.control.pause_state;
        if let Ok(mut paused) = lock.lock() {
            while *paused && !self.control.is_cancelled.load(Ordering::Relaxed) {
                paused = match cvar.wait(paused) {
                    Ok(guard) => guard,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
        }
    }

    fn record_warning(&self, message: String) {
        self.warning_count.fetch_add(1, Ordering::Relaxed);
        let mut first = self
            .first_error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        first.get_or_insert(message);
    }

    /// 1ディレクトリ分をまとめて登録し、閾値を超えた分だけUIへ送る
    fn push_entries(&self, entries: Vec<Arc<FileEntry>>) {
        if entries.is_empty() {
            return;
        }
        self.total_count.fetch_add(entries.len(), Ordering::Relaxed);

        let ready = {
            let mut buffer = self
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            buffer.extend(entries);
            if buffer.len() >= BATCH_SIZE {
                Some(std::mem::replace(
                    &mut *buffer,
                    Vec::with_capacity(BATCH_SIZE),
                ))
            } else {
                None
            }
        };

        // 送信はロックを手放してから行う。
        if let Some(batch) = ready {
            self.send_batch(batch);
        }
    }

    fn flush(&self) {
        let remaining = {
            let mut buffer = self
                .buffer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            std::mem::take(&mut *buffer)
        };
        self.send_batch(remaining);
    }

    fn send_batch(&self, batch: Vec<Arc<FileEntry>>) {
        if batch.is_empty() {
            return;
        }
        if self.tx.send(ScanMessage::Batch(batch)).is_err() {
            self.send_failed.store(true, Ordering::Relaxed);
        }
    }
}

/// 1ディレクトリを読み、ファイルは即座に登録し、サブディレクトリは別タスクへ分岐させる。
///
/// `fs::DirEntry::metadata()` はWindowsではディレクトリ列挙時のキャッシュから返るため
/// 追加のシステムコールが発生しない。パス指定の `symlink_metadata` を使うと1件ごとに
/// ファイルを開くことになり、数万件規模では数秒〜数十秒の差になる。
fn walk_directory<'scope>(
    scope: &rayon::Scope<'scope>,
    dir: PathBuf,
    ctx: Arc<WalkContext>,
) {
    if ctx.should_stop() {
        return;
    }
    ctx.wait_while_paused();
    if ctx.should_stop() {
        return;
    }

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(read_dir) => read_dir,
        Err(err) => {
            ctx.record_warning(format!("{}: {}", dir.display(), err));
            return;
        }
    };

    let mut entries = Vec::new();
    let mut child_dirs = Vec::new();

    for dir_entry_result in read_dir {
        let dir_entry = match dir_entry_result {
            Ok(dir_entry) => dir_entry,
            Err(err) => {
                ctx.record_warning(format!("{}: {}", dir.display(), err));
                continue;
            }
        };

        // file_type() もWindowsでは列挙結果のキャッシュから返る。
        let file_type = match dir_entry.file_type() {
            Ok(file_type) => file_type,
            Err(err) => {
                ctx.record_warning(format!("{}: {}", dir_entry.path().display(), err));
                continue;
            }
        };

        let path = dir_entry.path();
        let is_dir = file_type.is_dir();
        let (size, modified) = match dir_entry.metadata() {
            Ok(metadata) => (metadata.len(), metadata.modified().ok()),
            Err(_) => (0, None),
        };

        entries.push(Arc::new(FileEntry::new(
            dir_entry.file_name().to_string_lossy().to_string(),
            path.to_string_lossy().to_string(),
            is_dir,
            size,
            modified,
        )));

        // シンボリックリンクは辿らない（file_type がリンク自体を指すため循環しない）。
        if is_dir {
            child_dirs.push(path);
        }
    }

    ctx.push_entries(entries);

    for child in child_dirs {
        if ctx.should_stop() {
            return;
        }
        let child_ctx = Arc::clone(&ctx);
        scope.spawn(move |inner_scope| walk_directory(inner_scope, child, child_ctx));
    }
}

/// ルート自身も1件として登録する（従来の走査結果と件数を揃えるため）
fn push_root_entry(path: &Path, ctx: &WalkContext) {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string_lossy().to_string());
    let (size, modified) = std::fs::metadata(path)
        .map(|metadata| (metadata.len(), metadata.modified().ok()))
        .unwrap_or((0, None));

    ctx.push_entries(vec![Arc::new(FileEntry::new(
        file_name,
        path.to_string_lossy().to_string(),
        true,
        size,
        modified,
    ))]);
}

/// バックグラウンドで指定フォルダを高速並列走査（一時停止・再開・キャンセル対応）
pub fn start_scan(root_path: String, tx: Sender<ScanMessage>, control: ScanControl) {
    std::thread::spawn(move || {
        if control.is_cancelled.load(Ordering::Relaxed) {
            return;
        }
        let path = Path::new(&root_path);
        if !path.exists() || !path.is_dir() {
            let _ = tx.send(ScanMessage::Error(format!(
                "指定されたフォルダが存在しないか、ディレクトリではありません: {}",
                root_path
            )));
            return;
        }

        // パス検証は並行可能だが、走査本体は常に1世代だけ実行する。
        let _session_guard = SCAN_SESSION_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if control.is_cancelled.load(Ordering::Relaxed) {
            return;
        }
        if tx.send(ScanMessage::Validated(root_path.clone())).is_err() {
            return;
        }

        let start_time = Instant::now();
        let pool = scan_thread_pool();

        // フォルダ切り替え直後などの一時的なルート読込失敗は一度だけ再試行する。
        for attempt in 0..2 {
            let ctx = Arc::new(WalkContext::new(tx.clone(), control.clone()));

            push_root_entry(path, &ctx);

            let walk_ctx = Arc::clone(&ctx);
            pool.install(|| {
                rayon::scope(|scope| {
                    walk_directory(scope, path.to_path_buf(), walk_ctx);
                });
            });

            if control.is_cancelled.load(Ordering::Relaxed) {
                return;
            }

            ctx.flush();
            if ctx.send_failed.load(Ordering::Relaxed) {
                return;
            }

            let total_count = ctx.total_count.load(Ordering::Relaxed);
            let warning_count = ctx.warning_count.load(Ordering::Relaxed);
            let first_error = ctx
                .first_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();

            // ルート1件しか登録できず警告だけが出た場合は走査失敗とみなす。
            if total_count > 1 || warning_count == 0 {
                let _ = tx.send(ScanMessage::Complete {
                    total_count,
                    duration_ms: start_time.elapsed().as_millis(),
                    warning_count,
                    first_warning: first_error,
                });
                return;
            }

            if attempt == 0 && !control.is_cancelled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }

            let detail = first_error.unwrap_or_else(|| "項目を読み取れませんでした".to_string());
            let _ = tx.send(ScanMessage::Error(format!(
                "フォルダの走査に失敗しました: {} ({})",
                root_path, detail
            )));
            return;
        }
    });
}
