#!/usr/bin/env bash
# Regenerates the raster derivatives of the emblem. Needs agent-svgtools and ImageMagick.
set -euo pipefail
cd "$(dirname "$0")"
public=../ui/public
assets=../docs/user/assets
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

render() { agent-svgtools render xolotl-icon.svg "$2" --width "$1" --height "$1" >/dev/null; }
render_dark() { agent-svgtools render xolotl-icon-dark.svg "$2" --width "$1" --height "$1" >/dev/null; }

# Transparent PNG icons for the web manifest.
render 192 "$public/icon-192.png"
render 512 "$public/icon-512.png"

# Maskable icon: the emblem in the safe zone (80%) on the ink colour.
render_dark 410 "$tmp/mask.png"
magick -size 512x512 xc:'#192029' "$tmp/mask.png" -gravity center -composite "$public/icon-maskable-512.png"

# Apple touch icon: no transparency, the UI's page background.
render 150 "$tmp/apple.png"
magick -size 180x180 xc:'#f4f5f7' "$tmp/apple.png" -gravity center -composite "$public/apple-touch-icon.png"

# Legacy favicon with the sizes browsers actually request.
for s in 16 32 48; do render "$s" "$tmp/fav-$s.png"; done
magick "$tmp/fav-16.png" "$tmp/fav-32.png" "$tmp/fav-48.png" "$public/favicon.ico"

# Emblem at a size that works in a README, plus a social preview.
agent-svgtools render xolotl-emblem.svg xolotl-emblem.png --height 640 >/dev/null
render_dark 440 "$tmp/social.png"
magick -size 1280x640 xc:'#192029' "$tmp/social.png" -gravity center -composite xolotl-social.png

# Copies published to wyrd.foo with the user docs (docs/user/docs.yml: logo, logo-dark, favicon).
cp xolotl-icon.svg "$assets/logo.svg"
cp xolotl-icon-dark.svg "$assets/logo-dark.svg"
cp "$public/favicon.svg" "$assets/favicon.svg"
