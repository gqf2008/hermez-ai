class Hermes < Formula
  desc "Self-evolving AI agent system"
  homepage "https://github.com/NousResearch/hermes-agent"
  url "https://github.com/NousResearch/hermes-agent/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "PLACEHOLDER"
  license "MIT"
  head "https://github.com/NousResearch/hermes-agent.git", branch: "main"

  depends_on "rust" => :build
  depends_on "pkg-config" => :build
  depends_on "cmake" => :build
  depends_on "openssl@3"
  depends_on "sqlite"

  def install
    system "cargo", "install", *std_cargo_args(path: ".")

    # Install auxiliary binaries
    bin.install "target/release/hermes-agent"
    bin.install "target/release/hermes-acp"

    # Install web dashboard assets
    pkgshare.install "web/dist"

    # Install skills and docs
    pkgshare.install "skills"
    doc.install "docs"
  end

  def post_install
    (var/"log/hermes").mkpath
  end

  service do
    run [opt_bin/"hermes", "web", "serve"]
    keep_alive true
    log_path var/"log/hermes/web.log"
    error_log_path var/"log/hermes/web.error.log"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/hermes --version")
    system "#{bin}/hermes", "config", "check"
  end
end
