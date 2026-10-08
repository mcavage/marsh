#!/bin/sh
# Render the site's raster images from their SVG sources with headless Chrome:
#   docs/site/og.svg       -> docs/site/og.png              (1200x630 social card)
#   docs/site/favicon.svg  -> docs/site/apple-touch-icon.png (180x180)
# The PNGs are committed, so `make site` needs no browser. Rerun after editing
# either SVG.  CHROME=/path/to/chrome overrides the browser.
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
chrome=${CHROME:-"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"}
[ -x "$chrome" ] || { echo "site-images: no Chrome at $chrome (set CHROME)" >&2; exit 1; }
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
shot() { # SVG PNG W H
  printf '<!doctype html><html><body style="margin:0;background:#0a0c0b"><img src="file://%s" width="%s" height="%s" style="display:block"></body></html>' \
    "$1" "$3" "$4" >"$tmp/page.html"
  "$chrome" --headless=new --disable-gpu --hide-scrollbars --allow-file-access-from-files \
    --force-device-scale-factor=1 --window-size="$3,$4" --screenshot="$2" "file://$tmp/page.html" >/dev/null 2>&1
  echo "site-images: $2"
}
shot "$root/docs/site/og.svg" "$root/docs/site/og.png" 1200 630
shot "$root/docs/site/favicon.svg" "$root/docs/site/apple-touch-icon.png" 180 180
