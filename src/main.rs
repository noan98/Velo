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
    // フィルタ変更時に直接モデルを差し替えられるよう、ハンドルを 1 つ手元にも残す。
    let model = std::rc::Rc::new(VecModel::<FileRow>::default());
    window.set_rows(ModelRc::from(model.clone()));

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

    // 「戻る」: 直前のディレクトリへ戻る（戻る履歴がなければ何もしない）。
    window.on_go_back({
        let weak = window.as_weak();
        move || navigate_back(weak.clone())
    });

    // 「進む」: 戻った後にひとつ進む（進む履歴がなければ何もしない）。
    window.on_go_forward({
        let weak = window.as_weak();
        move || navigate_forward(weak.clone())
    });

    // アドレスバーに入力されたパスへ直接ジャンプする（Enter で確定）。
    // ディレクトリならそこへ、ファイルなら親ディレクトリへ移動。相対パスは
    // 現在ディレクトリ基準で解決し、存在しなければエラー表示を出す。
    window.on_path_submitted({
        let weak = window.as_weak();
        move |text| {
            let input = text.trim();
            if input.is_empty() {
                return;
            }

            // 相対パスは現在ディレクトリ基準で絶対パスへ解決する。
            let base = APP.with(|app| app.borrow().current_dir.clone());
            let raw = PathBuf::from(input);
            let resolved = if raw.is_relative() {
                base.join(raw)
            } else {
                raw
            };

            // ディレクトリ → そこへ / ファイル → 親へ / それ以外 → エラー。
            let target = if resolved.is_dir() {
                Some(resolved)
            } else if resolved.is_file() {
                resolved.parent().map(Path::to_path_buf)
            } else {
                None
            };

            match target {
                Some(dir) => navigate_to(weak.clone(), dir),
                None => {
                    // 解決できなかった旨をアドレスバーのエラー表示で知らせる。
                    if let Some(window) = weak.upgrade() {
                        window.set_address_error(true);
                    }
                }
            }
        }
    });

    // 検索バー: 現在ディレクトリ内のファイル名をインクリメンタルに絞り込む。
    // I/O は伴わず、保持済みの全エントリ（AppState.entries）から一致分だけを
    // UI スレッドで再構築してモデルに反映する（体感的に瞬時）。
    // フィルタで表示件数が変わるため、選択はリセットしてズレを防ぐ。
    // total-count はフィルタ前の全件数を保持するため、ここでは変更しない（set_vec のみ）。
    window.on_filter_changed({
        let weak = window.as_weak();
        let model = model.clone();
        move |text| {
            let rows = APP.with(|app| {
                let mut app = app.borrow_mut();
                app.filter = text.to_string();
                to_rows_from(app.visible_entries())
            });
            model.set_vec(rows);
            if let Some(window) = weak.upgrade() {
                window.set_selected_index(-1);
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

    // #50: タイプアヘッド選択。印字可能 1 文字と現在の選択インデックスを受け取り、
    // 前方一致エントリの表示インデックスを返す。一致なしのときは -1 を返す。
    // Slint 側で selected-index の更新とスクロール位置の調整を行うので、
    // ここでは AppState.typeahead_match の結果をそのまま返すだけでよい。
    window.on_typeahead({
        move |ch, current_selected| {
            APP.with(|app| {
                let mut app = app.borrow_mut();
                match app.typeahead_match(&ch, current_selected) {
                    Some(idx) => idx as i32,
                    None => -1,
                }
            })
        }
    });

    // ブレッドクラムのセグメントがクリックされたとき、その階層のパスへ移動する（#49）。
    // セグメントリストの先頭から i+1 個を結合してパスを構築し、navigate_to へ渡す。
    // Windows のドライブ表記（"C:" 等）と残りのコンポーネントを PathBuf で組み立てる。
    window.on_segment_clicked({
        let weak = window.as_weak();
        move |index| {
            // セグメント文字列を APP から取得するのではなく、現在の current_dir を
            // Path::components() で分解して先頭 index+1 個を結合する。
            // こうすると Slint 側の文字列と Rust 側のパス表現がずれるリスクを避けられる。
            let path = APP.with(|app| {
                let app = app.borrow();
                build_path_from_components(&app.current_dir, index as usize)
            });
            if let Some(p) = path {
                navigate_to(weak.clone(), p);
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
/// 行インデックスからエントリを引き、フォルダなら中へ移動し、
/// ファイルなら OS のデフォルトアプリで開く。
/// ファイルを開く処理はワーカースレッドで実行し、UI スレッドをブロックしない。
/// 失敗時は `upgrade_in_event_loop` で UI スレッドに載せ替えてエラーを表示する。
fn activate_row(weak: Weak<MainWindow>, index: i32) {
    // エントリから is_dir とパスを同時に取り出す。範囲外なら何もしない。
    let entry = APP.with(|app| {
        app.borrow()
            .entry_at(index as usize)
            .map(|e| (e.is_dir, e.path.clone()))
    });

    let Some((is_dir, path)) = entry else {
        return;
    };

    if is_dir {
        // フォルダ → 従来どおりナビゲーション。
        navigate_to(weak, path);
    } else {
        // ファイル → OS デフォルトアプリで開く。I/O なのでワーカーへ移す。
        std::thread::spawn(move || {
            if let Err(e) = open::that(&path) {
                // 失敗を UI スレッドへ持ち帰ってステータスバーに表示する。
                let message = format!("開けませんでした: {e}");
                let _ = weak.upgrade_in_event_loop(move |window| {
                    window.set_error_message(message.as_str().into());
                });
            }
        });
    }
}

/// 別ディレクトリへ移動する（ユーザー操作の入口）。
///
/// 現在ディレクトリを戻る履歴に積み、進む履歴をクリアしてから `current_dir` を更新する。
/// 監視ウォッチャの張り直しも行う。実体の読み込みは `load_dir` がワーカーで行う。
///
/// 「戻る/進む」経由の移動は別関数（`navigate_back`/`navigate_forward`）を使い、
/// この関数を通らないことでスタック操作のループを防ぐ。
fn navigate_to(weak: Weak<MainWindow>, path: PathBuf) {
    // current_dir は UI スレッドでだけ更新する。ここはコールバック内なので UI スレッド。
    // 別ディレクトリへ移ったらフィルタは引き継がず解除する（新しい場所の全件を見せる）。
    APP.with(|app| {
        let mut app = app.borrow_mut();
        // 現在地を戻る履歴に積む（空パス＝起動直後は積まない）。
        // 同一ディレクトリへの再移動（アドレスバー再入力・末尾セグメントのクリック等）では
        // 履歴に重複を積まないよう、移動先と現在地が異なるときだけ積む。
        let prev = app.current_dir.clone();
        if prev != PathBuf::new() && prev != path {
            app.push_history(prev);
        }
        app.current_dir = path.clone();
        app.filter.clear();
    });

    // 検索バー・アドレスバーのエラー表示も、移動に合わせてリセットする。
    if let Some(window) = weak.upgrade() {
        window.set_filter_text(SharedString::new());
        window.set_address_error(false);
        update_history_buttons(&window);
    }

    load_dir(weak, path, true);
}

/// 「戻る」操作専用の移動関数。
///
/// `navigate_to` は呼ばず（履歴を通常ナビゲーションとして積まないよう）、
/// `AppState::pop_back` でスタック操作だけ行ってから `load_dir` を呼ぶ。
fn navigate_back(weak: Weak<MainWindow>) {
    let dest = APP.with(|app| app.borrow_mut().pop_back());
    let Some(path) = dest else {
        return;
    };

    APP.with(|app| {
        let mut app = app.borrow_mut();
        app.current_dir = path.clone();
        app.filter.clear();
    });

    if let Some(window) = weak.upgrade() {
        window.set_filter_text(SharedString::new());
        window.set_address_error(false);
        update_history_buttons(&window);
    }

    load_dir(weak, path, true);
}

/// 「進む」操作専用の移動関数。
///
/// `navigate_to` は呼ばず（履歴を通常ナビゲーションとして積まないよう）、
/// `AppState::pop_forward` でスタック操作だけ行ってから `load_dir` を呼ぶ。
fn navigate_forward(weak: Weak<MainWindow>) {
    let dest = APP.with(|app| app.borrow_mut().pop_forward());
    let Some(path) = dest else {
        return;
    };

    APP.with(|app| {
        let mut app = app.borrow_mut();
        app.current_dir = path.clone();
        app.filter.clear();
    });

    if let Some(window) = weak.upgrade() {
        window.set_filter_text(SharedString::new());
        window.set_address_error(false);
        update_history_buttons(&window);
    }

    load_dir(weak, path, true);
}

/// 「戻る」「進む」ボタンの `enabled` 状態を現在の履歴スタックに合わせて更新する。
///
/// 履歴が空のときはボタンを無効化し、誤操作を防ぐ。
/// ナビゲーション操作のたびに呼んで UI と状態を同期させる。
fn update_history_buttons(window: &MainWindow) {
    let (can_back, can_forward) = APP.with(|app| {
        let app = app.borrow();
        (
            !app.history_back.is_empty(),
            !app.history_forward.is_empty(),
        )
    });
    window.set_can_go_back(can_back);
    window.set_can_go_forward(can_forward);
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
    // ガードのロジックは AppState::should_apply に集約し、ここでは呼び出すだけにする。
    if !APP.with(|app| app.borrow().should_apply(&path)) {
        return;
    }

    // ユーザー操作による移動のときだけ、監視対象を新ディレクトリへ張り直す。
    let watcher = if install_watcher {
        make_watcher(window.as_weak(), &path)
    } else {
        None
    };

    // エントリを保存し、現在のフィルタを適用した表示行を組み立てる。
    // フィルタが空なら（ユーザー操作直後はここに来る）ワーカーが整形済みの rows を
    // そのまま使う。フィルタ有効時（監視きっかけの再読み込み等）は一致分だけ作り直す。
    // 合わせて全件数（フィルタ前）を total_count として返し、ステータスバーへ渡す。
    let (display_rows, total_count) = APP.with(|app| {
        let mut app = app.borrow_mut();
        app.entries = entries;
        if install_watcher {
            app.watcher = watcher;
        }
        let total = app.entries.len();
        let rows = if app.filter.is_empty() {
            rows
        } else {
            to_rows_from(app.visible_entries())
        };
        (rows, total)
    });

    // モデルだけ差し替える（プロパティ全体の作り直しはしない）。
    if let Some(vec_model) = window
        .get_rows()
        .as_any()
        .downcast_ref::<VecModel<FileRow>>()
    {
        vec_model.set_vec(display_rows);
    }

    // フィルタ前の全件数をステータスバーへ渡す（フィルタ中は "N 件（全 M 件中）" と表示される）。
    window.set_total_count(total_count as i32);
    window.set_current_path(path.to_string_lossy().as_ref().into());

    // ブレッドクラム用のパスセグメントを生成して Slint へ渡す（#49）。
    // Path::components() でセグメントに分解し、表示用文字列に変換する。
    // 例: "C:\Users\user\Documents" → ["C:", "Users", "user", "Documents"]
    let segments = path_to_segments(&path);
    let segment_model = std::rc::Rc::new(VecModel::<SharedString>::from(segments));
    window.set_path_segments(ModelRc::from(segment_model));

    // ユーザー操作による移動のときは、アドレスバーの編集テキストを新パスへ合わせ、
    // 新ディレクトリでは選択を一旦解除する。
    // 監視きっかけの再読み込みでは（同じディレクトリなので）どちらも触らず、
    // 編集中のアドレス入力や選択行を壊さない。
    if install_watcher {
        window.set_address_bar_text(path.to_string_lossy().as_ref().into());
        window.set_selected_index(-1);
    }
    // エラー文言を反映（成功時は空文字でクリアし、ステータスバーを隠す）。
    window.set_error_message(error.unwrap_or_default().as_str().into());
    window.set_loading(false);
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
            // あわせて、死んだウォッチャを生かしたままにする“サイレント停止”を避けるため、
            // AppState から破棄する。以降イベントは届かない前提を状態にも反映しておく
            // （次のユーザー操作による移動で、新ディレクトリのウォッチャが張り直される）。
            let _ = error_weak.upgrade_in_event_loop(move |window| {
                APP.with(|app| app.borrow_mut().watcher = None);
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
    entries.iter().map(entry_to_row).collect()
}

/// `to_rows` のイテレータ版。フィルタ適用後の部分集合から行を作るのに使う
/// （`AppState::visible_entries` をそのまま流し込めるようにするため）。
fn to_rows_from<'a>(entries: impl Iterator<Item = &'a FileEntry>) -> Vec<FileRow> {
    entries.map(entry_to_row).collect()
}

/// エントリ 1 件を表示用 `FileRow`（整形済み文字列）へ変換する。
fn entry_to_row(e: &FileEntry) -> FileRow {
    FileRow {
        // アイコンは拡張子から決める（icon_for_entry）。SVG 化も差し替えで移行可能。
        type_label: icon_for_entry(e).into(),
        name: e.name.as_str().into(),
        size: if e.is_dir {
            SharedString::from("-")
        } else {
            format_size(e.size).into()
        },
        modified: format_time(e.modified).into(),
        is_dir: e.is_dir,
    }
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

/// パスを表示用セグメント列に分解する（ブレッドクラム用、#49）。
///
/// `Path::components()` で各部分を取り出し、表示に適した文字列に変換する。
/// Windows では "C:\" は Prefix + RootDir に分かれるため、ドライブ名（"C:"）だけを
/// 先頭に置き、以降のコンポーネントはフォルダ名として並べる。
/// Linux/macOS の絶対パスでは RootDir が "/" になるため先頭は "/" を出す。
fn path_to_segments(path: &Path) -> Vec<SharedString> {
    use std::path::Component;
    let mut segments: Vec<SharedString> = Vec::new();
    for component in path.components() {
        match component {
            // Windows: "C:\" → Prefix は "C:" として先頭へ。RootDir（"\"）は飲み込む。
            Component::Prefix(prefix) => {
                let s = prefix.as_os_str().to_string_lossy().into_owned();
                segments.push(SharedString::from(s));
            }
            // RootDir は単独 "/" のみの場合（Unix ルート）に先頭へ追加。
            // Windows の "\" は Prefix の直後に続くだけなので、Prefix がない場合のみ追加。
            Component::RootDir => {
                if segments.is_empty() {
                    segments.push(SharedString::from("/"));
                }
            }
            Component::Normal(name) => {
                segments.push(SharedString::from(name.to_string_lossy().as_ref()));
            }
            // "." や ".." は normalize 済みとして無視する（PathBuf は通常解決済み）。
            Component::CurDir | Component::ParentDir => {}
        }
    }
    segments
}

/// ブレッドクラムのインデックス `i` に対応するパスを構築する（#49）。
///
/// `current_dir` を `components()` で分解し、先頭から `i+1` 個を結合して PathBuf を返す。
/// インデックスが範囲外の場合は `None`。
fn build_path_from_components(current_dir: &Path, index: usize) -> Option<PathBuf> {
    use std::path::Component;
    // コンポーネントを収集（RootDir は Prefix の直後に続くため両方を保持する）。
    let components: Vec<Component<'_>> = current_dir.components().collect();
    // RootDir を Prefix にマージした「論理セグメント」列を作る。
    // Prefix + RootDir の連続は 1 セグメント扱いとし、残りは Normal が 1 セグメント。
    let mut logical: Vec<Vec<Component<'_>>> = Vec::new();
    let mut i_comp = 0usize;
    while i_comp < components.len() {
        match &components[i_comp] {
            Component::Prefix(_) => {
                // Prefix に続く RootDir があれば同じセグメントにまとめる。
                let mut seg = vec![components[i_comp]];
                if i_comp + 1 < components.len() {
                    if let Component::RootDir = &components[i_comp + 1] {
                        seg.push(components[i_comp + 1]);
                        i_comp += 1;
                    }
                }
                logical.push(seg);
            }
            Component::RootDir => {
                // Unix ルート "/" は単独セグメント。
                logical.push(vec![components[i_comp]]);
            }
            Component::Normal(_) => {
                logical.push(vec![components[i_comp]]);
            }
            // path_to_segments と同様に "." / ".." はセグメントに数えない。
            // 両関数で扱いを揃えることで、正規化されていないパスでもセグメント数がずれない。
            Component::CurDir | Component::ParentDir => {}
        }
        i_comp += 1;
    }

    if index >= logical.len() {
        return None;
    }

    // 先頭から index+1 個の論理セグメントを PathBuf に変換する。
    let mut result = PathBuf::new();
    for group in &logical[..=index] {
        for comp in group {
            result.push(comp);
        }
    }
    Some(result)
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

// 表示用フォーマッタのユニットテスト。UI（Slint）には触れないため、ヘッドレス環境でも走る。
#[cfg(test)]
mod tests {
    use super::{format_size, format_time, sort_column_from_int, sort_column_to_int, SortColumn};
    use std::time::SystemTime;

    #[test]
    fn format_size_bytes_no_unit_scaling() {
        // 1024 未満は素のバイト表記。境界の 1023 まで "B" のまま。
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1), "1 B");
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(1023), "1023 B");
    }

    #[test]
    fn format_size_kilobytes() {
        // ちょうど 1024 から KB に繰り上がる。小数第 1 位まで表示。
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1536), "1.5 KB");
    }

    #[test]
    fn format_size_megabytes() {
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(1024 * 1024 * 3 / 2), "1.5 MB");
    }

    #[test]
    fn format_size_gigabytes_and_terabytes() {
        assert_eq!(format_size(1024u64.pow(3)), "1.0 GB");
        assert_eq!(format_size(1024u64.pow(4)), "1.0 TB");
    }

    #[test]
    fn format_size_caps_at_terabytes() {
        // TB を超えても単位配列の上限で頭打ちになり、数値だけが大きくなる。
        assert_eq!(format_size(1024u64.pow(5)), "1024.0 TB");
    }

    #[test]
    fn format_time_none_is_dash() {
        assert_eq!(format_time(None), "-");
    }

    #[test]
    fn format_time_some_has_expected_shape() {
        // ローカルタイムゾーン依存なので厳密な値ではなく "YYYY-MM-DD HH:MM" の形だけ検証する。
        let formatted = format_time(Some(SystemTime::UNIX_EPOCH));
        let bytes = formatted.as_bytes();
        assert_eq!(formatted.len(), 16);
        assert_eq!(bytes[4], b'-');
        assert_eq!(bytes[7], b'-');
        assert_eq!(bytes[10], b' ');
        assert_eq!(bytes[13], b':');
        assert!(formatted[..4].bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn sort_column_int_round_trips() {
        // Slint との列識別子（int）変換が往復で保たれる。未知値は名前列へ倒れる。
        for column in [SortColumn::Name, SortColumn::Size, SortColumn::Modified] {
            assert_eq!(sort_column_from_int(sort_column_to_int(column)), column);
        }
        assert_eq!(sort_column_from_int(999), SortColumn::Name);
    }
}
