// Velo — Slint + Rust 製の軽量ファイラー（土台）。
//
// 設計の要点（ジュニア向けメモ）:
// * 状態の真実の源は Rust 側（`AppState`）。Slint は表示専用。
// * 重い I/O（read_dir / metadata）は必ずワーカースレッドで実行する。
// * ワーカー → UI への反映は `Weak::upgrade_in_event_loop` 経由のみ。
//   （UI のプロパティ/モデルをワーカースレッドから直接触らない）
// * 一覧は Slint の仮想化 `ListView` + `VecModel<FileRow>` で供給し、
//   更新時は `set_vec` でモデルだけ差し替える（プロパティ全体は作り直さない）。
//
// UI スレッド上の状態は `APP`（thread_local）に集約している。こうすると、
// `upgrade_in_event_loop` に渡すクロージャ（Send 必須でキャプチャに制約がある）からも、
// 同じ UI スレッドで動く各種コールバックからも、同一の状態へ安全にアクセスできる。

mod app_state;
mod fs;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use slint::{Model, ModelRc, SharedString, VecModel, Weak};

use app_state::AppState;
use fs::entry::FileEntry;

// build.rs が ui/app.slint から生成したコード（MainWindow, FileRow）を取り込む。
slint::include_modules!();

thread_local! {
    /// UI スレッド上のアプリ状態。すべての操作はこの 1 箇所を経由する。
    static APP: RefCell<AppState> = RefCell::new(AppState::default());
}

fn main() -> Result<(), slint::PlatformError> {
    let window = MainWindow::new()?;

    // 空の VecModel を一度だけ ListView に接続する。以後は set_vec で中身を入れ替える。
    let model = std::rc::Rc::new(VecModel::<FileRow>::default());
    window.set_rows(ModelRc::from(model));

    // フォルダのダブルクリック: 行インデックスからパスを引き、フォルダなら中に入る。
    window.on_row_double_clicked({
        let weak = window.as_weak();
        move |index| {
            let target = APP.with(|app| {
                app.borrow()
                    .entry_at(index as usize)
                    .filter(|e| e.is_dir)
                    .map(|e| e.path.clone())
            });
            if let Some(path) = target {
                navigate_to(weak.clone(), path);
            }
        }
    });

    // 「上へ」: 親ディレクトリへ移動する（ルートでは何もしない）。
    window.on_go_up({
        let weak = window.as_weak();
        move || {
            let parent = APP.with(|app| {
                app.borrow()
                    .current_dir
                    .parent()
                    .map(Path::to_path_buf)
            });
            if let Some(path) = parent {
                navigate_to(weak.clone(), path);
            }
        }
    });

    // 起動時はホームディレクトリを表示する。取得できなければカレントディレクトリ、
    // それも無ければ "." を使う。current_dir() は絶対パスを返すため、相対パス開始による
    // 親移動/表示パスのぶれを避けられる。
    let start = dirs::home_dir()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    navigate_to(window.as_weak(), start);

    window.run()
}

/// 別ディレクトリへ移動する（ユーザー操作の入口）。
///
/// `current_dir` を即座に更新し（最後に要求された移動が勝つようにするため）、
/// 監視ウォッチャの張り直しも行う。実体の読み込みは `load_dir` がワーカーで行う。
fn navigate_to(weak: Weak<MainWindow>, path: PathBuf) {
    // current_dir は UI スレッドでだけ更新する。ここはコールバック内なので UI スレッド。
    APP.with(|app| app.borrow_mut().current_dir = path.clone());
    load_dir(weak, path, true);
}

/// `path` の内容をワーカースレッドで読み込み、完了後に UI へ反映する。
///
/// `install_watcher` が true のときは（=ユーザー操作による移動）、反映時に
/// そのディレクトリの監視ウォッチャを張り直す。false のとき（=監視による再読み込み）は
/// ウォッチャはそのままに、一覧だけを更新する。
fn load_dir(weak: Weak<MainWindow>, path: PathBuf, install_watcher: bool) {
    // 読み込み中表示を即座に出す（呼び出し元は UI スレッド）。
    if let Some(window) = weak.upgrade() {
        window.set_loading(true);
    }

    std::thread::spawn(move || {
        // --- ここはワーカースレッド。UI には一切触れない ---
        let entries = fs::lister::list_dir(&path).unwrap_or_default();
        let rows = to_rows(&entries);

        // --- 反映だけ UI スレッドへ載せ替える ---
        let _ = weak.upgrade_in_event_loop(move |window| {
            apply_listing(&window, path, entries, rows, install_watcher);
        });
    });
}

/// 読み込み結果を UI に反映する（UI スレッドで実行される）。
fn apply_listing(
    window: &MainWindow,
    path: PathBuf,
    entries: Vec<FileEntry>,
    rows: Vec<FileRow>,
    install_watcher: bool,
) {
    // 「最後に要求された移動が勝つ」ためのガード。
    // 読み込み中に別のディレクトリへ移動されていたら、この古い結果は捨てる。
    let is_current = APP.with(|app| app.borrow().current_dir == path);
    if !is_current {
        return;
    }

    // モデルだけ差し替える（プロパティ全体の作り直しはしない）。
    if let Some(vec_model) = window.get_rows().as_any().downcast_ref::<VecModel<FileRow>>() {
        vec_model.set_vec(rows);
    }

    window.set_current_path(path.to_string_lossy().as_ref().into());
    window.set_loading(false);

    // ユーザー操作による移動のときだけ、監視対象を新ディレクトリへ張り直す。
    let watcher = if install_watcher {
        make_watcher(window.as_weak(), &path)
    } else {
        None
    };

    APP.with(|app| {
        let mut app = app.borrow_mut();
        app.entries = entries;
        if install_watcher {
            app.watcher = watcher;
        }
    });
}

/// `path` を監視するウォッチャを作る。変更があれば（デバウンス後に）一覧を再読み込みする。
///
/// ウォッチャのコールバックは別スレッドで呼ばれるため、`upgrade_in_event_loop` で
/// UI スレッドに載せ替えてから再読み込みを依頼する。
fn make_watcher(weak: Weak<MainWindow>, path: &Path) -> Option<fs::watcher::DirWatcher> {
    let watch_path = path.to_path_buf();
    let result = fs::watcher::watch_dir(path, move || {
        let weak = weak.clone();
        let path = watch_path.clone();
        // ウィンドウが既に閉じている場合は失敗するが、その時は何もしなくてよい。
        let _ = weak.upgrade_in_event_loop(move |window| {
            // 監視きっかけの再読み込みなので watcher は張り直さない（false）。
            load_dir(window.as_weak(), path, false);
        });
    });

    match result {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            eprintln!("failed to watch directory: {error}");
            None
        }
    }
}

/// ドメインのエントリ列を、表示用の `FileRow`（整形済み文字列）に変換する。
/// 整形（サイズ・日時）はワーカースレッド側で済ませ、UI スレッドの仕事を減らす。
fn to_rows(entries: &[FileEntry]) -> Vec<FileRow> {
    entries
        .iter()
        .map(|e| FileRow {
            // アイコンは後回しスコープ。今は種別を表す記号で代用する
            // （将来はバックグラウンドでアイコン取得 + LRU キャッシュに差し替え可能）。
            type_label: if e.is_dir { "📁" } else { "📄" }.into(),
            name: e.name.as_str().into(),
            size: if e.is_dir {
                SharedString::from("-")
            } else {
                format_size(e.size).into()
            },
            modified: format_time(e.modified).into(),
            is_dir: e.is_dir,
        })
        .collect()
}

/// バイト数を人が読みやすい単位に整形する（例: 1536 → "1.5 KB"）。
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

/// 最終更新日時をローカル時刻の "YYYY-MM-DD HH:MM" 形式に整形する。
fn format_time(time: Option<SystemTime>) -> String {
    match time {
        Some(t) => {
            let local: chrono::DateTime<chrono::Local> = t.into();
            local.format("%Y-%m-%d %H:%M").to_string()
        }
        None => "-".to_string(),
    }
}
