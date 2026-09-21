#!/bin/sh
# Build (or update) a signed apt repository from cargo-deb output.
#   build-apt-repo.sh <dist dir with */tornas.deb> <site dir> <gpg key id>
# The site dir may already contain a previous repository (gh-pages checkout);
# old packages in pool/ are kept so downgrades remain possible.
set -eu
DIST=$1; SITE=$2; KEYID=$3
SUITE=stable; COMPONENT=main
mkdir -p "$SITE/pool/$COMPONENT/t/tornas"
for deb in "$DIST"/*/tornas.deb; do
  [ -f "$deb" ] || continue
  ver=$(dpkg-deb -f "$deb" Version); arch=$(dpkg-deb -f "$deb" Architecture)
  cp "$deb" "$SITE/pool/$COMPONENT/t/tornas/tornas_${ver}_${arch}.deb"
done
cd "$SITE"
rm -rf .git
for arch in amd64 arm64 armhf; do
  d="dists/$SUITE/$COMPONENT/binary-$arch"; mkdir -p "$d"
  dpkg-scanpackages --arch "$arch" pool/ > "$d/Packages"
  gzip -9fk "$d/Packages"
done
apt-ftparchive \
  -o "APT::FTPArchive::Release::Origin=tornas" \
  -o "APT::FTPArchive::Release::Label=tornas" \
  -o "APT::FTPArchive::Release::Suite=$SUITE" \
  -o "APT::FTPArchive::Release::Codename=$SUITE" \
  -o "APT::FTPArchive::Release::Components=$COMPONENT" \
  -o "APT::FTPArchive::Release::Architectures=amd64 arm64 armhf" \
  release "dists/$SUITE" > "dists/$SUITE/Release"
PASS=${APT_GPG_PASSPHRASE:-}
gpg --batch --yes --pinentry-mode loopback --passphrase "$PASS" -u "$KEYID" --clearsign -o "dists/$SUITE/InRelease" "dists/$SUITE/Release"
gpg --batch --yes --pinentry-mode loopback --passphrase "$PASS" -u "$KEYID" -abs -o "dists/$SUITE/Release.gpg" "dists/$SUITE/Release"
gpg --batch --export "$KEYID" > tornas.gpg
gpg --batch --armor --export "$KEYID" > tornas.asc
cat > index.html <<'HTML'
<!doctype html><title>tornas apt repository</title>
<h1>tornas apt repository</h1>
<pre>curl -fsSL https://mridang.github.io/tornas/tornas.gpg | sudo tee /usr/share/keyrings/tornas.gpg >/dev/null
echo "deb [signed-by=/usr/share/keyrings/tornas.gpg] https://mridang.github.io/tornas stable main" | sudo tee /etc/apt/sources.list.d/tornas.list
sudo apt update &amp;&amp; sudo apt install tornas</pre>
<p><a href="pool/main/t/tornas/">packages</a> · <a href="https://github.com/mridang/tornas">source</a></p>
HTML
touch .nojekyll
echo "repository built in $SITE:"; find dists pool -type f | sort
