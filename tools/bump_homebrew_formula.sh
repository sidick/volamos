#!/usr/bin/env bash
# Point Formula/volamos.rb at a release tag: rewrite `url` and `sha256`.
#
# release-please cannot do this itself, because the checksum is of the tag's
# source tarball and the tag does not exist until the release PR is merged.
# The release-please workflow's `homebrew` job runs this after the release.
#
#   tools/bump_homebrew_formula.sh v0.23.0
#
# Prints "changed" or "unchanged" on stdout; everything else goes to stderr.
# Needs curl, and either shasum or sha256sum.
set -euo pipefail

tag=${1:?usage: bump_homebrew_formula.sh vX.Y.Z}
case $tag in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) echo "not a release tag: $tag" >&2; exit 2 ;;
esac

formula=$(dirname "$0")/../Formula/volamos.rb
url="https://github.com/sidick/volamos/archive/refs/tags/${tag}.tar.gz"

if command -v shasum >/dev/null; then sum() { shasum -a 256; }; else sum() { sha256sum; }; fi
sha=$(curl -fsSL --retry 5 --retry-all-errors "$url" | sum | cut -d' ' -f1)
[ ${#sha} -eq 64 ] || { echo "bad checksum from $url" >&2; exit 1; }

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
sed -E \
  -e "s|^(  url \")[^\"]*(\")\$|\1${url}\2|" \
  -e "s|^(  sha256 \")[0-9a-f]{64}(\")\$|\1${sha}\2|" \
  "$formula" > "$tmp"

# Both lines must have matched: a formula edited into a shape the seds
# no longer recognise should fail here, not silently publish a stale bump.
grep -qF "  url \"${url}\"" "$tmp" || { echo "no url line matched in $formula" >&2; exit 1; }
grep -qF "  sha256 \"${sha}\"" "$tmp" || { echo "no sha256 line matched in $formula" >&2; exit 1; }

if cmp -s "$formula" "$tmp"; then
  echo unchanged
else
  cat "$tmp" > "$formula"
  echo "$formula -> $tag ($sha)" >&2
  echo changed
fi
