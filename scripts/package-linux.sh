#!/bin/sh
set -eu

if [ "$(uname -s)" != Linux ]; then
    echo "请在 Linux 上运行此脚本。" >&2
    exit 1
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
host=$(rustc -vV | sed -n 's/^host: //p')
case "$host" in *-unknown-linux-gnu|*-unknown-linux-musl) ;; *) echo "不支持的 Linux 原生工具链：$host" >&2; exit 1 ;; esac
version=$(awk '/^\[package\]/{in_package=1; next} /^\[/{in_package=0} in_package && /^version[[:space:]]*=/{gsub(/"/, "", $3); print $3; exit}' "$root/Cargo.toml")
[ -n "$version" ] || { echo "无法读取项目版本。" >&2; exit 1; }
name="icloud-download-helper-$version-$host"
output="$root/dist/$name.tar.gz"
staging=$(mktemp -d "${TMPDIR:-/tmp}/icloud-package.XXXXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM

cd "$root"
cargo build --release --locked --features gui --bins --target "$host" --target-dir "$root/target"
mkdir -p "$staging/$name" "$root/dist"
cp "$root/target/$host/release/icloud-dl" "$root/target/$host/release/icloud-dl-gui" "$staging/$name/"
cp README.md LICENSE THIRD_PARTY_NOTICES.md "$staging/$name/"
tar -czf "$output" -C "$staging" "$name"
printf '打包完成：%s\n' "$output"
