use std::cmp::Ordering;
use std::path::Path;

use super::entry::FileEntry;
use crate::app_state::SortColumn;

/// 指定ディレクトリを読み込み、`sort` / `ascending` に従って並べたエントリ一覧を返す。
///
/// **重要:** この関数は `read_dir` と各エントリの `metadata` 取得を行うため I/O が重い。
/// 必ずバックグラウンドスレッドから呼ぶこと（UI スレッドから直接呼ばない）。
///
/// 1 件のメタデータ取得に失敗しても、その 1 件を諦めて残りを返す
/// （アクセス権のないファイルが 1 つあっても一覧全体を止めないため）。
pub fn list_dir(path: &Path, sort: SortColumn, ascending: bool) -> std::io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();

    for dent in std::fs::read_dir(path)? {
        let dent = match dent {
            Ok(d) => d,
            Err(_) => continue, // 列挙中の一過性エラーは握りつぶして次へ。
        };

        let name = dent.file_name().to_string_lossy().into_owned();

        // metadata() はシンボリックリンクの先を辿る。取得失敗時は控えめな既定値にする。
        let meta = dent.metadata().ok();
        let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
        let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
        let modified = meta.as_ref().and_then(|m| m.modified().ok());

        entries.push(FileEntry {
            name,
            path: dent.path(),
            is_dir,
            size,
            modified,
        });
    }

    // フォルダは常に先頭へ集める（Explorer 風。昇順/降順の切替に関わらず固定）。
    // その上で、指定列・指定方向でソートする。
    //
    // 名前の小文字化は比較のたびに再計算するとコストが嵩むため、ここで 1 件 1 回だけ
    // 計算してキーとして持ち回る（デコレート → ソート → アンデコレート）。
    let mut keyed: Vec<(FileEntry, String)> = entries
        .into_iter()
        .map(|e| {
            let name_key = e.name.to_lowercase();
            (e, name_key)
        })
        .collect();

    keyed.sort_by(|(a, a_name), (b, b_name)| {
        // フォルダ優先はソート方向に依らず固定。is_dir=true を先頭側にする。
        let dir_order = b.is_dir.cmp(&a.is_dir);
        if dir_order != Ordering::Equal {
            return dir_order;
        }
        let ord = match sort {
            SortColumn::Name => a_name.cmp(b_name),
            SortColumn::Size => a.size.cmp(&b.size),
            SortColumn::Modified => a.modified.cmp(&b.modified),
        };
        if ascending {
            ord
        } else {
            ord.reverse()
        }
    });

    Ok(keyed.into_iter().map(|(entry, _)| entry).collect())
}
