# Homebrew formula for the tap. scripts/release.sh fills in the version
# and checksum of the release archive.
class Ats < Formula
  desc "Queues resource-heavy commands from AI agents and starts them by available memory"
  homepage "https://github.com/Natsuki-Kaede/agent-task-scheduler"
  url "https://github.com/Natsuki-Kaede/agent-task-scheduler/releases/download/v@VERSION@/ats-@VERSION@-aarch64-apple-darwin.tar.gz"
  version "@VERSION@"
  sha256 "@SHA256@"
  license "GPL-3.0-only"

  depends_on arch: :arm64
  depends_on :macos

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
