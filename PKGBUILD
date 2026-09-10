# Maintainer: Legambey <ton.email@example.com>
pkgname=vidio
pkgver=0.1.0
pkgrel=1
pkgdesc="Moniteur vidéo/audio basse latence pour cartes d'acquisition USB"
arch=('x86_64' 'aarch64')
url="https://github.com/Legambey/VidIO"
license=('GPL-3.0-or-later')
depends=('alsa-lib' 'gcc-libs' 'glibc' 'libxkbcommon' 'vulkan-icd-loader')
makedepends=('cargo')
options=('!lto')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
sha256sums=('SKIP')

prepare() {
  cd "VidIO-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
  cd "VidIO-$pkgver"
  export RUSTUP_TOOLCHAIN=stable
  export CARGO_TARGET_DIR=target
  cargo build --frozen --release
}

package() {
  cd "VidIO-$pkgver"
  install -Dm0755 -t "$pkgdir/usr/bin/" target/release/vidio
  install -Dm0644 LICENSE "$pkgdir/usr/share/licenses/$pkgname/LICENSE"
  install -Dm0644 -t "$pkgdir/usr/share/doc/$pkgname/" README.md
}
