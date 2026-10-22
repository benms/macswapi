# This repository is its own Homebrew tap:
#   brew tap benms/macswapi https://github.com/benms/macswapi
#   brew install --HEAD macswapi
# After a release is tagged, add `url` and `sha256` for its tarball so plain
# `brew install macswapi` works.
class Macswapi < Formula
  desc "Per-process memory, compressor and swap inspector for macOS"
  homepage "https://github.com/benms/macswapi"
  license "MIT"
  head "https://github.com/benms/macswapi.git", branch: "main"

  depends_on "rust" => :build
  depends_on :macos

  def install
    system "cargo", "install", *std_cargo_args
    generate_completions_from_executable(bin/"macswapi", "--completions")
  end

  test do
    assert_match "macswapi [N]", shell_output("#{bin}/macswapi --help")
    assert_match "pid,ppid,name,cmprs", shell_output("#{bin}/macswapi 1 --csv")
    assert_match "#compdef macswapi", shell_output("#{bin}/macswapi --completions zsh")
  end
end
