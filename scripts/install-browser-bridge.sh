#!/bin/sh
set -eu
umask 077
case "$(uname -s)" in Darwin) ;; *) echo 'browser-use-bridge currently supports macOS.' >&2; exit 1;; esac
version="${IPU_BROWSER_BRIDGE_VERSION:-latest}"
base=https://github.com/leeguooooo/iphone-use/releases/latest/download
if [ "$version" != latest ]; then
  case "$version" in *[!A-Za-z0-9._-]*) echo 'Invalid version' >&2; exit 1;; esac
  base="https://github.com/leeguooooo/iphone-use/releases/download/$version"
fi
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT HUP INT TERM
asset=browser-use-bridge-macos-universal.tar.gz
curl -fL --retry 2 "$base/$asset" -o "$stage/$asset"
curl -fL --retry 2 "$base/$asset.sha256" -o "$stage/$asset.sha256"
(cd "$stage" && shasum -a 256 -c "$asset.sha256")
test "$(tar -tzf "$stage/$asset")" = browser-use-bridge
if tar -tvzf "$stage/$asset" | awk '{print substr($0,1,1)}' | grep -E '[lh]' >/dev/null; then exit 1; fi
tar -xzf "$stage/$asset" -C "$stage"
codesign --verify "$stage/browser-use-bridge"
mkdir -p "$HOME/.local/bin"
install -m 755 "$stage/browser-use-bridge" "$HOME/.local/bin/browser-use-bridge"
echo "Installed $HOME/.local/bin/browser-use-bridge"
