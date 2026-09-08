use crate::config::{KindFilter, SortMode};
use crate::entry::FileEntry;
use rayon::prelude::*;
use std::cmp::Ordering;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchToken {
    Contains(String),
    NotContains(String),
}

impl MatchToken {
    pub fn matches(&self, name_lower: &str, path_lower: &str) -> bool {
        match self {
            MatchToken::Contains(term) => {
                if term.contains('\\') || term.contains('/') {
                    let norm_term = term.replace('/', "\\");
                    path_lower.contains(&norm_term)
                } else {
                    name_lower.contains(term) || path_lower.contains(term)
                }
            }
            MatchToken::NotContains(term) => {
                if term.contains('\\') || term.contains('/') {
                    let norm_term = term.replace('/', "\\");
                    !path_lower.contains(&norm_term)
                } else {
                    !name_lower.contains(term) && !path_lower.contains(term)
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrClause {
    pub tokens: Vec<MatchToken>,
}

impl OrClause {
    pub fn matches(&self, name_lower: &str, path_lower: &str) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        let has_positive = self
            .tokens
            .iter()
            .any(|t| matches!(t, MatchToken::Contains(_)));
        if has_positive {
            self.tokens.iter().any(|t| match t {
                MatchToken::Contains(_) => t.matches(name_lower, path_lower),
                MatchToken::NotContains(_) => false,
            })
        } else {
            self.tokens
                .iter()
                .all(|t| t.matches(name_lower, path_lower))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    pub raw: String,
    pub and_clauses: Vec<OrClause>,
    /// ファイル / フォルダの絞り込み。パース結果ではなく UI トグルから設定する。
    pub kind: KindFilter,
}

impl SearchQuery {
    pub fn parse(input: &str) -> Self {
        let normalized = input.replace('\u{3000}', " ").trim().to_lowercase();

        if normalized.is_empty() {
            return Self {
                raw: input.to_string(),
                and_clauses: Vec::new(),
                kind: KindFilter::default(),
            };
        }

        let mut and_clauses = Vec::new();
        for part in normalized.split_whitespace() {
            let mut or_tokens = Vec::new();

            let or_parts: Vec<&str> = if part.contains('|') {
                part.split('|').collect()
            } else {
                vec![part]
            };

            for item in or_parts {
                let item = item.trim();
                if item.is_empty() {
                    continue;
                }

                if let Some(stripped) = item.strip_prefix('!').or_else(|| item.strip_prefix('-')) {
                    if !stripped.is_empty() {
                        or_tokens.push(MatchToken::NotContains(stripped.to_string()));
                    }
                } else {
                    or_tokens.push(MatchToken::Contains(item.to_string()));
                }
            }

            if !or_tokens.is_empty() {
                and_clauses.push(OrClause { tokens: or_tokens });
            }
        }

        Self {
            raw: input.to_string(),
            and_clauses,
            kind: KindFilter::default(),
        }
    }

    /// 絞り込みが一切ない（全件そのまま通る）状態か。
    pub fn is_empty(&self) -> bool {
        self.and_clauses.is_empty() && self.kind == KindFilter::Both
    }

    pub fn matches(&self, entry: &FileEntry) -> bool {
        if !self.kind.accepts(entry.is_dir) {
            return false;
        }
        if self.and_clauses.is_empty() {
            return true;
        }
        self.and_clauses
            .iter()
            .all(|clause| clause.matches(&entry.name_lower, &entry.path_lower))
    }
}

/// Rayonによる高速並列検索 ＋ 指定ソートモード適用済みインデックス配列を返す
pub fn filter_indices_parallel(
    entries: &[Arc<FileEntry>],
    query: &SearchQuery,
    sort_mode: SortMode,
    limit: usize,
) -> Vec<usize> {
    if entries.is_empty() {
        return Vec::new();
    }

    let mut matched_indices: Vec<usize> = if query.is_empty() {
        (0..entries.len()).collect()
    } else {
        (0..entries.len())
            .into_par_iter()
            .filter(|&i| query.matches(&entries[i]))
            .collect()
    };

    // 全ヒットをソートしてから切り詰めず、先に上位 limit 件だけを選ぶ。
    // 大量ヒット時の計算量を O(n log n) から概ね O(n + limit log limit) に抑える。
    match sort_mode {
        SortMode::DateDesc => {
            sort_top_indices(&mut matched_indices, limit, |&a, &b| {
                entries[b]
                    .modified
                    .cmp(&entries[a].modified)
                    .then_with(|| entries[a].path_lower.cmp(&entries[b].path_lower))
            });
        }
        SortMode::DateAsc => {
            sort_top_indices(&mut matched_indices, limit, |&a, &b| {
                entries[a]
                    .modified
                    .cmp(&entries[b].modified)
                    .then_with(|| entries[a].path_lower.cmp(&entries[b].path_lower))
            });
        }
        SortMode::NameAsc => {
            sort_top_indices(&mut matched_indices, limit, |&a, &b| {
                entries[a]
                    .name_lower
                    .cmp(&entries[b].name_lower)
                    .then_with(|| entries[a].path_lower.cmp(&entries[b].path_lower))
            });
        }
        SortMode::NameDesc => {
            sort_top_indices(&mut matched_indices, limit, |&a, &b| {
                entries[b]
                    .name_lower
                    .cmp(&entries[a].name_lower)
                    .then_with(|| entries[a].path_lower.cmp(&entries[b].path_lower))
            });
        }
        SortMode::PathAsc => {
            sort_top_indices(&mut matched_indices, limit, |&a, &b| {
                entries[a]
                    .path_lower
                    .cmp(&entries[b].path_lower)
                    .then_with(|| a.cmp(&b))
            });
        }
        SortMode::PathDesc => {
            sort_top_indices(&mut matched_indices, limit, |&a, &b| {
                entries[b]
                    .path_lower
                    .cmp(&entries[a].path_lower)
                    .then_with(|| a.cmp(&b))
            });
        }
    }

    matched_indices
}

fn sort_top_indices<F>(indices: &mut Vec<usize>, limit: usize, mut compare: F)
where
    F: FnMut(&usize, &usize) -> Ordering,
{
    if limit == 0 {
        indices.clear();
        return;
    }

    if indices.len() > limit {
        indices.select_nth_unstable_by(limit, &mut compare);
        indices.truncate(limit);
    }
    indices.sort_unstable_by(compare);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn test_query_normalization_and() {
        let q = SearchQuery::parse("Rust　2026　PDF");
        assert_eq!(q.and_clauses.len(), 3);

        let entry = FileEntry::new(
            "Rust_Guide_2026.pdf".to_string(),
            "C:\\Docs\\Rust_Guide_2026.pdf".to_string(),
            false,
            100,
            None,
        );
        assert!(q.matches(&entry));
    }

    #[test]
    fn test_filter_indices_and_sort() {
        let now = SystemTime::now();
        let older = now - Duration::from_secs(3600);

        let entry_a = Arc::new(FileEntry::new(
            "alpha.txt".to_string(),
            "C:\\alpha.txt".to_string(),
            false,
            10,
            Some(older),
        ));
        let entry_z = Arc::new(FileEntry::new(
            "zebra.txt".to_string(),
            "C:\\zebra.txt".to_string(),
            false,
            10,
            Some(now),
        ));

        let list = vec![entry_a, entry_z];
        let q = SearchQuery::parse("");

        // 日付降順 (zebraが最初)
        let res_date = filter_indices_parallel(&list, &q, SortMode::DateDesc, 10);
        assert_eq!(res_date[0], 1);

        // 名前昇順 (alphaが最初)
        let res_name = filter_indices_parallel(&list, &q, SortMode::NameAsc, 10);
        assert_eq!(res_name[0], 0);
    }

    #[test]
    fn test_partial_sort_returns_correct_top_results() {
        let now = SystemTime::now();
        let names = ["delta", "alpha", "echo", "charlie", "bravo"];
        let list: Vec<Arc<FileEntry>> = names
            .iter()
            .enumerate()
            .map(|(index, name)| {
                Arc::new(FileEntry::new(
                    format!("{name}.txt"),
                    format!("C:\\{:02}_{name}.txt", names.len() - index),
                    false,
                    10,
                    Some(now - Duration::from_secs(index as u64)),
                ))
            })
            .collect();
        let query = SearchQuery::parse("");

        let name_asc = filter_indices_parallel(&list, &query, SortMode::NameAsc, 3);
        let name_asc_values: Vec<&str> = name_asc
            .iter()
            .map(|&index| list[index].file_name.as_str())
            .collect();
        assert_eq!(name_asc_values, ["alpha.txt", "bravo.txt", "charlie.txt"]);

        let name_desc = filter_indices_parallel(&list, &query, SortMode::NameDesc, 3);
        let name_desc_values: Vec<&str> = name_desc
            .iter()
            .map(|&index| list[index].file_name.as_str())
            .collect();
        assert_eq!(name_desc_values, ["echo.txt", "delta.txt", "charlie.txt"]);

        let date_desc = filter_indices_parallel(&list, &query, SortMode::DateDesc, 2);
        assert_eq!(date_desc, [0, 1]);

        let date_asc = filter_indices_parallel(&list, &query, SortMode::DateAsc, 2);
        assert_eq!(date_asc, [4, 3]);

        let path_asc = filter_indices_parallel(&list, &query, SortMode::PathAsc, 2);
        assert_eq!(path_asc, [4, 3]);

        let empty = filter_indices_parallel(&list, &query, SortMode::NameAsc, 0);
        assert!(empty.is_empty());
    }
}
