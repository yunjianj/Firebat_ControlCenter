# 从当前工作区构建本地包；请在仓库根目录运行 makepkg。
pkgname=firebat-control-center
pkgver=0.6.1
pkgrel=1
pkgdesc='Firebat GX Linux 控制中心（CLI 和 GUI）'
arch=('x86_64')
url='https://github.com/yunjianj/Firebat_ControlCenter'
license=('GPL-2.0-or-later')
depends=('gcc-libs' 'glibc' 'fontconfig' 'freetype2' 'libxkbcommon' 'wayland' 'libx11' 'libxcursor' 'libxi' 'libxrandr' 'libxkbcommon-x11' 'libgl' 'polkit' 'hicolor-icon-theme')
makedepends=('rust' 'pkgconf')
options=('!debug')
source=()
sha256sums=()

build() {
    cd "$startdir"
    CARGO_TARGET_DIR="$startdir/target" cargo build --release --locked --offline
}

package() {
    install -Dm755 "$startdir/target/release/firebat-cc" "$pkgdir/usr/bin/firebat-cc"
    install -Dm755 "$startdir/target/release/firebat-cc-gui" "$pkgdir/usr/bin/firebat-cc-gui"
    install -Dm644 "$startdir/packaging/firebat-cc-gui.desktop" "$pkgdir/usr/share/applications/firebat-cc-gui.desktop"
    install -Dm644 "$startdir/gui/ui/assets/logo.png" "$pkgdir/usr/share/icons/hicolor/256x256/apps/firebat-cc-gui.png"
}
