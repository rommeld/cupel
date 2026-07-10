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
# The checksums below are the REAL values for v0.1.3-beta, fetched from that
# release's sha256sums.txt - this formula is publishable as-is.

class Cupel < Formula
  desc "Lean Rust coding agent with provider-neutral inference and a TUI"
  homepage "https://github.com/rommeld/cupel"
  version "0.1.3-beta"
  license "MIT"

  on_macos do
    url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-macos-universal.tar.gz"
    sha256 "26ea08bed6631c92dfdaafa228d205cb601daf745d10c24ab80e0ba06b344e36"
  end

  on_linux do
    on_intel do
      url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-linux-x86_64.tar.gz"
      sha256 "6b3c76cfe479256737838efe33356af953143ffc9c0db9d90441fe67afda8857"
    end
    on_arm do
      url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-linux-aarch64.tar.gz"
      sha256 "db4634c57468607d1492f601ca7c21e189368ec860300a43762ce4fddad34fa3"
    end
  end

  def install
    bin.install "cupel"
  end

  test do
    assert_match "usage: cupel", shell_output("#{bin}/cupel --help")
  end
end
