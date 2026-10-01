"""Exercise release binaries and restore archives made on every supported OS."""
import argparse
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile


PAYLOADS = {
    "root.txt": b"portable payload, byte-for-byte",
    "node_modules/.pnpm/package/node_modules/pkg/index.js": bytes(range(256)) * 16,
    "中文/嵌套/文件.txt": b"Unicode names are portable",
    ".hidden/empty": b"",
    ".hidden/sixteen": b"0123456789abcdef",
    ".hidden/seventeen": b"0123456789abcdef!",
}


def run(binary, root, header=False):
    subprocess.run([str(binary), str(root), "--quiet", *(["--obfuscate"] if header else [])], check=True, timeout=120)


def create(binary, output, platform):
    output.mkdir(parents=True, exist_ok=True)
    for header in (False, True):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "archive"
            root.mkdir()
            for relative, contents in PAYLOADS.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(contents)
                # Whole seconds are exactly portable through tar and all three OSes.
                os.utime(path, ns=(1_700_000_000_000_000_000,) * 2)
            (root / "empty-directory/also-empty").mkdir(parents=True)
            run(binary, root, header)
            with tarfile.open(output / f"{platform}-{'header' if header else 'names'}.tar", "w") as archive:
                archive.add(root, arcname="archive")
            run(binary, root)
            verify(root)


def verify(root):
    for relative, contents in PAYLOADS.items():
        assert (root / relative).read_bytes() == contents, relative
    assert (root / "empty-directory/also-empty").is_dir()
    assert not (root / "DCDATA").exists()
    files = [p for p in root.rglob("*") if p.is_file() and p.name != ".dircrypt.lock"]
    assert len(files) == len(PAYLOADS)


def check(packages, archives):
    package = next(p for p in packages.iterdir() if p.name.endswith((".zip", ".tar.gz")))
    inputs = sorted(archives.rglob("*.tar"))
    assert len(inputs) == 6, f"Expected six archives from all three builders; got {inputs}"
    with tempfile.TemporaryDirectory() as temporary:
        unpacked = Path(temporary) / "binary"
        shutil.unpack_archive(package, unpacked)
        binary = next(unpacked.glob("*/dircrypt.exe" if os.name == "nt" else "*/dircrypt"))
        if os.name != "nt":
            binary.chmod(0o755)
        for index, source in enumerate(inputs):
            destination = Path(temporary) / f"transfer-{index}"
            with tarfile.open(source) as archive:
                archive.extractall(destination, filter="data")
            root = destination / "archive"
            run(binary, root)
            verify(root)
            print(f"Restored and verified {source.name}")


def main():
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    make = sub.add_parser("create")
    make.add_argument("binary", type=Path)
    make.add_argument("output", type=Path)
    make.add_argument("platform")
    test = sub.add_parser("check")
    test.add_argument("packages", type=Path)
    test.add_argument("archives", type=Path)
    args = parser.parse_args()
    if args.command == "create":
        create(args.binary.resolve(), args.output, args.platform)
    else:
        check(args.packages, args.archives)


if __name__ == "__main__":
    main()
