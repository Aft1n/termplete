class Termplete < Formula
  desc "Terminal autocomplete that learns your choices"
  homepage "https://github.com/aft1n/termplete"
  version "0.1.1"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.1/termplete-0.1.1-aarch64-apple-darwin.tar.gz"
      sha256 "8c5fa98038c7fcead0b4ba0e335005c55d4ede75dd8a2ebd5840df64f113a527"
    elsif Hardware::CPU.intel?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.1/termplete-0.1.1-x86_64-apple-darwin.tar.gz"
      sha256 "d441abc034f3d5a1f670de8f1867a4397025c2f16d78b770858e8ef5061ae7a7"
    else
      odie "TermPlete supports Apple Silicon and Intel Macs"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.1/termplete-0.1.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "18b842db280cf9c6d6b839bd58659c48ececf4d0af548f5f7501e87ea8ffb458"
    elsif Hardware::CPU.intel?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.1/termplete-0.1.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "1ca191f2af2201fcc6167094e6a0ed990a39377af6ca3a3667cdb2654921f3d1"
    else
      odie "TermPlete supports ARM64 and x86_64 Linux"
    end
  end

  def install
    bin.install "termplete", "tp"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/termplete --version")
    assert_match version.to_s, shell_output("#{bin}/tp --version")
  end
end
