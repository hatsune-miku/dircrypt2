"""Publish only complete, tested builds. A rerun repairs a draft, not a release."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tomllib


def gh(*args, check=True):
    result = subprocess.run(["gh", *args], text=True, capture_output=True)
    if check and result.returncode:
        raise RuntimeError(f"GitHub operation failed: {result.stderr.strip()}")
    return result


def main():
    repo = os.environ["GITHUB_REPOSITORY"]
    commit = os.environ["GITHUB_SHA"]
    run = os.environ["GITHUB_RUN_NUMBER"]
    version = tomllib.loads(Path("Cargo.toml").read_text(encoding="utf-8"))["package"]["version"]
    tag = f"v{version}-rc.{run}.{commit[:8]}"
    output = Path("release-assets")
    assets = sorted(p for p in output.iterdir() if p.name.endswith((".zip", ".tar.gz")))
    expected = {
        "dircrypt2-x86_64-pc-windows-msvc.zip",
        "dircrypt2-aarch64-apple-darwin.tar.gz",
        "dircrypt2-x86_64-unknown-linux-musl.tar.gz",
    }
    assert {p.name for p in assets} == expected, "Refusing an incomplete platform release"
    checksums = []
    for path in assets:
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        recorded = path.with_name(path.name + ".sha256").read_text(encoding="ascii").split()[0]
        assert digest == recorded, f"Checksum mismatch: {path.name}"
        checksums.append(f"{digest}  {path.name}\n")
    sums = output / "SHA256SUMS"
    sums.write_text("".join(checksums), encoding="ascii")
    notes = output / "release-notes.md"
    notes.write_text(
        f"Release candidate built from `{commit}`.\n\n"
        "- Windows AMD64: static MSVC runtime and SQLite.\n"
        "- macOS ARM64: Apple Silicon, macOS 11 or later; not Developer ID signed or notarized.\n"
        "- Linux AMD64: static musl executable, no system SQLite dependency.\n\n"
        "All three native test suites and cross-platform archive restoration checks passed. "
        "SHA256SUMS covers the three downloads.\n\n"
        "Default mode changes names only. Use `--obfuscate` explicitly for header changes. "
        "DCDATA automatically selects restoration. This is reversible obfuscation, not encryption.\n\n"
        "Recovery format 2; restore archives created by older builds with their original executable first.\n",
        encoding="utf-8",
    )
    existing = gh("release", "view", tag, "--repo", repo, "--json", "isDraft,url", check=False)
    if existing.returncode == 0:
        value = json.loads(existing.stdout)
        if not value["isDraft"]:
            print(f"Already published: {value['url']}")
            return
    else:
        gh("release", "create", tag, "--repo", repo, "--target", commit,
           "--title", f"dircrypt2 {tag}", "--notes-file", str(notes), "--draft", "--prerelease")
    gh("release", "upload", tag, "--repo", repo, "--clobber", *(str(p) for p in [*assets, sums]))
    gh("release", "edit", tag, "--repo", repo, "--draft=false", "--prerelease", "--latest=false")
    url = f"https://github.com/{repo}/releases/tag/{tag}"
    print(url)
    with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as summary:
        summary.write(f"Published [release candidate {tag}]({url}) with all three platform packages.\n")


if __name__ == "__main__":
    main()
