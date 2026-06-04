use std::path::Path;

use super::entry::FileEntry;

/// 指定ディレクトリを読み込み、エントリ一覧を返す。
///
/// **重要:** この関数は `read_dir` と各エントリの `metadata` 取得を行うため I/O が重い。
/// 必ずバックグラウンドスレッドから呼ぶこと（UI スレッドから直接呼ばない）。
///
/// 1 件のメタデータ取得に失敗しても、その 1 件を諦めて残りを返す
/// （アクセス権のないファイルが 1 つあっても一覧全体を止めないため）。
pub fn list_dir(path: &Path) -> std::io::Result<Vec<FileEntry>> {
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

    // 土台としての固定ソート: フォルダを先に、その中で名前順（大文字小文字を無視）。
    // ソート UI は後回しスコープなので、ここで決め打ちにしておく。
    // sort_by_cached_key で小文字化キーを 1 件 1 回だけ計算する（比較ごとの割り当てを避ける）。
    // `!is_dir` は false(0)=フォルダ → true(1)=ファイル の順になり、フォルダが先に来る。
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));

    Ok(entries)
}
