class Bitty < Formula
  desc "Bitty pre-alpha terminal workspace minimal correct terminal"
  homepage "https://github.com/bitty-terminal/bitty"
  url "https://github.com/bitty-terminal/bitty/archive/v0.0.1.tar.gz"
  sha256 "SKIP"
  license any_of: ["MIT", "Apache-2.0"]
  version "0.0.1"

  depends_on "pkg-config" => :build
  depends_on "rust" => :build
  depends_on "fontconfig"
  depends_on "freetype"

  def install
    system "cargo", "build", "--release", "--locked", "-p", "bitty-app"
    bin.install "target/release/bitty-app" => "bitty"
    doc.install "README.md"
    doc.install "CHANGELOG.md"
    (share/"licenses/bitty").install "LICENSE"
  end

  test do
    assert_match "0.0.1", shell_output("#{bin}/bitty --version")
    assert_match "bitty", shell_output("#{bin}/bitty --help")
  end
end
