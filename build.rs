// .slint マークアップを Rust コードへコンパイルする。
// 生成物は main.rs の `slint::include_modules!()` で取り込む。
fn main() {
    slint_build::compile("ui/app.slint").unwrap();
}
