#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    echo "请在 macOS 上运行此脚本。" >&2
    exit 1
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
host=$(rustc -vV | sed -n 's/^host: //p')
case "$host" in *-apple-darwin) ;; *) echo "当前 Rust 工具链不是 macOS 原生工具链：$host" >&2; exit 1 ;; esac
version=$(awk '/^\[package\]/{in_package=1; next} /^\[/{in_package=0} in_package && /^version[[:space:]]*=/{gsub(/"/, "", $3); print $3; exit}' "$root/Cargo.toml")
[ -n "$version" ] || { echo "无法读取项目版本。" >&2; exit 1; }
name="icloud-download-helper-$version-$host"
output="$root/dist/$name.zip"
staging=$(mktemp -d "${TMPDIR:-/tmp}/icloud-package.XXXXXXXX")
trap 'rm -rf "$staging"' EXIT HUP INT TERM

cd "$root"
cargo build --release --locked --features gui --bins --target "$host" --target-dir "$root/target"
mkdir -p "$staging/$name/iCloud Download Helper.app/Contents/MacOS" "$root/dist"
app="$staging/$name/iCloud Download Helper.app/Contents"
cp "$root/target/$host/release/icloud-dl-gui" "$app/MacOS/icloud-dl-gui"
cp "$root/target/$host/release/icloud-dl" "$staging/$name/icloud-dl"
cp README.md LICENSE THIRD_PARTY_NOTICES.md "$staging/$name/"
cat > "$app/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "https://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
    <key>CFBundleExecutable</key><string>icloud-dl-gui</string>
    <key>CFBundleIdentifier</key><string>com.thesummert.iclouddownloadhelper</string>
    <key>CFBundleName</key><string>iCloud Download Helper</string>
    <key>CFBundleDisplayName</key><string>iCloud Download Helper</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>$version</string>
    <key>CFBundleVersion</key><string>$version</string>
    <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
/usr/bin/ditto -c -k --norsrc --keepParent "$staging/$name" "$output"
printf '打包完成：%s\n' "$output"
