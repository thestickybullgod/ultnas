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
#   index.html   the landing page, from index.html.in beside this script,
#                with the latest version and the package list filled in
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)

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
# The landing page. The newest version of each package goes in the
# manifest, and ultnas's newest version is the headline release, so the page
# follows every release without editing.
declare -A newest=()
for f in "pool/$component/u/ultnas"/*.deb; do
    name=$(dpkg-deb -f "$f" Package)
    ver=$(dpkg-deb -f "$f" Version)
    if [ -z "${newest[$name]:-}" ] || dpkg --compare-versions "$ver" gt "$(dpkg-deb -f "${newest[$name]}" Version)"; then
        newest[$name]=$f
    fi
done
rows=""
for name in $(printf '%s\n' "${!newest[@]}" | sort); do
    f=${newest[$name]}
    ver=$(dpkg-deb -f "$f" Version)
    size=$(( ($(stat -c %s "$f") + 1023) / 1024 ))
    desc=$(dpkg-deb -f "$f" Description | head -n 1 | sed 's/&/\&amp;/g; s/</\&lt;/g; s/>/\&gt;/g')
    rows+="          <tr><td class=\"pkg\">$name</td><td class=\"ver\">$ver</td><td class=\"size\">$size KiB</td><td>$desc</td></tr>"$'\n'
done
latest=$(dpkg-deb -f "${newest[ultnas]}" Version)
latest=${latest%-*}   # 0.2.0-1 → 0.2.0: the release, without the package revision

page=$(<"$here/index.html.in")
page=${page//@VERSION@/"$latest"}
page=${page//@BUILT@/"$(date -u +%Y-%m-%d)"}
page=${page//@FINGERPRINT@/"$fp_spaced"}
page=${page//@PACKAGES@/"${rows%$'\n'}"}
printf '%s\n' "$page" > index.html

echo "Built $out: $(ls "pool/$component/u/ultnas" | wc -l) package(s), signed by $fpr"
