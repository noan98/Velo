use std::path::Path;
use std::time::Duration;

use notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_full::{new_debouncer, DebounceEventResult, Debouncer, RecommendedCache};

/// 1 ディレクトリを監視するデバウンサの具体型。
///
/// この値を生かしておく（drop しない）あいだだけ監視が続く。
/// 別ディレクトリへ移動したら、古い `DirWatcher` を drop して新しいものに置き換える。
pub type DirWatcher = Debouncer<RecommendedWatcher, RecommendedCache>;

/// 監視のデバウンス時間。`notify` の生イベントは 1 操作で複数飛ぶことがあるため、
/// この時間まとめてから 1 回だけ通知する（受け入れ条件「数百ミリ秒以内」に収まる値）。
const DEBOUNCE: Duration = Duration::from_millis(300);

/// `path` を非再帰で監視し、変更がデバウンスされて確定するたびに `on_change` を呼ぶ。
/// 監視中にエラーが起きた場合は、まとめたメッセージで `on_error` を呼ぶ。
///
/// **注意:** どちらのコールバックもデバウンサ内部のスレッドで呼ばれる。
/// したがって UI を更新する際は、呼び出し側で `upgrade_in_event_loop` 等を使って
/// 必ずイベントループ（UI スレッド）に載せ替えること。
pub fn watch_dir<F, G>(path: &Path, on_change: F, on_error: G) -> notify::Result<DirWatcher>
where
    F: Fn() + Send + 'static,
    G: Fn(String) + Send + 'static,
{
    let mut debouncer = new_debouncer(DEBOUNCE, None, move |result: DebounceEventResult| {
        match result {
            // 個々のイベント内容は土台では見ず、「何か変わった」だけで再読み込みする。
            Ok(_events) => on_change(),
            // 監視エラーは握りつぶさず、1 つの文言にまとめて呼び出し側（UI）へ通知する。
            Err(errors) => {
                let message = errors
                    .iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ");
                on_error(message);
            }
        }
    })?;

    debouncer.watch(path, RecursiveMode::NonRecursive)?;
    Ok(debouncer)
}
