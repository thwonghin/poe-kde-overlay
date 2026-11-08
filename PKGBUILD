# Maintainer: Hin Wong <fraywong@gmail.com>
pkgname=poe-kde-overlay
pkgver=0.1.0
pkgrel=1
pkgdesc="Native KWin/Wayland backend for Awakened PoE Trade (PoE1) and Exiled Exchange 2 (PoE2)"
arch=('x86_64')
url="https://github.com/thwonghin/poe-kde-overlay"
license=('MIT')
depends=('gtk4' 'gtk4-layer-shell' 'webkitgtk-6.0')
makedepends=('git' 'rust' 'nodejs' 'npm')
source=("$pkgname::git+https://github.com/thwonghin/poe-kde-overlay.git")
sha256sums=('SKIP')

pkgver() {
  cd "$pkgname"
  printf "r%s.%s" "$(git rev-list --count HEAD)" "$(git rev-parse --short HEAD)"
}

prepare() {
  cd "$pkgname"
  git submodule update --init --recursive
}

build() {
  cd "$pkgname"
  ./scripts/build-renderer.sh
  # -flto=auto in CFLAGS/CXXFLAGS makes GCC compile the `ring` crate's C code
  # (via its build script) into "fat" LTO objects. GCC's collect2 then
  # auto-detects the LTO bytecode sections in that static archive and enters
  # LTO mode for the final link regardless of LDFLAGS — which breaks badly
  # when mixed with Rust's LLVM-based objects under ld.lld, silently dropping
  # ring_core symbols and producing "undefined symbol" errors. Verified by
  # bisecting the flag set directly: stripping -flto=auto here is sufficient
  # and necessary; every other hardening flag can stay.
  CFLAGS="${CFLAGS/-flto=auto/}" CXXFLAGS="${CXXFLAGS/-flto=auto/}" \
    cargo build --release --locked
}

check() {
  cd "$pkgname"
  # Same -flto=auto stripping as build(): the test binary links ring too.
  # --release reuses build()'s compiled dependencies.
  CFLAGS="${CFLAGS/-flto=auto/}" CXXFLAGS="${CXXFLAGS/-flto=auto/}" \
    cargo test --release --locked
}

package() {
  cd "$pkgname"
  install -Dm755 target/release/poe-kde-overlay "$pkgdir/usr/bin/poe-kde-overlay"
  install -d "$pkgdir/usr/share/poe-kde-overlay"
  cp -r vendor/awakened-poe-trade/renderer/dist "$pkgdir/usr/share/poe-kde-overlay/poe1"
  cp -r vendor/exiled-exchange-2/renderer/dist "$pkgdir/usr/share/poe-kde-overlay/poe2"
  install -Dm644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm644 data/io.github.thwonghin.PoeKdeOverlay.desktop \
    "$pkgdir/usr/share/applications/io.github.thwonghin.PoeKdeOverlay.desktop"
}
