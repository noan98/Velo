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

use app_state::{AppState, SortColumn};
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

    // ソートの初期状態（名前・昇順）を UI のインジケータへ反映する。
    window.set_sort_column(sort_column_to_int(SortColumn::Name));
    window.set_sort_ascending(true);

    // フォルダのダブルクリック: 行インデックスからパスを引き、フォルダなら中に入る。
    window.on_row_double_clicked({
        let weak = window.as_weak();
        move |index| activate_row(weak.clone(), index)
    });

    // Enter での実行: 選択中の行がフォルダなら中に入る（ダブルクリックと同じ挙動）。
    window.on_row_activated({
        let weak = window.as_weak();
        move |index| activate_row(weak.clone(), index)
    });

    // 「上へ」: 親ディレクトリへ移動する（ルートでは何もしない）。
    window.on_go_up({
        let weak = window.as_weak();
        move || {
            let parent = APP.with(|app| app.borrow().current_dir.parent().map(Path::to_path_buf));
            if let Some(path) = parent {
                navigate_to(weak.clone(), path);
            }
        }
    });

    // ヘッダクリックでのソート切替。同じ列の再クリックで昇順/降順をトグルし、
    // 別列に切り替えたときは昇順から始める。
    window.on_sort_changed({
        let weak = window.as_weak();
        move |column| {
            let new_column = sort_column_from_int(column);
            let (column, ascending) = APP.with(|app| {
                let mut app = app.borrow_mut();
                if app.sort_column == new_column {
                    app.sort_ascending = !app.sort_ascending;
                } else {
                    app.sort_column = new_column;
                    app.sort_ascending = true;
                }
                (app.sort_column, app.sort_ascending)
            });
            if let Some(window) = weak.upgrade() {
                window.set_sort_column(sort_column_to_int(column));
                window.set_sort_ascending(ascending);
                // 並び順が変わったので選択はリセットし、現在ディレクトリを読み直す。
                // 同じディレクトリのままなのでウォッチャは張り直さない（false）。
                window.set_selected_index(-1);
                let path = APP.with(|app| app.borrow().current_dir.clone());
                load_dir(window.as_weak(), path, false);
            }
        }
    });

    // ダーク/ライトの手動切替。テーマはグローバルとして UI 側に持ち、ここでは反転だけ行う。
    window.on_toggle_theme({
        let weak = window.as_weak();
        move || {
            if let Some(window) = weak.upgrade() {
                let theme = Theme::get(&window);
                theme.set_dark_mode(!theme.get_dark_mode());
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

/// 行を「実行」する（ダブルクリック / Enter 共通の入口）。
///
/// 行インデックスからエントリを引き、フォルダのときだけその中へ移動する。
/// ファイルを開く操作は土台では未対応（後回しスコープ）。
fn activate_row(weak: Weak<MainWindow>, index: i32) {
    let target = APP.with(|app| {
        app.borrow()
            .entry_at(index as usize)
            .filter(|e| e.is_dir)
            .map(|e| e.path.clone())
    });
    if let Some(path) = target {
        navigate_to(weak, path);
    }
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

    // ソート条件は UI スレッド側の状態から取り出してワーカーへ渡す
    // （ワーカーは APP thread_local に触れないため、ここで読んでムーブする）。
    let (sort, ascending) = APP.with(|app| {
        let app = app.borrow();
        (app.sort_column, app.sort_ascending)
    });

    std::thread::spawn(move || {
        // --- ここはワーカースレッド。UI には一切触れない ---
        // 失敗は握りつぶさず、エラー文言を持ち帰って UI に出す（サイレント失敗の防止）。
        let (entries, rows, error) = match fs::lister::list_dir(&path, sort, ascending) {
            Ok(entries) => {
                let rows = to_rows(&entries);
                (entries, rows, None)
            }
            Err(e) => (
                Vec::new(),
                Vec::new(),
                Some(format!("読み込みに失敗しました: {e}")),
            ),
        };

        // --- 反映だけ UI スレッドへ載せ替える ---
        let _ = weak.upgrade_in_event_loop(move |window| {
            apply_listing(&window, path, entries, rows, error, install_watcher);
        });
    });
}

/// 読み込み結果を UI に反映する（UI スレッドで実行される）。
fn apply_listing(
    window: &MainWindow,
    path: PathBuf,
    entries: Vec<FileEntry>,
    rows: Vec<FileRow>,
    error: Option<String>,
    install_watcher: bool,
) {
    // 「最後に要求された移動が勝つ」ためのガード。
    // 読み込み中に別のディレクトリへ移動されていたら、この古い結果は捨てる。
    let is_current = APP.with(|app| app.borrow().current_dir == path);
    if !is_current {
        return;
    }

    // モデルだけ差し替える（プロパティ全体の作り直しはしない）。
    if let Some(vec_model) = window
        .get_rows()
        .as_any()
        .downcast_ref::<VecModel<FileRow>>()
    {
        vec_model.set_vec(rows);
    }

    window.set_current_path(path.to_string_lossy().as_ref().into());
    window.set_loading(false);

    // エラー文言を反映（成功時は空文字でクリアし、ステータスバーを隠す）。
    window.set_error_message(error.unwrap_or_default().as_str().into());

    // ユーザー操作による移動のときは、新ディレクトリでは選択を一旦解除する。
    if install_watcher {
        window.set_selected_index(-1);
    }

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
    let change_weak = weak.clone();
    let error_weak = weak.clone();
    let result = fs::watcher::watch_dir(
        path,
        move || {
            let weak = change_weak.clone();
            let path = watch_path.clone();
            // ウィンドウが既に閉じている場合は失敗するが、その時は何もしなくてよい。
            let _ = weak.upgrade_in_event_loop(move |window| {
                // 監視きっかけの再読み込みなので watcher は張り直さない（false）。
                load_dir(window.as_weak(), path, false);
            });
        },
        move |message| {
            // 監視中エラーはターミナルに埋もれさせず、ステータスバーへ可視化する。
            let _ = error_weak.upgrade_in_event_loop(move |window| {
                window.set_error_message(format!("監視エラー: {message}").as_str().into());
            });
        },
    );

    match result {
        Ok(watcher) => Some(watcher),
        Err(error) => {
            // ウォッチャの作成自体に失敗した場合も UI に出す（監視が効かないことを伝える）。
            if let Some(window) = weak.upgrade() {
                window.set_error_message(format!("監視を開始できません: {error}").as_str().into());
            }
            None
        }
    }
}

/// ドメインのエントリ列を、表示用の `FileRow`（整形済み文字列）に変換する。
/// 整形（サイズ・日時・アイコン）はワーカースレッド側で済ませ、UI スレッドの仕事を減らす。
fn to_rows(entries: &[FileEntry]) -> Vec<FileRow> {
    entries
        .iter()
        .map(|e| FileRow {
            type_label: icon_for_entry(e).into(),
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

/// エントリの種別を表す絵文字を返す（表示用ラベル）。
///
/// 生データ（`FileEntry`）にアイコン項目を持たせるのではなく、表示へ変換するこの場所で
/// 拡張子から決める。SVG アイコン化（フェーズ 2）も、ここを差し替えるだけで移行できる。
/// 拡張子は大文字小文字を無視するため、ASCII 小文字に正規化してから判定する。
fn icon_for_entry(entry: &FileEntry) -> &'static str {
    if entry.is_dir {
        return "📁";
    }
    let ext = entry
        .path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("rs") => "🦀",
        Some("toml" | "json" | "yaml" | "yml") => "⚙",
        Some("png" | "jpg" | "jpeg" | "gif" | "svg" | "webp" | "bmp") => "🖼",
        Some("mp4" | "mov" | "avi" | "mkv" | "webm") => "🎬",
        Some("mp3" | "flac" | "wav" | "ogg" | "m4a") => "🎵",
        Some("zip" | "tar" | "gz" | "7z" | "rar") => "📦",
        Some("pdf") => "📑",
        Some("md" | "txt") => "📝",
        Some("exe" | "msi") => "🖥",
        _ => "📄",
    }
}

/// Slint から渡る列識別子（int）を `SortColumn` に変換する。未知の値は名前列に倒す。
fn sort_column_from_int(value: i32) -> SortColumn {
    match value {
        1 => SortColumn::Size,
        2 => SortColumn::Modified,
        _ => SortColumn::Name,
    }
}

/// `SortColumn` を Slint へ渡す列識別子（int）に変換する。
fn sort_column_to_int(column: SortColumn) -> i32 {
    match column {
        SortColumn::Name => 0,
        SortColumn::Size => 1,
        SortColumn::Modified => 2,
    }
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
