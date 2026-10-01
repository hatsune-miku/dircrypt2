"""Benchmark the Rust CLI on generated files, then verify every restored byte.

The input directory is newly allocated beneath --parent. It is never an existing
user tree. Successful fixtures are retained for inspection; remove only the
reported .dircrypt-bench-v2-* directory after saving the report.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from itertools import islice


def checked_parallel(items, function, label, total):
    iterator = iter(items)
    done, last = 0, time.perf_counter()
    with ThreadPoolExecutor(max_workers=16) as pool:
        while chunk := list(islice(iterator, 1024)):
            for value in pool.map(function, chunk):
                yield value
            done += len(chunk)
            if time.perf_counter() - last >= 2:
                print(f"{label} {done}/{total}", flush=True)
                last = time.perf_counter()


def relative(index, shape):
    if shape == "flat":
        return f"original-file-with-a-long-name-{index:07}.bin"
    if index < 8192:
        return f".cache/wide/file-{index:07}.bin"
    if index < 8320:
        return f"root-file-{index:07}.bin"
    package, slot = divmod(index - 8320, 48)
    group = "src" if slot < 24 else "dist" if slot < 40 else ".tests"
    return (f"node_modules/.pnpm/package-{package:05}@1.0.0/node_modules/"
            f"@scope/package-{package:05}/{group}/file-{slot:02}-数据.js")


def payload(index):
    seed = index.to_bytes(8, "little") + hashlib.sha256(str(index).encode()).digest()
    size = 8192 if index % 97 == 0 else 128
    return (seed * ((size + len(seed) - 1) // len(seed)))[:size]


def run(args):
    base = Path(args.fixture).resolve() if args.fixture else Path(tempfile.mkdtemp(prefix=".dircrypt-bench-v2-", dir=args.parent)).resolve()
    if args.fixture:
        if (base.parent != Path(args.parent).resolve() or not base.name.startswith(".dircrypt-bench-v2-")
                or (base / "GENERATED-BY-BENCHMARK").read_text() != "dircrypt native benchmark\n"):
            raise ValueError("--fixture must name a generated benchmark directory beneath --parent")
    root = base / "input"
    root.mkdir(exist_ok=bool(args.fixture))
    print(f"Fixture: {base}", flush=True)
    (base / "GENERATED-BY-BENCHMARK").write_text("dircrypt native benchmark\n")
    last = time.perf_counter()
    directories = set()
    for i in range(0 if args.fixture else args.files):
        path = root / relative(i, args.shape)
        if path.parent not in directories:
            path.parent.mkdir(parents=True, exist_ok=True)
            directories.add(path.parent)
        path.write_bytes(payload(i))
        if time.perf_counter() - last >= 2:
            print(f"Generating {i + 1}/{args.files}", flush=True)
            last = time.perf_counter()
    if not args.fixture:
        (root / ".bin" / "empty-directory").mkdir(parents=True)
    results = []
    executable = str(Path(args.exe).resolve())
    for round_number in range(args.rounds):
        pair = {}
        for action in ("map", "restore"):
            report = base / f"round-{round_number}-{action}.json"
            reuse_map = bool(args.fixture and round_number == 0 and action == "map" and (root / "DCDATA").is_dir())
            if report.exists() and not reuse_map:
                report = base / f"repeat-{time.time_ns()}-{action}.json"
            command = [executable, str(root), "--jobs", str(args.jobs), "--report", str(report)]
            if args.obfuscate and action == "map":
                command.append("--obfuscate")
            started = time.perf_counter()
            if not reuse_map:
                process = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                           text=True, encoding="utf-8", errors="replace")
                with report.with_suffix(".log").open("w", encoding="utf-8") as log:
                    for line in process.stdout:
                        print(line.rstrip(), flush=True)
                        log.write(line)
                if process.wait() != 0:
                    raise RuntimeError(f"{action} failed; fixture retained at {base}")
            pair[action] = json.loads(report.read_text())
            if not reuse_map:
                pair[action]["process_seconds"] = time.perf_counter() - started
            if pair[action]["files"] != args.files:
                raise AssertionError("Unexpected processed file count")
            if action == "map" and not args.obfuscate:
                # Check the entire multiset of bytes, including at the mapped stage.
                expected = sorted(hashlib.sha256(payload(i)).digest() for i in range(args.files))
                paths = (Path(parent) / name for parent, _, names in os.walk(root / "DCDATA" / "data") for name in names)
                actual = list(checked_parallel(paths, lambda p: hashlib.sha256(p.read_bytes()).digest(), "Verifying mapped bytes", args.files))
                if sorted(actual) != expected:
                    raise AssertionError("Mapped file contents changed")
        print("Verifying every restored filename and byte...", flush=True)
        def verify_one(i):
            if (root / relative(i, args.shape)).read_bytes() != payload(i):
                raise AssertionError(f"Restored bytes differ at {i}")
        for _ in checked_parallel(range(args.files), verify_one, "Verifying restored bytes", args.files):
            pass
        if (root / "DCDATA").exists() or not (root / ".bin/empty-directory").is_dir():
            raise AssertionError("Incomplete directory restoration")
        pair["verified_files"] = args.files
        results.append(pair)
    output = {"implementation": "Rust dircrypt 2", "platform": platform.platform(),
              "files": args.files, "shape": args.shape, "obfuscate": args.obfuscate,
              "jobs_argument": args.jobs, "rounds": results,
              "notes": "Generated 128/8192-byte files. Fixture generation and byte verification excluded. Warm filesystem cache; no cache flush between phases."}
    destination = Path(args.output)
    with destination.open("x", encoding="utf-8") as stream:
        json.dump(output, stream, ensure_ascii=False, indent=2)
    print(f"Saved {destination}; generated fixture retained at {base}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", required=True)
    parser.add_argument("--fixture", help="Reuse a directory previously generated by this benchmark")
    parser.add_argument("--exe", default="target/release/dircrypt.exe" if os.name == "nt" else "target/release/dircrypt")
    parser.add_argument("--files", type=int, default=130000)
    parser.add_argument("--shape", choices=("mixed", "flat"), default="mixed")
    parser.add_argument("--jobs", type=int, default=0)
    parser.add_argument("--rounds", type=int, default=1)
    parser.add_argument("--obfuscate", action="store_true")
    parser.add_argument("--output", required=True)
    run(parser.parse_args())
