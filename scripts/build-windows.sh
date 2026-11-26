#!/usr/bin/env bash
# Release-Build für x86_64-pc-windows-msvc von Linux aus (cargo-xwin) und ZIP-Paket.
#
# Voraussetzungen (einmalig, siehe DECISIONS.md):
#   rustup target add x86_64-pc-windows-msvc
#   cargo-xwin installieren (cargo install --locked cargo-xwin oder Release-Binary)
# Beim ersten Lauf lädt cargo-xwin das MSVC-CRT und das Windows SDK nach ~/.cache/cargo-xwin
# (Microsoft-Lizenz wird dabei akzeptiert). Gelinkt wird mit lld-link (rust-lld).
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-msvc
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
NAME="ptpmaster-${VERSION}-windows-x64"

cargo xwin build --release --target "$TARGET"

rm -rf "dist/$NAME" "dist/$NAME.zip"
mkdir -p "dist/$NAME/scripts"
cp "target/$TARGET/release/ptpmaster.exe" "dist/$NAME/"
cp ptpmaster.example.toml README.md DECISIONS.md LICENSE "dist/$NAME/"
cp ptpmaster.example.toml "dist/$NAME/ptpmaster.toml"
cp scripts/firewall.ps1 "dist/$NAME/scripts/"
(cd dist && zip -qr "$NAME.zip" "$NAME")

echo "Fertig: dist/$NAME.zip"
ls -l "dist/$NAME/ptpmaster.exe" "dist/$NAME.zip"
