use std::fs::DirEntry;
use std::path::Path;

use rayon::prelude::*;

use super::entry::FileEntry;

/// `metadata()` の並列取得に切り替える件数のしきい値。
///
/// 小規模ディレクトリでは rayon のスレッドプール起動オーバーヘッドが
/// 直列実行より高くつくため、この件数未満は直列で処理する。
/// （値は「数百件で並列化の効果が出始める」という経験則の決め打ち。
///   厳密な最適値は実機計測で詰める余地がある。）
const PARALLEL_THRESHOLD: usize = 256;

/// 指定ディレクトリを読み込み、エントリ一覧を返す。
///
/// **重要:** この関数は `read_dir` と各エントリの `metadata` 取得を行うため I/O が重い。
/// 必ずバックグラウンドスレッドから呼ぶこと（UI スレッドから直接呼ばない）。
///
/// 1 件のメタデータ取得に失敗しても、その 1 件を諦めて残りを返す
/// （アクセス権のないファイルが 1 つあっても一覧全体を止めないため）。
///
/// **大規模ディレクトリ対策:** `metadata()` は 1 件ごとに I/O を伴うため、件数が多いと
/// 直列取得では待ち時間が積み上がる。`PARALLEL_THRESHOLD` 件以上のときは rayon で
/// 各エントリの metadata 取得を並列化し、待ち時間を重ね合わせて短縮する。
/// （並列化するのは metadata 取得のみ。最後のソートは件数が多くても安いので直列のまま。）
pub fn list_dir(path: &Path) -> std::io::Result<Vec<FileEntry>> {
    // まず read_dir でエントリ列挙だけ済ませる（ここは元々軽い）。
    // 列挙中の一過性エラーは握りつぶして次へ（filter_map で Ok のみ拾う）。
    let dents: Vec<DirEntry> = std::fs::read_dir(path)?.filter_map(Result::ok).collect();

    // 重い metadata 取得を、件数に応じて並列／直列で実行する。
    let mut entries: Vec<FileEntry> = if dents.len() >= PARALLEL_THRESHOLD {
        dents.par_iter().map(to_entry).collect()
    } else {
        dents.iter().map(to_entry).collect()
    };

    // 土台としての固定ソート: フォルダを先に、その中で名前順（大文字小文字を無視）。
    // ソート UI は後回しスコープなので、ここで決め打ちにしておく。
    // sort_by_cached_key で小文字化キーを 1 件 1 回だけ計算する（比較ごとの割り当てを避ける）。
    // `!is_dir` は false(0)=フォルダ → true(1)=ファイル の順になり、フォルダが先に来る。
    entries.sort_by_cached_key(|e| (!e.is_dir, e.name.to_lowercase()));

    Ok(entries)
}

/// 列挙済みの `DirEntry` 1 件を、metadata を取得して `FileEntry` に変換する。
///
/// この関数は並列・直列どちらの経路からも呼ばれる純粋な変換なので、
/// 取得失敗時も控えめな既定値で必ず 1 件を返す（残りの一覧を止めないため）。
fn to_entry(dent: &DirEntry) -> FileEntry {
    let name = dent.file_name().to_string_lossy().into_owned();

    // metadata() はシンボリックリンクの先を辿る。取得失敗時は控えめな既定値にする。
    let meta = dent.metadata().ok();
    let is_dir = meta.as_ref().map(|m| m.is_dir()).unwrap_or(false);
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let modified = meta.as_ref().and_then(|m| m.modified().ok());

    FileEntry {
        name,
        path: dent.path(),
        is_dir,
        size,
        modified,
    }
}
