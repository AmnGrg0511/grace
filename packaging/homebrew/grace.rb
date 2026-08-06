class Grace < Formula
  desc "Production-grade agent CLI - ReAct agent with memory, sessions, skills"
  homepage "https://github.com/AmnGrg0511/grace"
  license "MIT OR Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.2.0/grace-aarch64-apple-darwin.tar.gz"
      sha256 "6a6308c90f6375bf380fc4cec56d0ea03c6c240968047668d17e09314dd303e0"
    else
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.2.0/grace-x86_64-apple-darwin.tar.gz"
      sha256 "148683027c97174184a0368abf5a47f4da1c096ba53172426ceb01999ad37826"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.2.0/grace-aarch64-unknown-linux-musl.tar.gz"
      sha256 "898ab366d42460cb1aad6320b3e1c7f07a6778282986627f4df661b6ca35bab2"
    else
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.2.0/grace-x86_64-unknown-linux-musl.tar.gz"
      sha256 "52acf5e10758721176fd7bb7cda08df5ff388b0384f58a509f29a7da454c5ccd"
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