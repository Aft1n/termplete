class Termplete < Formula
  desc "Terminal autocomplete that learns your choices"
  homepage "https://github.com/aft1n/termplete"
  version "0.1.0"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.0/termplete-0.1.0-aarch64-apple-darwin.tar.gz"
      sha256 "f2c2f04d881ec0d733cde1c4a4557395badb7b74096e03f70e66b60bfef325ae"
    elsif Hardware::CPU.intel?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.0/termplete-0.1.0-x86_64-apple-darwin.tar.gz"
      sha256 "d955f758538bd4cf41485b8264475c95491e09d63eaa979371b4a5f9efbdbbf0"
    else
      odie "TermPlete supports Apple Silicon and Intel Macs"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.0/termplete-0.1.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "d14824e40cf41852d559d6113ff67ba2efdf802b2f070ee9836aae1d0f439bdf"
    elsif Hardware::CPU.intel?
      url "https://github.com/aft1n/termplete/releases/download/v0.1.0/termplete-0.1.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "2ab734e1b776222b7bc21405df9f14fc8e2f5d5c25060ccfafd3849fd3f06192"
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
