// .slint マークアップを Rust コードへコンパイルする。
// 生成物は main.rs の `slint::include_modules!()` で取り込む。
fn main() {
    // Slint コンパイラは式や AST を再帰的にたどるため、UI が育つと深い再帰になる。
    // Windows のメインスレッド既定スタックは 1MB と小さく、ここで STATUS_STACK_OVERFLOW
    // （ビルドスクリプトのスタック溢れ）に陥ることがある（Linux は既定 8MB なので顕在化しにくい）。
    // そこで、十分に大きいスタックを確保したワーカースレッド上でコンパイルを実行し、
    // プラットフォームに依存せず安定してビルドできるようにする。
    // cargo: ディレクティブは slint_build が標準出力へ書くため、別スレッドからでも問題ない。
    std::thread::Builder::new()
        .stack_size(64 * 1024 * 1024)
        .spawn(|| {
            slint_build::compile("ui/app.slint").unwrap();
        })
        .expect("Slint コンパイル用スレッドの生成に失敗")
        .join()
        .expect("Slint コンパイル用スレッドが panic した");
}
