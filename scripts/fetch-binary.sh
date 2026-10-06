#!/bin/sh
# Build step of the Herdr DB plugin: installs bin/herdr-db.
#
# Downloads the release archive matching the manifest version for this OS and
# architecture, verifies its SHA-256, and installs the binary atomically.
# Without a matching release (development branch), builds with cargo when it
# is available, and fails with a clear message otherwise.
set -eu

REPO="samirsid-dev/herdr-db"
ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"

say() { printf 'herdr-db: %s\n' "$*" >&2; }
fail() { say "$*"; exit 1; }

VERSION=$(sed -n 's/^version *= *"\(.*\)"/\1/p' herdr-plugin.toml | head -n 1)
[ -n "$VERSION" ] || fail "version introuvable dans herdr-plugin.toml"

case "$(uname -s)" in
  Darwin) OS=apple-darwin ;;
  Linux) OS=unknown-linux-musl ;;
  *) fail "système non pris en charge : $(uname -s) (macOS et Linux uniquement)" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=aarch64 ;;
  x86_64|amd64) ARCH=x86_64 ;;
  *) fail "architecture non prise en charge : $(uname -m)" ;;
esac
TARGET="$ARCH-$OS"
ARCHIVE="herdr-db-$TARGET.tar.gz"
URL="https://github.com/$REPO/releases/download/v$VERSION/$ARCHIVE"

mkdir -p bin

# Already installed at the right version: nothing to do.
if [ -x bin/herdr-db ] && bin/herdr-db --version 2>/dev/null | grep -q " $VERSION\$"; then
  say "herdr-db $VERSION déjà installé"
  exit 0
fi

sha256() {
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    fail "ni shasum ni sha256sum : impossible de vérifier le binaire"
  fi
}

download() {
  if command -v curl >/dev/null 2>&1; then
    curl -fsSL --retry 2 -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1"
  else
    fail "ni curl ni wget pour télécharger le binaire"
  fi
}

build_from_source() {
  if ! command -v cargo >/dev/null 2>&1; then
    fail "aucune release v$VERSION pour $TARGET et cargo est absent : installez une version publiée (herdr plugin install $REPO)"
  fi
  say "pas de release v$VERSION : compilation avec cargo (quelques minutes)"
  cargo build --release --locked -p herdr-db
  cp target/release/herdr-db bin/herdr-db.tmp
  chmod 755 bin/herdr-db.tmp
  mv -f bin/herdr-db.tmp bin/herdr-db
  say "herdr-db compilé depuis les sources"
}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

say "téléchargement de $ARCHIVE (v$VERSION)"
if ! download "$URL" "$WORK/$ARCHIVE" 2>/dev/null; then
  build_from_source
  exit 0
fi
download "$URL.sha256" "$WORK/$ARCHIVE.sha256" || fail "empreinte SHA-256 introuvable pour $ARCHIVE"
EXPECTED=$(cut -d ' ' -f 1 < "$WORK/$ARCHIVE.sha256")
ACTUAL=$(sha256 "$WORK/$ARCHIVE")
[ "$EXPECTED" = "$ACTUAL" ] || fail "empreinte SHA-256 invalide pour $ARCHIVE : installation refusée"

tar -xzf "$WORK/$ARCHIVE" -C "$WORK"
BINARY=$(find "$WORK" -type f -name herdr-db | head -n 1)
[ -n "$BINARY" ] || fail "binaire absent de l'archive $ARCHIVE"
# Atomic replace: running panes keep their old inode.
cp "$BINARY" bin/herdr-db.tmp
chmod 755 bin/herdr-db.tmp
mv -f bin/herdr-db.tmp bin/herdr-db
say "herdr-db $VERSION installé"
