use std::path::PathBuf;
use std::time::SystemTime;

/// ファイル / フォルダ 1 件を表すドメインモデル。
///
/// ここには「表示用の整形」（サイズ文字列や日時文字列）は持たせない。
/// 整形は UI 層に変換するときに行い、このモデルは生のデータだけを保持する。
/// こうしておくと、ソートやフィルタなどの将来機能を素の値で実装できる。
#[derive(Clone, Debug)]
pub struct FileEntry {
    /// 表示名（ファイル名のみ。親パスは含まない）。
    pub name: String,
    /// 絶対パス。ナビゲーションやファイル操作の「真実の源」。
    pub path: PathBuf,
    /// ディレクトリなら true。
    pub is_dir: bool,
    /// バイト単位のサイズ。ディレクトリでは意味がないので 0 を入れる。
    pub size: u64,
    /// 最終更新日時。取得に失敗した場合は None。
    pub modified: Option<SystemTime>,
}
