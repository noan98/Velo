use std::cmp::Ordering;
use std::fs::DirEntry;
use std::path::Path;

use rayon::prelude::*;

use super::entry::FileEntry;
use crate::app_state::SortColumn;

/// `metadata()` の並列取得に切り替える件数のしきい値。
///
/// 小規模ディレクトリでは rayon のスレッドプール起動オーバーヘッドが
/// 直列実行より高くつくため、この件数未満は直列で処理する。
/// （値は「数百件で並列化の効果が出始める」という経験則の決め打ち。
///   厳密な最適値は実機計測で詰める余地がある。）
const PARALLEL_THRESHOLD: usize = 256;

/// 指定ディレクトリを読み込み、`sort` / `ascending` に従って並べたエントリ一覧を返す。
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
pub fn list_dir(path: &Path, sort: SortColumn, ascending: bool) -> std::io::Result<Vec<FileEntry>> {
    // まず read_dir でエントリ列挙だけ済ませる（ここは元々軽い）。
    // 列挙中の一過性エラーは握りつぶして次へ（filter_map で Ok のみ拾う）。
    let dents: Vec<DirEntry> = std::fs::read_dir(path)?.filter_map(Result::ok).collect();

    // 重い metadata 取得を、件数に応じて並列／直列で実行する。
    let entries: Vec<FileEntry> = if dents.len() >= PARALLEL_THRESHOLD {
        dents.par_iter().map(to_entry).collect()
    } else {
        dents.iter().map(to_entry).collect()
    };

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // テストの既定ソート（名前・昇順）を短く書くためのヘルパ。
    fn list_by_name(path: &Path) -> std::io::Result<Vec<FileEntry>> {
        list_dir(path, SortColumn::Name, true)
    }

    /// 存在しないパスは `read_dir` 段階で失敗し、`Err` がそのまま伝播する。
    /// （読み込み失敗をサイレントに握りつぶさず UI へ出す、という挙動の前提）。
    #[test]
    fn list_dir_nonexistent_path_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(list_by_name(&missing).is_err());
    }

    /// フォルダはファイルより必ず前。さらにそれぞれ名前昇順で並ぶ。
    #[test]
    fn list_dir_sorts_folders_before_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("zzz_folder")).unwrap();
        fs::create_dir(dir.path().join("mmm_folder")).unwrap();
        fs::write(dir.path().join("aaa_file.txt"), b"hi").unwrap();
        fs::write(dir.path().join("bbb_file.txt"), b"hi").unwrap();

        let entries = list_by_name(dir.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(
            names,
            ["mmm_folder", "zzz_folder", "aaa_file.txt", "bbb_file.txt"]
        );
        assert!(entries[0].is_dir);
        assert!(entries[1].is_dir);
        assert!(!entries[2].is_dir);
        assert!(!entries[3].is_dir);
    }

    /// 名前ソートは大文字小文字を無視する（"apple" < "Banana" < "Cherry"）。
    /// バイト順だと大文字が先に来てしまうため、ここが崩れていないかを守る。
    #[test]
    fn list_dir_name_sort_is_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("Banana"), b"x").unwrap();
        fs::write(dir.path().join("apple"), b"x").unwrap();
        fs::write(dir.path().join("Cherry"), b"x").unwrap();

        let entries = list_by_name(dir.path()).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(names, ["apple", "Banana", "Cherry"]);
    }

    /// 降順でも「フォルダ優先」は固定。フォルダ群・ファイル群の各内部だけが逆順になる。
    #[test]
    fn list_dir_descending_keeps_folders_first() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a_folder")).unwrap();
        fs::create_dir(dir.path().join("b_folder")).unwrap();
        fs::write(dir.path().join("a_file"), b"x").unwrap();
        fs::write(dir.path().join("b_file"), b"x").unwrap();

        let entries = list_dir(dir.path(), SortColumn::Name, false).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        // フォルダが先（内部は降順）→ ファイル（内部は降順）。
        assert_eq!(names, ["b_folder", "a_folder", "b_file", "a_file"]);
    }

    /// サイズ列の昇順ソート。フォルダは先頭固定なので、ファイルだけがサイズ順に並ぶ。
    #[test]
    fn list_dir_sorts_files_by_size_ascending() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big"), vec![0u8; 300]).unwrap();
        fs::write(dir.path().join("small"), vec![0u8; 10]).unwrap();
        fs::write(dir.path().join("medium"), vec![0u8; 100]).unwrap();

        let entries = list_dir(dir.path(), SortColumn::Size, true).unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();

        assert_eq!(names, ["small", "medium", "big"]);
    }

    /// `FileEntry` に生のメタデータ（種別・サイズ・絶対パス・更新時刻）が入る。
    #[test]
    fn list_dir_populates_entry_metadata() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("data.bin"), b"0123456789").unwrap();

        let entries = list_by_name(dir.path()).unwrap();
        let entry = entries.iter().find(|e| e.name == "data.bin").unwrap();

        assert!(!entry.is_dir);
        assert_eq!(entry.size, 10);
        assert_eq!(entry.path, dir.path().join("data.bin"));
        assert!(entry.modified.is_some());
    }

    /// 空ディレクトリは空ベクタを返す（エラーではない）。
    #[test]
    fn list_dir_empty_directory_is_ok_and_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_by_name(dir.path()).unwrap().is_empty());
    }

    /// 権限のないディレクトリは `Err` を返す。
    ///
    /// ただし root はパーミッションビットを無視して読めてしまい前提が崩れるため、
    /// 標準の `read_dir` でも読める（=特権）環境では検証をスキップする。
    #[cfg(unix)]
    #[test]
    fn list_dir_permission_denied_returns_error() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        fs::create_dir(&locked).unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let denied_for_us = fs::read_dir(&locked).is_err();
        let result = list_by_name(&locked);

        // 後始末: tempdir が消せるように権限を戻す。
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));

        if denied_for_us {
            assert!(result.is_err(), "権限のないディレクトリは Err を返すべき");
        } else {
            // 特権実行（root 等）では読めてしまうので Ok でも問題なし。
            assert!(result.is_ok());
        }
    }
}
