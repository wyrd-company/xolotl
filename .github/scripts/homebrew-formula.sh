#!/usr/bin/env bash
# Print the Homebrew formula of a release, from its archives' .sha256 files.
#
#   homebrew-formula.sh <version> <directory with the .sha256 files> [base URL]
#
# Fails when a file is missing or has no digest, so a partial release can't
# publish a formula that points at nothing.
set -euo pipefail

version=${1:?version}
dir=${2:?directory}
base=${3:-https://github.com/wyrd-company/xolotl/releases/download/$version}

digest() {
  local file="$dir/xolotl-$version-$1.tar.gz.sha256" value
  value=$(cut -d' ' -f1 "$file")
  if [[ ! $value =~ ^[0-9a-f]{64}$ ]]; then
    echo "no SHA-256 digest in $file" >&2
    return 1
  fi
  printf '%s' "$value"
}

mac_arm=$(digest aarch64-apple-darwin)
linux_arm=$(digest aarch64-unknown-linux-gnu)
linux_intel=$(digest x86_64-unknown-linux-gnu)

cat <<FORMULA
class Xolotl < Formula
  desc "LLM subscription proxy and AI gateway backed by OpenBao"
  homepage "https://github.com/wyrd-company/xolotl"
  version "$version"
  license "Apache-2.0"

  on_macos do
    depends_on arch: :arm

    url "$base/xolotl-$version-aarch64-apple-darwin.tar.gz"
    sha256 "$mac_arm"
  end

  on_linux do
    on_arm do
      url "$base/xolotl-$version-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_arm"
    end

    on_intel do
      url "$base/xolotl-$version-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "$linux_intel"
    end
  end

  def install
    bin.install "xolotl"
    prefix.install_metafiles
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/xolotl --version")
  end
end
FORMULA
