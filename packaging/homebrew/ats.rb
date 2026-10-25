# Homebrew formula for the tap. scripts/formula.sh fills in the version
# and the checksums of the release archives.
class Ats < Formula
  desc "Queues resource-heavy commands from AI agents and starts them by available memory"
  homepage "https://github.com/Natsuki-Kaede/agent-task-scheduler"
  version "@VERSION@"
  license "GPL-3.0-only"

  on_macos do
    on_arm do
      url "https://github.com/Natsuki-Kaede/agent-task-scheduler/releases/download/v@VERSION@/ats-aarch64-apple-darwin.tar.gz"
      sha256 "@SHA256_aarch64-apple-darwin@"
    end
    on_intel do
      url "https://github.com/Natsuki-Kaede/agent-task-scheduler/releases/download/v@VERSION@/ats-x86_64-apple-darwin.tar.gz"
      sha256 "@SHA256_x86_64-apple-darwin@"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/Natsuki-Kaede/agent-task-scheduler/releases/download/v@VERSION@/ats-aarch64-unknown-linux-musl.tar.gz"
      sha256 "@SHA256_aarch64-unknown-linux-musl@"
    end
    on_intel do
      url "https://github.com/Natsuki-Kaede/agent-task-scheduler/releases/download/v@VERSION@/ats-x86_64-unknown-linux-musl.tar.gz"
      sha256 "@SHA256_x86_64-unknown-linux-musl@"
    end
  end

  def install
    bin.install "ats"
  end

  def caveats
    <<~EOS
      Run `ats install` to add the agent hooks and start the daemon at login.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/ats --version")
  end
end
