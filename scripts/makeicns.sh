#!/bin/sh
set -e

usage() {
    cat <<EOF
$0: Build the macOS icns file and menu-bar template from the mousehop SVGs.

The app icon SVG is a self-contained rounded tile (its own background and
rounded corners), so it is rendered straight into the Big Sur+ 824px squircle
and centered on a 1024 canvas with transparent padding. The menu-bar template
is the tray glyph, flattened to black+alpha so NSStatusBarButton can tint it
for light and dark menu bars.

usage: $0 [APP_SVG [TRAY_SVG [ICNS [ICONSET]]]]

ARGUMENTS
    APP_SVG  The app icon SVG (a self-contained rounded tile)
             Defaults to ./mousehop-gtk/resources/com.mousehop.Mousehop.svg
    TRAY_SVG The tray / menu-bar glyph SVG
             Defaults to ./mousehop-gtk/resources/icons/mousehop-tray.svg
    ICNS     The icns file to create
             Defaults to ./target/icon.icns
    ICONSET  The iconset directory to create
             Defaults to ./target/icon.iconset
             This is just a temporary directory
EOF
}

if [ "$1" = "-h" ] || [ "$1" = "--help" ]; then
    usage
    exit 0
fi

app_svg="${1:-./mousehop-gtk/resources/com.mousehop.Mousehop.svg}"
tray_svg="${2:-./mousehop-gtk/resources/icons/mousehop-tray.svg}"
icns="${3:-./target/icon.icns}"
iconset="${4:-./target/icon.iconset}"

set -u

workdir="$(dirname "$iconset")/icon-work"
rm -rf "$iconset" "$workdir"
mkdir -p "$iconset" "$workdir"

# Big Sur+ macOS icon template proportions (in a 1024 canvas):
#   canvas   = 1024
#   squircle = 824  (the rounded tile, inset 100px from the canvas edges)
# The app SVG already carries its own background and rounded corners (rx
# 22.5%), so it is rendered straight to the squircle size — no separate
# background fill is needed.
CANVAS=1024
SQUIRCLE=824
OFFSET=$(( (CANVAS - SQUIRCLE) / 2 ))

# 1) Render the app tile at squircle size.
#    rsvg-convert handles our SVG correctly; ImageMagick sometimes crops it.
rsvg-convert -w "$SQUIRCLE" -h "$SQUIRCLE" "$app_svg" -o "$workdir/tile.png"

# 2) Center the tile on a transparent canvas. The transparent padding makes
#    the Dock/Finder render it at the same visual size as first-party apps.
magick -size ${CANVAS}x${CANVAS} xc:none \
    "$workdir/tile.png" -geometry +${OFFSET}+${OFFSET} -composite \
    -colorspace sRGB -type TrueColorAlpha PNG32:"$workdir/icon-1024.png"

# 3) Generate each iconset size from the master so all sizes stay consistent.
for size in 1024 512 256 128 64 32 16; do
    magick "$workdir/icon-1024.png" -resize ${size}x${size} \
        -colorspace sRGB -type TrueColorAlpha PNG32:"$workdir/${size}.png"
done

cp "$workdir/1024.png" "$iconset"/icon_512x512@2x.png
cp "$workdir/512.png"  "$iconset"/icon_512x512.png
cp "$workdir/512.png"  "$iconset"/icon_256x256@2x.png
cp "$workdir/256.png"  "$iconset"/icon_256x256.png
cp "$workdir/256.png"  "$iconset"/icon_128x128@2x.png
cp "$workdir/128.png"  "$iconset"/icon_128x128.png
cp "$workdir/64.png"   "$iconset"/icon_32x32@2x.png
cp "$workdir/32.png"   "$iconset"/icon_32x32.png
cp "$workdir/32.png"   "$iconset"/icon_16x16@2x.png
cp "$workdir/16.png"   "$iconset"/icon_16x16.png

mkdir -p "$(dirname "$icns")"

# Menu-bar template icon: render the tray glyph (the purpose-built small-size
# mark) and flatten all RGB channels to 0 (black) while keeping alpha, so the
# artwork reads as a clean silhouette. NSStatusBarButton tints template images
# to match the menu bar appearance in light and dark modes.
menubar_template="$(dirname "$icns")/menubar-template.png"
rsvg-convert -w 44 -h 44 "$tray_svg" -o "$workdir/menubar-44.png"
magick "$workdir/menubar-44.png" -channel RGB -evaluate set 0 +channel \
    "$menubar_template"

if ! iconutil -c icns "$iconset" -o "$icns"; then
    if ! command -v perl >/dev/null 2>&1; then
        echo "iconutil failed and perl is not available for the fallback icns writer" >&2
        exit 1
    fi

    echo "iconutil rejected the iconset; writing icns directly" >&2
    perl - "$icns" "$iconset" <<'PERL'
use strict;
use warnings;

my ($icns, $iconset) = @ARGV;
my @icons = (
    [ 'icp4', "$iconset/icon_16x16.png" ],
    [ 'ic11', "$iconset/icon_16x16\@2x.png" ],
    [ 'icp5', "$iconset/icon_32x32.png" ],
    [ 'ic12', "$iconset/icon_32x32\@2x.png" ],
    [ 'ic07', "$iconset/icon_128x128.png" ],
    [ 'ic13', "$iconset/icon_128x128\@2x.png" ],
    [ 'ic08', "$iconset/icon_256x256.png" ],
    [ 'ic14', "$iconset/icon_256x256\@2x.png" ],
    [ 'ic09', "$iconset/icon_512x512.png" ],
    [ 'ic10', "$iconset/icon_512x512\@2x.png" ],
);

my $body = '';
for my $icon (@icons) {
    my ($type, $path) = @$icon;
    open my $fh, '<:raw', $path or die "$path: $!";
    local $/;
    my $png = <$fh>;
    $body .= $type . pack('N', length($png) + 8) . $png;
}

open my $out, '>:raw', $icns or die "$icns: $!";
print {$out} 'icns' . pack('N', length($body) + 8) . $body;
PERL
fi
