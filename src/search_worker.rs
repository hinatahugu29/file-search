use crate::config::SortMode;
use crate::entry::FileEntry;
use crate::query::{filter_indices_parallel, SearchQuery};
use eframe::egui;
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::Arc;

enum SearchCommand {
    Reset,
    AddBatch(Vec<Arc<FileEntry>>),
    Search {
        request_id: u64,
        query: SearchQuery,
        sort_mode: SortMode,
        limit: usize,
    },
}

pub struct SearchResult {
    pub request_id: u64,
    pub indices: Vec<usize>,
}

pub struct SearchWorker {
    command_tx: Sender<SearchCommand>,
    result_rx: Receiver<SearchResult>,
}

impl SearchWorker {
    pub fn new(repaint_context: egui::Context) -> Self {
        let (command_tx, command_rx) = channel();
        let (result_tx, result_rx) = channel();

        std::thread::spawn(move || {
            let available_threads = std::thread::available_parallelism()
                .map(|count| count.get())
                .unwrap_or(1);
            let search_threads = available_threads.saturating_sub(1).clamp(1, 4);
            let thread_pool = rayon::ThreadPoolBuilder::new()
                .num_threads(search_threads)
                .thread_name(|index| format!("file-search-{index}"))
                .build()
                .expect("検索用スレッドプールを作成できませんでした");

            let mut entries = Vec::new();

            while let Ok(first_command) = command_rx.recv() {
                let mut pending_search = None;
                apply_command(first_command, &mut entries, &mut pending_search);

                // 検索中に届いた要求をまとめ、最新の検索条件だけを実行する。
                while let Ok(command) = command_rx.try_recv() {
                    apply_command(command, &mut entries, &mut pending_search);
                }

                if let Some((request_id, query, sort_mode, limit)) = pending_search {
                    let indices = thread_pool
                        .install(|| filter_indices_parallel(&entries, &query, sort_mode, limit));

                    if result_tx
                        .send(SearchResult {
                            request_id,
                            indices,
                        })
                        .is_err()
                    {
                        return;
                    }
                    repaint_context.request_repaint();
                }
            }
        });

        Self {
            command_tx,
            result_rx,
        }
    }

    pub fn reset(&self) {
        let _ = self.command_tx.send(SearchCommand::Reset);
    }

    pub fn add_batch(&self, batch: Vec<Arc<FileEntry>>) {
        let _ = self.command_tx.send(SearchCommand::AddBatch(batch));
    }

    pub fn search(&self, request_id: u64, query: SearchQuery, sort_mode: SortMode, limit: usize) {
        let _ = self.command_tx.send(SearchCommand::Search {
            request_id,
            query,
            sort_mode,
            limit,
        });
    }

    pub fn try_recv(&self) -> Result<SearchResult, TryRecvError> {
        self.result_rx.try_recv()
    }
}

fn apply_command(
    command: SearchCommand,
    entries: &mut Vec<Arc<FileEntry>>,
    pending_search: &mut Option<(u64, SearchQuery, SortMode, usize)>,
) {
    match command {
        SearchCommand::Reset => {
            entries.clear();
            *pending_search = None;
        }
        SearchCommand::AddBatch(batch) => entries.extend(batch),
        SearchCommand::Search {
            request_id,
            query,
            sort_mode,
            limit,
        } => {
            *pending_search = Some((request_id, query, sort_mode, limit));
        }
    }
}
