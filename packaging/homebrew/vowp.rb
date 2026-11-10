# typed: false
# frozen_string_literal: true

# Formula for the VOWP command-line binary.
class Vowp < Formula
  desc "Local-first supervisor for AI coding agents"
  homepage "https://github.com/syver777/vowp"
  version "__VOWP_VERSION__"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/syver777/vowp/releases/download/v#{version}/vowp-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "__SHA256_AARCH64_APPLE_DARWIN__"
    end
    on_intel do
      url "https://github.com/syver777/vowp/releases/download/v#{version}/vowp-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "__SHA256_X86_64_APPLE_DARWIN__"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/syver777/vowp/releases/download/v#{version}/vowp-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA256_AARCH64_UNKNOWN_LINUX_GNU__"
    end
    on_intel do
      url "https://github.com/syver777/vowp/releases/download/v#{version}/vowp-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "__SHA256_X86_64_UNKNOWN_LINUX_GNU__"
    end
  end

  def install
    binary = Dir["vowp-*/vowp"].first || "vowp"
    bin.install binary
    %w[README.md LICENSE].each do |name|
      path = File.join(File.dirname(binary), name)
      doc.install path if File.exist?(path)
    end
  end

  test do
    assert_match "vowp #{version}", shell_output("#{bin}/vowp --version")
  end
end
