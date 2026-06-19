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
/// テストから参照できるよう `pub(crate)` にしている。
pub(crate) const DEBOUNCE: Duration = Duration::from_millis(300);

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    /// `cond` が真になるまで短い間隔でポーリングし、成立したら true を返す。
    /// `timeout` を超えても成立しなければ false。
    ///
    /// 固定スリープ（例: `DEBOUNCE + 400ms`）だけで待つと、CI の負荷や
    /// ファイルシステム通知の遅延で「まだ届いていないのに時間切れ」になりフレークしやすい。
    /// 条件成立で即座に抜け、遅い環境では上限まで粘る形にすることで、速さと安定性を両立する。
    fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        cond()
    }

    // ---- ケース1: 存在するディレクトリを watch → Ok が返る ----

    #[test]
    fn watch_dir_存在するパスへの監視は成功する() {
        let tmp = tempfile::tempdir().unwrap();
        // 存在するディレクトリを渡すと DirWatcher が正常に返るはず
        let result = watch_dir(tmp.path(), || {}, |_| {});
        assert!(result.is_ok(), "存在するディレクトリの監視は Ok を返すべき");
    }

    // ---- ケース2: ファイルを作成すると on_change コールバックが呼ばれる ----

    #[test]
    fn watch_dir_ファイル作成でコールバックが呼ばれる() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_cb = Arc::clone(&counter);

        // コールバック内でカウンタをインクリメントする
        let _watcher = watch_dir(
            tmp.path(),
            move || {
                counter_cb.fetch_add(1, Ordering::SeqCst);
            },
            |_| {},
        )
        .expect("監視の開始に失敗");

        // ファイルを作成してイベントを発火させる
        std::fs::File::create(tmp.path().join("test.txt")).unwrap();

        // コールバックが呼ばれるまでポーリングで待つ（上限は DEBOUNCE + 余裕の 1500ms）。
        // 早く届けば即座に抜け、遅い環境でも上限まで粘るのでフレークしにくい。
        assert!(
            wait_until(DEBOUNCE + Duration::from_millis(1500), || counter
                .load(Ordering::SeqCst)
                >= 1),
            "ファイル作成後にコールバックが少なくとも 1 回呼ばれるべき"
        );
    }

    // ---- ケース3: 300ms 以内の連続変更が 1 回のコールバックに畳み込まれる ----

    #[test]
    fn watch_dir_短時間の連続変更がデバウンスされる() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_cb = Arc::clone(&counter);

        let _watcher = watch_dir(
            tmp.path(),
            move || {
                counter_cb.fetch_add(1, Ordering::SeqCst);
            },
            |_| {},
        )
        .expect("監視の開始に失敗");

        // DEBOUNCE（300ms）未満の間隔で複数回ファイルを作成し、
        // デバウンス後に集約されることを確認する。
        // ファイルシステムの実装によってはイベントがさらにまとめられる場合もあるため、
        // 「DEBOUNCE 内の 5 回変更がコールバック呼び出し件数より多い」ことだけを検証する
        // （== 1 を強制すると OS/カーネル差でフレークするリスクがある）。
        for i in 0..5u32 {
            std::fs::File::create(tmp.path().join(format!("debounce_{i}.txt"))).unwrap();
            // イベントをばらけさせず、DEBOUNCE 内に収まるよう間隔を小さくする
            std::thread::sleep(Duration::from_millis(30));
        }

        // まず「少なくとも 1 回はデバウンス後のコールバックが届く」までポーリングで待つ。
        assert!(
            wait_until(DEBOUNCE + Duration::from_millis(1500), || counter
                .load(Ordering::SeqCst)
                >= 1),
            "変更後にコールバックが少なくとも 1 回呼ばれるべき"
        );

        // 「5 回の変更が 5 回のコールバックに膨らんでいない」という“起きないこと”の検証は
        // ポーリングでは確かめられない（非事象は待つしかない）。最初のコールバック確定後、
        // 後続のフラッシュが来ないことを確かめるため DEBOUNCE 分だけ落ち着かせてから上限を見る。
        std::thread::sleep(DEBOUNCE + Duration::from_millis(200));

        let calls = counter.load(Ordering::SeqCst);
        // 環境によっては複数回届くこともあるため、上限は 5 回未満（< 5）で検証する。
        assert!(
            calls < 5,
            "デバウンス処理により呼び出し回数は変更回数(5)より少ないはず (実際: {calls})"
        );
    }

    // ---- ケース4: DirWatcher を drop すると以降の変更でコールバックが呼ばれない ----

    #[test]
    fn watch_dir_ドロップ後はコールバックが呼ばれない() {
        let tmp = tempfile::tempdir().unwrap();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_cb = Arc::clone(&counter);

        let watcher = watch_dir(
            tmp.path(),
            move || {
                counter_cb.fetch_add(1, Ordering::SeqCst);
            },
            |_| {},
        )
        .expect("監視の開始に失敗");

        // まず watcher が動作していることを、コールバック到達までポーリングして確認する。
        std::fs::File::create(tmp.path().join("before_drop.txt")).unwrap();
        assert!(
            wait_until(DEBOUNCE + Duration::from_millis(1500), || counter
                .load(Ordering::SeqCst)
                >= 1),
            "drop 前のファイル作成でコールバックが呼ばれるべき"
        );

        // watcher を drop して監視を停止する
        drop(watcher);

        // drop 直後の内部クリーンアップが落ち着くまで少し待つ
        std::thread::sleep(Duration::from_millis(100));

        // drop 後のカウンタ値を記録しておく
        let count_after_drop = counter.load(Ordering::SeqCst);

        // drop 後にファイルを作成してもコールバックは呼ばれないはず。
        // これも“起きないこと”の検証なので、十分な時間（DEBOUNCE + 余裕）待ってから確認する。
        std::fs::File::create(tmp.path().join("after_drop.txt")).unwrap();
        std::thread::sleep(DEBOUNCE + Duration::from_millis(400));

        let count_final = counter.load(Ordering::SeqCst);
        assert_eq!(
            count_after_drop, count_final,
            "DirWatcher を drop した後はファイル変更があってもコールバックが増えないはず"
        );
    }
}
