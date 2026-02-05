class Sem < Formula
  desc "Semantic version control CLI — entity-level diffs on top of Git"
  homepage "https://github.com/Ataraxy-Labs/sem"
  url "https://github.com/Ataraxy-Labs/sem/archive/refs/tags/v0.2.0.tar.gz"
  sha256 ""
  license "MIT"

  depends_on "rust" => :build
  depends_on "pkg-config" => :build
  depends_on "libgit2"

  def install
    cd "crates" do
      system "cargo", "install", *std_cargo_args(path: "sem-cli")
    end
  end

  test do
    # sem requires a git repo to operate
    system "git", "init", "test-repo"
    cd "test-repo" do
      (testpath/"test-repo/hello.py").write <<~PYTHON
        def greet():
            print("hello")
      PYTHON
      system "git", "add", "."
      system "git", "commit", "-m", "init"

      output = shell_output("#{bin}/sem diff --commit HEAD --format json")
      json = JSON.parse(output)
      assert_equal 1, json["changes"].length
      assert_equal "function", json["changes"][0]["entityType"]
      assert_equal "greet", json["changes"][0]["entityName"]
    end
  end
end
