class Grace < Formula
  desc "Production-grade agent CLI - ReAct agent with memory, sessions, skills"
  homepage "https://github.com/AmnGrg0511/grace"
  license "MIT OR Apache-2.0"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.9/grace-aarch64-apple-darwin.tar.gz"
      sha256 "4027550bc62edac39d42a12a8e172ef2adcaba28c21da97b0be3713cea501f5c"
    else
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.9/grace-x86_64-apple-darwin.tar.gz"
      sha256 "be00959a548f2323ef9e82cbbe8da82e11ea3f9a1fc02fade64c08116e565938"
    end
  end

  on_linux do
    if Hardware::CPU.arm?
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.9/grace-aarch64-unknown-linux-musl.tar.gz"
      sha256 "bfa5ec272344546169fa66c5e66762ba3204661a77df5618995861a52b9ac502"
    else
      url "https://github.com/AmnGrg0511/grace/releases/download/v0.1.9/grace-x86_64-unknown-linux-musl.tar.gz"
      sha256 "bcf6a5ae9588c547007f1294d37843f1e53f544426569846a95fbedc7fc3c3ee"
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