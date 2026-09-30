#!/usr/bin/env bash
# install the Warp Agent desktop entry + hicolor icons so the Linux
# taskbar/launcher shows the app icon (matched via StartupWMClass)
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

icon_root="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor"
for f in "$here"/icons/warpagent-*.png; do
	size="$(basename "$f" .png)"; size="${size#warpagent-}"
	dir="$icon_root/${size}x${size}/apps"
	mkdir -p "$dir"
	cp "$f" "$dir/warpagent.png"
done
mkdir -p "$icon_root/scalable/apps"
cp "$here/icons/warpagent.svg" "$icon_root/scalable/apps/"

app_dir="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
mkdir -p "$app_dir"
rm -f "$app_dir/warpagent.desktop" # pre-Rust entry, replaced by warp-agent.desktop
cp "$here/warp-agent.desktop" "$app_dir/"

command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$app_dir" || true
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -tf "$icon_root" || true
echo "installed: $app_dir/warp-agent.desktop + hicolor icons"
