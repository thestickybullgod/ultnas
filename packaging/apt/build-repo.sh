#!/usr/bin/env bash
# Build a signed APT repository from a directory of .deb files.
#
#   build-repo.sh <debs-dir> <out-dir> <gpg-fingerprint>
#
# The signing key must already be in the GPG keyring. If $GPG_PASSPHRASE is
# set, it unlocks the key non-interactively.
#
# Produces, under <out-dir>:
#   pool/main/u/ultnas/*.deb
#   dists/stable/{InRelease,Release,Release.gpg}
#   dists/stable/main/binary-<arch>/{Packages,Packages.gz,Release}
#   ultnas.gpg   the public key, binary (for /usr/share/keyrings)
#   ultnas.asc   the public key, armored
#   index.html   install instructions
set -euo pipefail

debs=${1:?usage: build-repo.sh <debs-dir> <out-dir> <gpg-fingerprint>}
out=${2:?usage: build-repo.sh <debs-dir> <out-dir> <gpg-fingerprint>}
fpr=${3:?usage: build-repo.sh <debs-dir> <out-dir> <gpg-fingerprint>}
suite=stable
component=main
archs="amd64"

shopt -s nullglob
files=("$debs"/*.deb)
if [ ${#files[@]} -eq 0 ]; then
    echo "no .deb files in $debs" >&2
    exit 1
fi

rm -rf "$out"
pool="$out/pool/$component/u/ultnas"
mkdir -p "$pool"
cp "${files[@]}" "$pool/"

cd "$out"
for arch in $archs; do
    dir="dists/$suite/$component/binary-$arch"
    mkdir -p "$dir"
    apt-ftparchive --arch "$arch" packages "pool/$component" > "$dir/Packages"
    gzip -9nk "$dir/Packages"
    cat > "$dir/Release" <<EOF
Archive: $suite
Component: $component
Origin: Ultnas
Label: Ultnas
Architecture: $arch
EOF
done

apt-ftparchive \
    -o "APT::FTPArchive::Release::Origin=Ultnas" \
    -o "APT::FTPArchive::Release::Label=Ultnas" \
    -o "APT::FTPArchive::Release::Suite=$suite" \
    -o "APT::FTPArchive::Release::Codename=$suite" \
    -o "APT::FTPArchive::Release::Architectures=$archs" \
    -o "APT::FTPArchive::Release::Components=$component" \
    -o "APT::FTPArchive::Release::Description=Ultnas: protects text files from invisible-character tampering" \
    release "dists/$suite" > "dists/$suite/Release"

# A pasted secret may end in CR/LF; gpg reads up to LF, so drop a CR too.
# (Spaces are kept: a passphrase may really contain them.)
GPG_PASSPHRASE=${GPG_PASSPHRASE:-}
GPG_PASSPHRASE=${GPG_PASSPHRASE%$'\n'}
GPG_PASSPHRASE=${GPG_PASSPHRASE%$'\r'}
sign=(gpg --batch --yes --local-user "$fpr" --digest-algo SHA512)
if [ -n "${GPG_PASSPHRASE:-}" ]; then
    sign+=(--pinentry-mode loopback --passphrase-fd 0)
fi
printf '%s' "${GPG_PASSPHRASE:-}" | "${sign[@]}" --clearsign -o "dists/$suite/InRelease" "dists/$suite/Release"
printf '%s' "${GPG_PASSPHRASE:-}" | "${sign[@]}" --armor --detach-sign -o "dists/$suite/Release.gpg" "dists/$suite/Release"

gpg --batch --export "$fpr" > ultnas.gpg
gpg --batch --armor --export "$fpr" > ultnas.asc

# Everything a browser visitor needs, and a check that signing worked.
gpg --batch --verify "dists/$suite/InRelease" 2>/dev/null
fp_spaced=$(echo "$fpr" | sed -E 's/(.{4})/\1 /g; s/ $//')
cat > index.html <<EOF
<!doctype html>
<meta charset="utf-8">
<title>Ultnas APT repository</title>
<h1>Ultnas APT repository</h1>
<p>Ultnas protects text files from invisible-character tampering.
<a href="https://github.com/thestickybullgod/ultnas">Source and documentation</a>.</p>
<h2>Install (Debian 12+, Ubuntu 22.04+)</h2>
<pre>curl -fsSL https://thestickybullgod.github.io/ultnas/ultnas.gpg | sudo tee /usr/share/keyrings/ultnas.gpg &gt;/dev/null
echo "deb [signed-by=/usr/share/keyrings/ultnas.gpg] https://thestickybullgod.github.io/ultnas stable main" | sudo tee /etc/apt/sources.list.d/ultnas.list
sudo apt update
sudo apt install ultnas</pre>
<p>Signing key fingerprint: <code>$fp_spaced</code></p>
EOF

echo "Built $out: $(ls "pool/$component/u/ultnas" | wc -l) package(s), signed by $fpr"
