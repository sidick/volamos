# Homebrew formula for volamos.
#
# This repository doubles as its own Homebrew tap. The formula builds from
# source, so it works on Apple Silicon and Intel Macs and on Linux, and the
# binary is compiled on the user's machine, so macOS Gatekeeper never
# quarantines it. Install with:
#
#   brew tap sidick/volamos https://github.com/sidick/volamos
#   brew trust sidick/volamos
#   brew install volamos
#
# Homebrew 7 and later load formulae only from taps you have trusted, which
# is what `brew trust` records; older releases have no such command.
#
# or build the development tree directly:
#
#   brew install --HEAD volamos
#
# Releases are cut by release-please, which does not touch this file: after
# a release, update `url` (the new tag) and `sha256` below. Compute the
# checksum from the tagged tarball:
#
#   curl -fsSL https://github.com/sidick/volamos/archive/refs/tags/vX.Y.Z.tar.gz | shasum -a 256

class Volamos < Formula
  desc "Wine-style runtime for classic AmigaOS m68k CLI binaries"
  homepage "https://sidick.github.io/volamos/"
  url "https://github.com/sidick/volamos/archive/refs/tags/v0.22.0.tar.gz"
  sha256 "4713485e61625f0107723feebfd8eb291a3556c0d0225c575a36378571e450e3"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/sidick/volamos.git", branch: "main"

  depends_on "rust" => :build

  def install
    # Cargo.lock is committed; std_cargo_args passes --locked so the build
    # uses the pinned dependency graph.
    system "cargo", "install", *std_cargo_args(path: "crates/volamos")

    # A tiny AmigaOS executable the test (and a first run) can use: it
    # prints a greeting through dos.library, so running it exercises the
    # loader, the CPU and a library call.
    pkgshare.install "fixtures/hello"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/volamos --version")
    assert_match "Hello from volamos", shell_output("#{bin}/volamos #{pkgshare}/hello")
  end
end
