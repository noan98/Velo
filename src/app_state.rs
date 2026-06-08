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
    /// 現在表示中のエントリ一覧（行インデックス → パス解決に使う）。
    pub entries: Vec<FileEntry>,
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
            watcher: None,
            sort_column: SortColumn::Name,
            sort_ascending: true,
        }
    }
}

impl AppState {
    /// 行インデックスからエントリを引く（範囲外なら None）。
    pub fn entry_at(&self, index: usize) -> Option<&FileEntry> {
        self.entries.get(index)
    }
}
