#!/usr/bin/env bash
set -euo pipefail

version=$1
package_version=${version/-/\~}
binary="$GITHUB_WORKSPACE/target/release/dowai"
dist="$GITHUB_WORKSPACE/dist"
work="$RUNNER_TEMP/dowai-packaging"
mkdir -p "$dist" "$work/deb/DEBIAN" "$work/deb/usr/bin" "$work/debian" \
  "$work/rpm/BUILD" "$work/rpm/RPMS" "$work/rpm/SOURCES" \
  "$work/rpm/SPECS" "$work/rpm/SRPMS" "$work/makeself"

ldd "$binary" | tee "$work/ldd.txt"
if grep -q 'not found' "$work/ldd.txt"; then
  echo "Missing native libraries" >&2
  exit 1
fi

install -m 755 "$binary" "$work/deb/usr/bin/dowai"
cat > "$work/debian/control" <<'EOF'
Source: dowai
Section: utils
Priority: optional
Maintainer: DOWAI maintainers

Package: dowai
Architecture: amd64
Description: DOWAI data pipeline CLI
EOF
depends=$(cd "$work" && dpkg-shlibdeps -O "$binary")
depends=${depends#shlibs:Depends=}
test -n "$depends"
cat > "$work/deb/DEBIAN/control" <<EOF
Package: dowai
Version: $package_version
Section: utils
Priority: optional
Architecture: amd64
Maintainer: DOWAI maintainers
Depends: $depends
Description: DOWAI data pipeline CLI
EOF
dpkg-deb --build --root-owner-group "$work/deb" "$dist/dowai-$version-linux-x64.deb"
dpkg-deb -I "$dist/dowai-$version-linux-x64.deb"

rpmbuild -bb "$GITHUB_WORKSPACE/packaging/dowai.spec" \
  --define "_topdir $work/rpm" --define "package_version $package_version" \
  --define "binary $binary"
cp "$work/rpm/RPMS/x86_64/"*.rpm "$dist/dowai-$version-linux-x64.rpm"
rpm -qpR "$dist/dowai-$version-linux-x64.rpm"

install -m 755 "$binary" "$work/makeself/dowai"
install -m 755 "$GITHUB_WORKSPACE/packaging/install-linux.sh" "$work/makeself/install.sh"
makeself "$work/makeself" "$dist/dowai-$version-linux-x64.sh" \
  "DOWAI $version" ./install.sh
bash "$dist/dowai-$version-linux-x64.sh" --check
