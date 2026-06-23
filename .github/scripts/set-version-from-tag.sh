#!/usr/bin/env bash
# Git タグからバージョンを抽出し、Cargo.toml / Cargo.lock の velo パッケージへ動的に反映する。
#
# なぜ必要か:
#   dist（cargo-dist）は「push されたタグのバージョン」と「Cargo.toml の version」が
#   一致していないとリリース対象を特定できず失敗する。両者を手作業で揃えるのは事故のもとで、
#   実際に Cargo.toml が 0.1.0 のまま v0.1.2 を push してリリース CI が落ちた。
#   そこでリリース CI 内でこのスクリプトを走らせ、タグを唯一の真実として version を埋める。
#   これにより、リリース時に Cargo.toml を手編集する必要がなくなる。
#
# 前提:
#   GitHub Actions から呼ばれ、GITHUB_REF / GITHUB_REF_NAME を参照する。
#   タグ push 以外（PR など）では何もせず終了する。
#   Windows ランナーでも動くよう、awk / grep / sed といった移植性の高いツールだけを使う。
set -euo pipefail

ref="${GITHUB_REF:-}"
case "$ref" in
  refs/tags/*) ;;
  *)
    echo "タグ push ではないためバージョン注入をスキップします (ref='${ref}')"
    exit 0
    ;;
esac

name="${GITHUB_REF_NAME:-}"
# v0.1.2 / velo-v0.1.2 / 0.1.2-rc.1 などからセマンティックバージョン部分だけを取り出す。
version="$(printf '%s' "$name" | grep -oE '[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?' | head -n1 || true)"
if [ -z "$version" ]; then
  echo "タグ '${name}' からバージョンを抽出できませんでした" >&2
  exit 1
fi
echo "タグ '${name}' → バージョン '${version}' を Cargo.toml / Cargo.lock に反映します"

# Cargo.toml: [package] テーブルが先頭にあるため、最初に現れる version 行だけを置き換える。
awk -v v="$version" '
  !done && /^version = / { print "version = \"" v "\""; done = 1; next }
  { print }
' Cargo.toml > Cargo.toml.tmp && mv Cargo.toml.tmp Cargo.toml

# Cargo.lock: name = "velo" の直後に続く version 行を置き換える（他パッケージは触らない）。
awk -v v="$version" '
  {
    if (prev == "name = \"velo\"" && $0 ~ /^version = /) {
      print "version = \"" v "\""
    } else {
      print
    }
    prev = $0
  }
' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock

echo "反映後の値:"
echo "  Cargo.toml: $(grep -m1 '^version = ' Cargo.toml)"
# Cargo.lock の velo パッケージの version も出力し、両ファイルが揃ったことをログで目視確認できるようにする。
echo "  Cargo.lock: $(grep -A1 '^name = "velo"$' Cargo.lock | grep -m1 '^version = ')"
