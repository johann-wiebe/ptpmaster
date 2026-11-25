#!/usr/bin/env bash
# Richtet die Build-Toolchain ohne sudo im Home-Verzeichnis ein (CI-Runner, frische Linux-VM):
#   - Rust stable mit Target x86_64-pc-windows-msvc, clippy, rustfmt
#   - Zig als cc-Ersatz für Host-Builds (Build-Skripte, proc-macros), weil das Runner-Image kein gcc hat
#   - cargo-xwin (Release-Binary) für den Windows-Build mit MSVC-CRT/SDK und lld-link
# Downloads werden per SHA-256 geprüft. Bereits Vorhandenes (z. B. aus dem Cache) wird übersprungen.
# Gibt am Ende die PATH-Ergänzung aus bzw. schreibt sie nach $GITHUB_PATH.
set -euo pipefail

ZIG_VERSION=0.17.1
ZIG_SHA256=3581863e3cbcfd0fce0efc682ce7e9e93da96668a977791d356a26d2a7e9b17a
XWIN_VERSION=0.24.1
XWIN_SHA256=207ec5436db138b60d90cb41513ce2e3db4893a9c4e25da6209790c1a2db88cf

OPT="$HOME/.local/opt"
mkdir -p "$OPT"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() {  # fetch <url> <sha256> <datei>
  curl -fsSL --retry 3 -o "$3" "$1"
  echo "$2  $3" | sha256sum -c --quiet -
}

# Rust
if ! command -v "$HOME/.cargo/bin/rustup" >/dev/null; then
  curl -fsSL --retry 3 https://sh.rustup.rs -o "$tmp/rustup.sh"
  sh "$tmp/rustup.sh" -y --no-modify-path --profile minimal --default-toolchain stable \
    -c clippy,rustfmt -t x86_64-pc-windows-msvc
else
  "$HOME/.cargo/bin/rustup" toolchain install stable --profile minimal -c clippy,rustfmt -t x86_64-pc-windows-msvc
fi

# Zig + cc-Wrapper
zig_dir="$OPT/zig-x86_64-linux-$ZIG_VERSION"
if [ ! -x "$zig_dir/zig" ]; then
  fetch "https://ziglang.org/download/$ZIG_VERSION/zig-x86_64-linux-$ZIG_VERSION.tar.xz" "$ZIG_SHA256" "$tmp/zig.tar.xz"
  tar -xJf "$tmp/zig.tar.xz" -C "$OPT"
fi
mkdir -p "$OPT/zigcc"
cat > "$OPT/zigcc/cc" <<EOF
#!/usr/bin/env bash
# cc-Ersatz über Zig; filtert Linker-Flags, die zig cc nicht versteht.
out=()
for a in "\$@"; do
  case "\$a" in
    -Wl,--no-undefined-version|-fuse-ld=lld|-Wl,-znostart-stop-gc|-Wl,-z,nostart-stop-gc) ;;
    *) out+=("\$a") ;;
  esac
done
exec "$zig_dir/zig" cc -target x86_64-linux-gnu "\${out[@]}"
EOF
chmod +x "$OPT/zigcc/cc"

# cargo-xwin
if ! "$HOME/.cargo/bin/cargo-xwin" --version 2>/dev/null | grep -q "$XWIN_VERSION"; then
  fetch "https://github.com/rust-cross/cargo-xwin/releases/download/v$XWIN_VERSION/cargo-xwin-v$XWIN_VERSION.x86_64-unknown-linux-musl.tar.gz" \
    "$XWIN_SHA256" "$tmp/xwin.tar.gz"
  tar -xzf "$tmp/xwin.tar.gz" -C "$tmp"
  install -m 0755 "$tmp/cargo-xwin" "$HOME/.cargo/bin/cargo-xwin"
fi

# Host-Linker nur setzen, wenn kein echter cc vorhanden ist
if ! command -v cc >/dev/null && ! command -v gcc >/dev/null; then
  echo "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=$OPT/zigcc/cc" >> "${GITHUB_ENV:-/dev/null}"
  echo "Host-Linker: $OPT/zigcc/cc (export CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=$OPT/zigcc/cc)"
fi
echo "$HOME/.cargo/bin" >> "${GITHUB_PATH:-/dev/null}"
"$HOME/.cargo/bin/rustc" --version
"$HOME/.cargo/bin/cargo-xwin" --version
