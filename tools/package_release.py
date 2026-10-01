"""Package a tested native binary without machine-specific paths."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
import zipfile


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    windows = "windows" in args.target
    stem = f"dircrypt2-{args.target}"
    with tempfile.TemporaryDirectory() as temporary:
        bundle = Path(temporary) / stem
        bundle.mkdir()
        binary = bundle / ("dircrypt.exe" if windows else "dircrypt")
        shutil.copy2(args.binary, binary)
        if not windows:
            binary.chmod(0o755)
        shutil.copy2("README.md", bundle / "README.md")
        shutil.copy2("docs/ARCHITECTURE.md", bundle / "ARCHITECTURE.md")
        (bundle / "build.json").write_text(json.dumps({
            "target": args.target,
            "commit": os.environ.get("GITHUB_SHA", "local"),
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        }, indent=2) + "\n", encoding="utf-8")
        archive = args.output / (stem + (".zip" if windows else ".tar.gz"))
        if windows:
            with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
                for path in sorted(bundle.iterdir()):
                    output.write(path, arcname=f"{stem}/{path.name}")
        else:
            with tarfile.open(archive, "w:gz") as output:
                output.add(bundle, arcname=stem)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        archive.with_name(archive.name + ".sha256").write_text(f"{digest}  {archive.name}\n", encoding="ascii")
        print(archive)


if __name__ == "__main__":
    main()
