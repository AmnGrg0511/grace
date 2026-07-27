class Grace < Formula
  desc "Production-grade agent CLI - ReAct agent with memory, sessions, skills"
  homepage "https://github.com/AmnGrg0511/grace"
  license "MIT OR Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.0/grace-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256"
    else
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.0/grace-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.0/grace-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256"
    else
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.0/grace-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_ACTUAL_SHA256"
    end
  end

  def install
    bin.install "grace"
  end

  def caveats
    <<~EOS
      Grace installed! Run `grace --chat` to start.

      Quick start:
        grace --chat                    # Interactive mode
        grace --remember "fact"         # Store a memory
        grace --search-sessions "query" # Search history
        grace --completions bash        # Shell completions

      Add to your shell config:
        alias g='grace'
        eval "$(grace --completions bash)"

      Environment variables for security:
        export GRACE_ALLOW_DIR="/path/to/projects"
        export GRACE_TERMINAL_ALLOW="ls,cat,rg"
    EOS
  end

  test do
    assert_match "grace", shell_output("#{bin}/grace --help")
  end
end