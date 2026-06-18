# Maintainer: Christian Balcom <robot.inventor@gmail.com>

pkgname=prism-widgets
pkgver=0.1.0
pkgrel=1
pkgdesc='Configurable Damascene layer-shell information panels for Wayland status surfaces'
arch=('x86_64')
url='https://github.com/computer-whisperer/prism-widgets'
license=('MIT OR Apache-2.0')
# libwayland-client and libvulkan are dlopened at runtime (wayland-sys via
# dlib, ash), so neither shows up as NEEDED — both are real dependencies.
depends=(
    'gcc-libs'
    'glibc'
    'vulkan-icd-loader'
    'wayland'
)
# Optional runtime helpers, invoked only when the matching module is
# configured: `gh` for the github module, `sh` for command modules.
optdepends=(
    'github-cli: github module status via `gh api`'
)
makedepends=('cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/refs/tags/v$pkgver.tar.gz")
# Run `updpkgsums` once the v0.1.0 tag exists on GitHub.
sha256sums=('SKIP')

prepare() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo fetch --locked --target "$(rustc -vV | sed -n 's/host: //p')"
}

build() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    export CARGO_TARGET_DIR=target
    cargo build --release --frozen
}

check() {
    cd "$pkgname-$pkgver"
    export RUSTUP_TOOLCHAIN=stable
    cargo test --release --frozen
}

package() {
    cd "$pkgname-$pkgver"
    install -Dm755 "target/release/prism-widgets" "$pkgdir/usr/bin/prism-widgets"
    install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
    install -Dm644 LICENSE-MIT "$pkgdir/usr/share/licenses/$pkgname/LICENSE-MIT"
    install -Dm644 LICENSE-APACHE "$pkgdir/usr/share/licenses/$pkgname/LICENSE-APACHE"
}
