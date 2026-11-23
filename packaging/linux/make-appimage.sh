#!/usr/bin/env bash
# Packs the Linux release binary into an AppImage (https://docs.appimage.org/reference/appdir.html).
#
#   APPIMAGE_ARCH=<x86_64|aarch64> make-appimage.sh <loudnorm binary> <icon PNG> <desktop file> <appimagetool> <runtime file> <output .AppImage>
#
# APPIMAGE_ARCH is the architecture of the binary and is required; it must match the appimagetool and the runtime.
# appimagetool is the appimagetool-<arch>.AppImage of https://github.com/AppImage/appimagetool/releases and the runtime
# file is the runtime-<arch> of https://github.com/AppImage/type2-runtime/releases. The versions and sha256 checksums that
# CI uses are in .github/workflows/build.yml (APPIMAGETOOL_VERSION, TYPE2_RUNTIME_VERSION and the build matrix).
#
# The AppImage runtime is passed in (not downloaded by appimagetool) so that CI can pin and verify it.
# FUSE is not needed for the build: appimagetool, itself an AppImage, runs through --appimage-extract-and-run.
set -euo pipefail

if [[ $# -ne 6 ]]; then
  echo "usage: APPIMAGE_ARCH=<x86_64|aarch64> $0 <loudnorm binary> <icon PNG> <desktop file> <appimagetool> <runtime file> <output .AppImage>" >&2
  exit 2
fi
binary=$1
icon=$2
desktop=$3
appimagetool=$4
runtime=$5
output=$6

appdir=$(mktemp --directory)
trap 'rm --recursive --force "$appdir"' EXIT

# Real files under usr/ as the AppDir conventions recommend; the root entries the specification requires are symlinks.
install --directory "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/256x256/apps"
install --mode=0755 "$binary" "$appdir/usr/bin/loudnorm"
install --mode=0644 "$desktop" "$appdir/usr/share/applications/loudnorm.desktop"
install --mode=0644 "$icon" "$appdir/usr/share/icons/hicolor/256x256/apps/loudnorm.png"
ln --symbolic usr/bin/loudnorm "$appdir/AppRun"
ln --symbolic usr/share/applications/loudnorm.desktop "$appdir/loudnorm.desktop"
ln --symbolic usr/share/icons/hicolor/256x256/apps/loudnorm.png "$appdir/loudnorm.png"
ln --symbolic loudnorm.png "$appdir/.DirIcon"

# Without --no-appstream appimagetool warns that the AppDir has no AppStream metadata; the app ships none.
ARCH=${APPIMAGE_ARCH:?set APPIMAGE_ARCH to x86_64 or aarch64} \
  "$appimagetool" --appimage-extract-and-run --no-appstream --runtime-file "$runtime" "$appdir" "$output"
