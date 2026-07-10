# Homebrew formula for cupel - a BINARY formula: it downloads release
# archives instead of compiling, so `brew install` needs no Rust toolchain.
#
# This file is the TEMPLATE that lives with the code. To publish it:
#   1. Create a repository named `homebrew-tap` under your GitHub account.
#   2. Copy this file to `Formula/cupel.rb` in that repository.
#   3. After each release, update `version` and the three sha256 values from
#      the release's sha256sums.txt (see packaging/README.md for a one-liner).
# Users then install with:
#   brew install rommeld/tap/cupel
#
# The checksums below are the REAL values for v0.1.2-beta, fetched from that
# release's sha256sums.txt - this formula is publishable as-is.

class Cupel < Formula
  desc "Lean Rust coding agent with provider-neutral inference and a TUI"
  homepage "https://github.com/rommeld/cupel"
  version "0.1.2-beta"
  license "MIT"

  on_macos do
    url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-macos-universal.tar.gz"
    sha256 "cb92b6a07f778d289792a49a954c7296a191c1acce3ff7f47c75aa473f8b6e2a"
  end

  on_linux do
    on_intel do
      url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-linux-x86_64.tar.gz"
      sha256 "7441f25341d0a153938b3cf88887d34d8e6bd1abe05a885ca100580daec56df4"
    end
    on_arm do
      url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-linux-aarch64.tar.gz"
      sha256 "9b0303a8b8b652d66219674e3705a04adde2d36a840994e453ee62ce3c3f7f8b"
    end
  end

  def install
    bin.install "cupel"
  end

  test do
    assert_match "usage: cupel", shell_output("#{bin}/cupel --help")
  end
end
