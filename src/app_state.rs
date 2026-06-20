use std::path::PathBuf;

use crate::fs::entry::FileEntry;
use crate::fs::watcher::DirWatcher;

/// ナビゲーション履歴の上限件数。
///
/// 古くなりすぎた項目は末尾（最も古い側）から切り捨てる。
/// 上限を無制限にするとメモリが膨らみ続けるため、現実的なブラウザに倣い 50 件とする。
const HISTORY_LIMIT: usize = 50;

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
/// Slint 側はこの状態を整形して表示するだけの”表示専用”という役割分担。
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
    /// 戻る履歴スタック（先頭が直前、末尾が最古）。
    ///
    /// 通常ナビゲーション時に現在ディレクトリを push する。
    /// 「戻る」操作で pop して current_dir へ移動する。
    /// 上限 [`HISTORY_LIMIT`] を超えたら末尾（最古）から切り捨てる。
    pub history_back: Vec<PathBuf>,
    /// 進む履歴スタック（先頭が直後、末尾が最新）。
    ///
    /// 「戻る」操作時に現在ディレクトリを push する。
    /// 「進む」操作で pop して current_dir へ移動する。
    /// 通常ナビゲーション時は全件をクリアする。
    pub history_forward: Vec<PathBuf>,
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
            history_back: Vec::new(),
            history_forward: Vec::new(),
        }
    }
}

impl AppState {
    /// ワーカーが返してきた読み込み結果を UI に反映してよいかを判定する。
    ///
    /// 「最後のナビゲーションが勝つ」ガードの核心。`navigate_to` が `current_dir` を
    /// 即座に更新するため、読み込み完了時点で `current_dir` が変わっていれば、その結果は
    /// **古いナビゲーションに対するもの**であり捨てなければならない。
    /// パス比較だけで判断するためフィルタ状態には依存せず、純粋に「目的地が一致するか」を返す。
    pub fn should_apply(&self, path: &std::path::Path) -> bool {
        self.current_dir == path
    }

    /// 通常ナビゲーション（ユーザーが新しい場所へ移動）時の履歴更新。
    ///
    /// 現在ディレクトリを戻る履歴に積み、進む履歴をクリアする。
    /// 上限 [`HISTORY_LIMIT`] を超えたら最古の項目を切り捨てる。
    /// 「戻る/進む」経由の移動では呼ばない（スタック操作のループを防ぐため）。
    pub fn push_history(&mut self, current: PathBuf) {
        self.history_back.insert(0, current);
        if self.history_back.len() > HISTORY_LIMIT {
            self.history_back.truncate(HISTORY_LIMIT);
        }
        self.history_forward.clear();
    }

    /// 「戻る」操作: 戻る履歴スタックから 1 件取り出して移動先を返す。
    ///
    /// 現在ディレクトリは進む履歴に積む。
    /// 戻れる履歴がなければ `None` を返す。
    pub fn pop_back(&mut self) -> Option<PathBuf> {
        if self.history_back.is_empty() {
            return None;
        }
        let dest = self.history_back.remove(0);
        self.history_forward.insert(0, self.current_dir.clone());
        Some(dest)
    }

    /// 「進む」操作: 進む履歴スタックから 1 件取り出して移動先を返す。
    ///
    /// 現在ディレクトリは戻る履歴に積む。
    /// 進める履歴がなければ `None` を返す。
    pub fn pop_forward(&mut self) -> Option<PathBuf> {
        if self.history_forward.is_empty() {
            return None;
        }
        let dest = self.history_forward.remove(0);
        self.history_back.insert(0, self.current_dir.clone());
        Some(dest)
    }

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

    // ---- should_apply のテスト群 ----

    /// `current_dir` と渡したパスが一致するとき `should_apply` が true を返す。
    ///
    /// これが「反映してよい」という最もシンプルなケース。
    #[test]
    fn should_apply_returns_true_when_paths_match() {
        let dir = PathBuf::from("/home/user/documents");
        let state = AppState {
            current_dir: dir.clone(),
            ..AppState::default()
        };
        assert!(state.should_apply(&dir));
    }

    /// `current_dir` と渡したパスが異なるとき `should_apply` が false を返す。
    ///
    /// 古いナビゲーションの結果は捨てなければならない——それがこのガードの存在意義。
    #[test]
    fn should_apply_returns_false_when_paths_differ() {
        let state = AppState {
            current_dir: PathBuf::from("/home/user/documents"),
            ..AppState::default()
        };
        assert!(!state.should_apply(std::path::Path::new("/home/user/downloads")));
    }

    /// 連続ナビゲーション後、最後のパスにだけ true、古いパスには false を返す。
    ///
    /// 「最後のナビゲーションが勝つ」の核心：複数回 current_dir を更新した後は
    /// 最後に設定したパスだけが有効で、それ以前のパスはすべて古い結果とみなす。
    #[test]
    fn should_apply_only_last_navigation_wins() {
        let first = PathBuf::from("/home/user/a");
        let second = PathBuf::from("/home/user/b");
        let last = PathBuf::from("/home/user/c");

        // 最終的に `last` へ移動した状態を作る
        let state = AppState {
            current_dir: last.clone(),
            ..AppState::default()
        };

        // 最後のパスだけ true
        assert!(state.should_apply(&last));
        // 途中のパスはすべて false（古い結果として捨てる）
        assert!(!state.should_apply(&first));
        assert!(!state.should_apply(&second));
    }

    /// フィルタが有効（空でない）なときでも、`should_apply` はパス比較のみで判定する。
    ///
    /// ガードはフィルタ状態とは無関係——「どこを見ているか」だけが判断基準。
    /// 監視きっかけの再読み込みではフィルタを引き継いだまま同じディレクトリを再読みするので、
    /// フィルタが入っていても current_dir が一致すれば `true` でなければならない。
    #[test]
    fn should_apply_ignores_filter_state() {
        let dir = PathBuf::from("/home/user/src");

        // フィルタが有効でも一致すれば true
        let state_with_filter = AppState {
            current_dir: dir.clone(),
            filter: "rs".to_string(),
            ..AppState::default()
        };
        assert!(state_with_filter.should_apply(&dir));

        // フィルタが有効かつパスが違えば false（フィルタは関係なくパスだけで判断）
        let state_wrong_dir = AppState {
            current_dir: PathBuf::from("/home/user/other"),
            filter: "rs".to_string(),
            ..AppState::default()
        };
        assert!(!state_wrong_dir.should_apply(&dir));
    }

    /// `push_history` で戻る履歴が積まれ、進む履歴がクリアされることを守るテスト。
    #[test]
    fn push_history_accumulates_back_and_clears_forward() {
        let mut state = AppState::default();
        // 先に進む履歴を作っておく（直後に push_history でクリアされるか確認するため）。
        state.history_forward.push(PathBuf::from("/forward"));

        state.push_history(PathBuf::from("/a"));
        state.push_history(PathBuf::from("/b"));

        // 新しい履歴が先頭に来る（直前の移動元が先頭）。
        assert_eq!(state.history_back[0], PathBuf::from("/b"));
        assert_eq!(state.history_back[1], PathBuf::from("/a"));
        // 進む履歴はクリアされる。
        assert!(state.history_forward.is_empty());
    }

    /// `push_history` が上限 50 件を超えたら切り捨てることを守るテスト。
    #[test]
    fn push_history_caps_at_limit() {
        let mut state = AppState::default();
        for i in 0..=55usize {
            state.push_history(PathBuf::from(format!("/dir{i}")));
        }
        // 上限 50 件に切り捨てられる。
        assert_eq!(state.history_back.len(), HISTORY_LIMIT);
    }

    /// `pop_back` が直前のディレクトリを返し、現在地を進む履歴に移すことを守るテスト。
    #[test]
    fn pop_back_moves_current_to_forward() {
        let mut state = AppState {
            current_dir: PathBuf::from("/current"),
            ..AppState::default()
        };
        state.history_back.push(PathBuf::from("/prev"));

        let dest = state.pop_back();

        assert_eq!(dest, Some(PathBuf::from("/prev")));
        // 戻る履歴は空になる。
        assert!(state.history_back.is_empty());
        // 現在地が進む履歴の先頭に移る。
        assert_eq!(state.history_forward[0], PathBuf::from("/current"));
    }

    /// 戻る履歴が空のとき `pop_back` が `None` を返すことを守るテスト。
    #[test]
    fn pop_back_returns_none_when_empty() {
        let mut state = AppState::default();
        assert!(state.pop_back().is_none());
    }

    /// `pop_forward` が進むディレクトリを返し、現在地を戻る履歴に移すことを守るテスト。
    #[test]
    fn pop_forward_moves_current_to_back() {
        let mut state = AppState {
            current_dir: PathBuf::from("/current"),
            ..AppState::default()
        };
        state.history_forward.push(PathBuf::from("/next"));

        let dest = state.pop_forward();

        assert_eq!(dest, Some(PathBuf::from("/next")));
        // 進む履歴は空になる。
        assert!(state.history_forward.is_empty());
        // 現在地が戻る履歴の先頭に移る。
        assert_eq!(state.history_back[0], PathBuf::from("/current"));
    }

    /// 進む履歴が空のとき `pop_forward` が `None` を返すことを守るテスト。
    #[test]
    fn pop_forward_returns_none_when_empty() {
        let mut state = AppState::default();
        assert!(state.pop_forward().is_none());
    }
}
