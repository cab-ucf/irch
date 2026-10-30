#!/bin/sh
# Self-updating irch: curl -fsSL $R/install.sh | sh [-s -- --server [key]]
# --server: an always-on storage node instead (a service); with a Firebase
# service account's key, a push server too.
# Fetched by curl, nothing is quarantined: no Gatekeeper, no xattr.
set -eu
R=https://github.com/cab-ucf/irch/releases/latest/download
case $(uname -m) in arm64 | aarch64) cpu=aarch64 ;; *) cpu=x86_64 ;; esac
# irch's key: with minisign here, every file is checked as updates are.
KEY=RWSPfztwzR/MhakRL6cA70pN03n/nMCdTvoY5UHC2IcZuhcR+41RQI95
check() {
  command -v minisign >/dev/null || return 0
  curl -fsSL "$1.sig" | base64 -d >"$2.minisig"
  minisign -Vm "$2" -x "$2.minisig" -P "$KEY" | grep -q "file:${1##*/}" ||
    { echo "$1: not signed by irch" && exit 1; }
  rm -f "$2.minisig"
}
get() { curl -fL --progress-bar "$R/$1" -o "$2" && check "$R/$1" "$2"; }
bin=$HOME/.local/bin

if [ "${1:-}" = --server ]; then
  [ "$(uname -s)" = Darwin ] && t=apple-darwin || t=unknown-linux-musl
  mkdir -p "$bin"
  get "irch-$cpu-$t" "$bin/irch.part"
  chmod +x "$bin/irch.part" && mv "$bin/irch.part" "$bin/irch"
  # A Firebase service account's key after --server: a push server too.
  if [ -n "${2:-}" ]; then
    d=${XDG_DATA_HOME:-$HOME/.local/share}
    [ "$(uname -s)" = Darwin ] && d="$HOME/Library/Application Support"
    mkdir -p "$d/irch-serve" && install -m 600 "$2" "$d/irch-serve/fcm.json"
  fi
  exec "$bin/irch" --install
fi

if [ "$(uname -s)" = Darwin ]; then
  [ $cpu = aarch64 ] && f=irch_aarch64.app.tar.gz || f=irch_x64.app.tar.gz
  to=/Applications
  [ -w $to ] || { to=$HOME/Applications && mkdir -p "$to"; }
  t=$(mktemp -d)
  get "$f" "$t/irch.app.tar.gz"
  rm -rf "$to/irch.app"
  tar -xzf "$t/irch.app.tar.gz" -C "$to" && rm -rf "$t"
  open "$to/irch.app"
  echo "irch is in $to, and updates itself."
  exit
fi

if [ -e /etc/NIXOS ]; then
  echo "NixOS: add irch's module instead (it keeps it up to date too):"
  echo "  https://github.com/cab-ucf/irch#nixos"
  exit 1
fi

# The AppImage: one file, yours to replace, so updates need no password.
url=$(curl -fsSL "$R/latest.json" | tr -d ' \n' |
  sed "s/.*\"linux-$cpu-appimage\":{[^}]*\"url\":\"\([^\"]*\)\".*/\1/")
app=$bin/irch.AppImage
mkdir -p "$bin" "$HOME/.local/share/applications" \
  "$HOME/.local/share/icons/hicolor/512x512/apps"
curl -fL --progress-bar "$url" -o "$app.part" && check "$url" "$app.part"
chmod +x "$app.part" && mv "$app.part" "$app"
curl -fsSL "$R/irch.png" \
  -o "$HOME/.local/share/icons/hicolor/512x512/apps/irch.png" || true
# Without FUSE 2 an AppImage still runs, unpacked each time.
env=
ldconfig -p 2>/dev/null | grep -q libfuse.so.2 || {
  env="env APPIMAGE_EXTRACT_AND_RUN=1 " && export APPIMAGE_EXTRACT_AND_RUN=1
}
cat > "$HOME/.local/share/applications/irch.desktop" <<E
[Desktop Entry]
Type=Application
Name=irch
Comment=Private, peer-to-peer chat
Exec=$env"$app" %u
Icon=irch
Categories=Network;Chat;
MimeType=x-scheme-handler/irch;
E
command -v update-desktop-database >/dev/null &&
  update-desktop-database "$HOME/.local/share/applications" || true
command -v xdg-mime >/dev/null &&
  xdg-mime default irch.desktop x-scheme-handler/irch || true
echo "irch is in your menu, and updates itself."
(nohup "$app" >/dev/null 2>&1 &) || true
