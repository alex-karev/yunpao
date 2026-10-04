# Maintainer: Alex Karev <alexkarev@sjtu.edu.cn>
pkgname=yunpao
pkgver=0.1.0
pkgrel=1
pkgdesc="Remote Session and Task Manager"
arch=('x86_64')
url="https://github.com/alex-karev/yunpao"
license=('GPL-3.0-or-later')
depends=('openssh' 'rsync')
makedepends=('rust' 'cargo')
source=("$pkgname-$pkgver.tar.gz::$url/archive/v$pkgver.tar.gz")
sha256sums=('SKIP')

build() {
	cd "$pkgname-$pkgver"
	cargo build --release --locked
}

package() {
	cd "$pkgname-$pkgver"
	install -Dm755 "target/release/$pkgname" "$pkgdir/usr/bin/$pkgname"
	install -Dm644 README.md "$pkgdir/usr/share/doc/$pkgname/README.md"
}
