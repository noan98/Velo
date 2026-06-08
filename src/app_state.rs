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
