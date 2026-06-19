use std::path::PathBuf;

use crate::fs::entry::FileEntry;
use crate::fs::watcher::DirWatcher;

/// 一覧の並び替え基準となる列。
///
/// `FileEntry` が生データ（`u64` サイズ・`SystemTime`）を保持しているため、
/// どの列でも自然なソートをそのまま実装できる（整形済み文字列ではなく素の値で比較）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SortColumn {
    /// 名前（大文字小文字を無視）。既定。
    #[default]
    Name,
    /// バイト単位のサイズ。
    Size,
    /// 最終更新日時。
    Modified,
}

/// アプリの状態（真実の源）。
///
/// 「今どのディレクトリを開いていて、その中身を・どの順で見せているか」をここで保持する。
/// Slint 側はこの状態を整形して表示するだけの“表示専用”という役割分担。
///
/// この構造体は **UI スレッドからのみ** 触る前提なので `Send`/`Sync` は不要。
/// ワーカースレッドは I/O 結果（Send なデータ）を返すだけで、ここを直接触らない。
pub struct AppState {
    /// 現在表示中のディレクトリ。
    pub current_dir: PathBuf,
    /// 現在表示中の **全** エントリ一覧（フィルタ前。`current_dir` の中身そのまま）。
    ///
    /// 表示はこれをフィルタした結果だが、ここには常に全件を保持しておく。
    /// こうすることでフィルタの変更・クリア時に I/O を伴わず全件から再構築できる。
    pub entries: Vec<FileEntry>,
    /// 現在の絞り込み文字列（小文字・部分一致）。空ならフィルタ無効＝全件表示。
    ///
    /// 真実の源は Rust 側に置く方針なので、検索バーの文字列もここで保持する。
    /// 監視きっかけの再読み込み時も、この値を使って同じフィルタを再適用する。
    pub filter: String,
    /// `current_dir` を監視しているウォッチャ。生かしておくと監視が続く。
    pub watcher: Option<DirWatcher>,
    /// 現在のソート列。
    pub sort_column: SortColumn,
    /// 昇順なら true。
    pub sort_ascending: bool,
}

impl Default for AppState {
    fn default() -> Self {
        // `sort_ascending` の既定は true（名前の昇順）。bool の `Default` は false なので、
        // derive ではなく手書きで初期値を与える。
        Self {
            current_dir: PathBuf::new(),
            entries: Vec::new(),
            filter: String::new(),
            watcher: None,
            sort_column: SortColumn::Name,
            sort_ascending: true,
        }
    }
}

impl AppState {
    /// 表示中（フィルタ適用後）の行インデックスから、対応するエントリを引く。
    ///
    /// UI 側のインデックスは「フィルタ後の表示行」基準なので、ここでも同じ条件で
    /// フィルタしながら数える。範囲外・不一致なら None。
    pub fn entry_at(&self, index: usize) -> Option<&FileEntry> {
        self.visible_entries().nth(index)
    }

    /// 現在のフィルタに一致するエントリだけを、表示順のまま列挙する。
    ///
    /// フィルタが空なら全件を返す。一致判定はファイル名の小文字・部分一致。
    pub fn visible_entries(&self) -> impl Iterator<Item = &FileEntry> {
        let needle = self.filter.to_lowercase();
        self.entries
            .iter()
            .filter(move |e| needle.is_empty() || e.name.to_lowercase().contains(&needle))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::entry::FileEntry;
    use std::path::PathBuf;

    /// テスト用エントリを最小コストで生成するファクトリ。
    /// I/O は一切行わない（`size: 0`・`modified: None`）。
    fn make_entry(name: &str, is_dir: bool) -> FileEntry {
        FileEntry {
            name: name.to_string(),
            path: PathBuf::from(name),
            is_dir,
            size: 0,
            modified: None,
        }
    }

    /// フィルタが空のとき `visible_entries` が全件を返すことを守るテスト。
    #[test]
    fn visible_entries_no_filter_returns_all() {
        // フィルタは初期値のまま（空文字列）なので、全件が見えるはず
        let state = AppState {
            entries: vec![
                make_entry("alpha.txt", false),
                make_entry("beta.txt", false),
                make_entry("gamma", true),
            ],
            ..AppState::default()
        };
        let names: Vec<_> = state.visible_entries().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["alpha.txt", "beta.txt", "gamma"]);
    }

    /// フィルタを設定すると一致するエントリだけが返ることを守るテスト。
    #[test]
    fn visible_entries_filter_narrows_results() {
        let state = AppState {
            entries: vec![
                make_entry("main.rs", false),
                make_entry("lib.rs", false),
                make_entry("Cargo.toml", false),
            ],
            filter: "rs".to_string(),
            ..AppState::default()
        };
        let names: Vec<_> = state.visible_entries().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["main.rs", "lib.rs"]);
    }

    /// フィルタが大文字小文字を区別しないことを守るテスト。
    #[test]
    fn visible_entries_filter_is_case_insensitive() {
        let state = AppState {
            entries: vec![
                make_entry("Rust.rs", false),
                make_entry("python.py", false),
                make_entry("RUSTFMT", false),
            ],
            filter: "rust".to_string(),
            ..AppState::default()
        };
        let names: Vec<_> = state.visible_entries().map(|e| e.name.as_str()).collect();
        // 大文字 "Rust"・全大文字 "RUSTFMT" の両方にマッチすることを確認
        assert_eq!(names, ["Rust.rs", "RUSTFMT"]);
    }

    /// `entry_at(0)` が先頭エントリを返すことを守るテスト。
    #[test]
    fn entry_at_first_index_returns_first_entry() {
        let state = AppState {
            entries: vec![
                make_entry("first.txt", false),
                make_entry("second.txt", false),
            ],
            ..AppState::default()
        };
        let entry = state.entry_at(0).expect("先頭エントリは存在するはず");
        assert_eq!(entry.name, "first.txt");
    }

    /// `entry_at(N-1)` が末尾エントリを返すことを守るテスト。
    #[test]
    fn entry_at_last_index_returns_last_entry() {
        let state = AppState {
            entries: vec![
                make_entry("a.txt", false),
                make_entry("b.txt", false),
                make_entry("c.txt", false),
            ],
            ..AppState::default()
        };
        let last_idx = state.entries.len() - 1;
        let entry = state
            .entry_at(last_idx)
            .expect("末尾エントリは存在するはず");
        assert_eq!(entry.name, "c.txt");
    }

    /// `entry_at(N)`（件数と同じインデックス）が `None` を返すことを守るテスト。
    #[test]
    fn entry_at_out_of_bounds_returns_none() {
        let state = AppState {
            entries: vec![make_entry("a.txt", false), make_entry("b.txt", false)],
            ..AppState::default()
        };
        // len() は 2 なので、インデックス 2 は範囲外
        assert!(state.entry_at(2).is_none());
    }

    /// フィルタで件数を絞った後、`entry_at` のインデックスがフィルタ後の並びと整合することを守るテスト。
    #[test]
    fn entry_at_respects_filtered_index() {
        let state = AppState {
            entries: vec![
                make_entry("readme.md", false),
                make_entry("main.rs", false),
                make_entry("lib.rs", false),
                make_entry("build.rs", false),
            ],
            filter: "rs".to_string(),
            ..AppState::default()
        };
        // フィルタ後は ["main.rs", "lib.rs", "build.rs"] の 3 件。
        // インデックス 1 は "lib.rs" のはず。
        let entry = state.entry_at(1).expect("フィルタ後 1 番目は存在するはず");
        assert_eq!(entry.name, "lib.rs");
    }

    /// フィルタを空に戻すと `visible_entries` が全件に戻ることを守るテスト。
    #[test]
    fn clearing_filter_restores_all_entries() {
        // `filter` を途中で変更するため、ここだけ mut が必要
        let mut state = AppState {
            entries: vec![
                make_entry("foo.txt", false),
                make_entry("bar.txt", false),
                make_entry("baz.txt", false),
            ],
            ..AppState::default()
        };

        // まず絞り込む
        state.filter = "foo".to_string();
        let filtered: Vec<_> = state.visible_entries().map(|e| e.name.as_str()).collect();
        assert_eq!(filtered, ["foo.txt"]);

        // フィルタをクリアすると全件に戻る
        state.filter = String::new();
        let all: Vec<_> = state.visible_entries().map(|e| e.name.as_str()).collect();
        assert_eq!(all, ["foo.txt", "bar.txt", "baz.txt"]);
    }
}
