# Homebrew formula for cupel - a BINARY formula: it downloads release
# archives instead of compiling, so `brew install` needs no Rust toolchain.
#
# This file is the TEMPLATE that lives with the code. To publish it:
#   1. Create a repository named `homebrew-tap` under your GitHub account.
#   2. Copy this file to `Formula/cupel.rb` in that repository.
#   3. After each release, update `version` and the four sha256 values from
#      the release's sha256sums.txt (see packaging/README.md for a one-liner).
# Users then install with:
#   brew install rommeld/tap/cupel

class Cupel < Formula
  desc "Lean Rust coding agent with provider-neutral inference and a TUI"
  homepage "https://github.com/rommeld/cupel"
  version "0.1.0"
  license "MIT"

  on_macos do
    url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-macos-universal.tar.gz"
    sha256 "REPLACE_WITH_MACOS_SHA256"
  end

  on_linux do
    on_intel do
      url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-linux-x86_64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_X86_64_SHA256"
    end
    on_arm do
      url "https://github.com/rommeld/cupel/releases/download/v#{version}/cupel-linux-aarch64.tar.gz"
      sha256 "REPLACE_WITH_LINUX_AARCH64_SHA256"
    end
  end

  def install
    bin.install "cupel"
  end

  test do
    assert_match "usage: cupel", shell_output("#{bin}/cupel --help")
  end
end
