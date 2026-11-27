"""Job-parallelism measurement (design spec §4.1 H1/H5, §4.3).

Runs the release binary on one corpus with different `parallel_jobs` values and
reports the median wall time, so that the scaling hypotheses can be judged:

- H1: the still-image batch shrinks to roughly the core count. Refuted when the
  total time shortens by less than 20% even at the physical core count.
- H5: the copy-heavy batch is I/O bound and does not get faster. Refuted when
  parallelism shortens the total by 10% or more.

Every run starts from a fresh output folder and state database (otherwise the
second run would only measure reuse). Inputs are not copied: `input_dir` is set
to an absolute path.

Usage (from the repository root, on Windows so both sides see one OS):

    python rust/tests/bench_parallel.py --corpus .bench/corpus/cat1 \
        --jobs 1 2 4 8 --runs 5
"""

from __future__ import annotations

import argparse
import json
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
RUST_EXE = ROOT / "rust" / "target" / "release" / "img2webp.exe"

CONFIG = """[paths]
input_dir = {input_dir}
output_dir = outputs

[webp]
quality = 90

[animation]
missing_duration_ms = 80

[copy]
jpeg = {copy_jpeg}
avif = true
webp = true
other_files = true

[output]
overwrite_existing = true
reuse_unchanged = true
adopt_untracked = false
db_commit_every = 32
parallel_jobs = {jobs}

[preserve]
metadata = true
timestamps = true
"""


def run_once(project: Path, jobs: int, copy_jpeg: bool) -> float:
    outputs = project / "outputs"
    shutil.rmtree(outputs, ignore_errors=True)
    outputs.mkdir(parents=True)
    config = CONFIG.format(
        input_dir="inputs",
        copy_jpeg="true" if copy_jpeg else "false",
        jobs=jobs,
    )
    (project / "config.ini").write_text(config, encoding="utf-8")
    shutil.copy(RUST_EXE, project / "img2webp.exe")
    start = time.perf_counter()
    result = subprocess.run(
        [str(project / "img2webp.exe")],
        cwd=project,
        capture_output=True,
        text=True,
        timeout=600,
    )
    elapsed = time.perf_counter() - start
    if result.returncode != 0:
        raise SystemExit(
            f"binary failed ({result.returncode}): {result.stdout}\n{result.stderr}"
        )
    # The config uses the relative "inputs" folder (the binary resolves it next
    # to itself). Guard against a silently empty run.
    if "変換 0 / コピー 0 / 再利用 0 / 登録 0 / 失敗 0" in result.stdout:
        raise SystemExit(f"nothing was processed: {result.stdout}\n{result.stderr}")
    report_last = [line for line in result.stdout.splitlines() if line.startswith("完了:")]
    if not report_last:
        raise SystemExit(f"no completion line: {result.stdout}\n{result.stderr}")
    return elapsed


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--corpus", required=True)
    parser.add_argument("--jobs", type=int, nargs="+", required=True)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--label", default=None)
    parser.add_argument("--copy-jpeg", action="store_true")
    parser.add_argument("--json", default=None)
    arguments = parser.parse_args()
    corpus = Path(arguments.corpus).resolve()
    label = arguments.label or corpus.name
    if not corpus.is_dir():
        raise SystemExit(f"missing corpus: {corpus}")
    report = {"label": label, "corpus": str(corpus), "runs": arguments.runs, "jobs": {}}
    with tempfile.TemporaryDirectory(prefix="img2webp-bench-") as temporary:
        project = Path(temporary)
        inputs = project / "inputs"
        inputs.mkdir()
        for item in corpus.iterdir():
            if item.is_file():
                (inputs / item.name).write_bytes(item.read_bytes())
        for jobs in arguments.jobs:
            times = []
            for _ in range(arguments.runs):
                times.append(run_once(project, jobs, arguments.copy_jpeg))
            median = statistics.median(times)
            report["jobs"][str(jobs)] = {
                "times": [round(value, 4) for value in times],
                "median": round(median, 4),
            }
            print(f"{label} jobs={jobs:>2} median={median:.4f}s times={[round(t, 4) for t in times]}")
    sequential = report["jobs"][str(arguments.jobs[0])]["median"]
    fastest = min(entry["median"] for entry in report["jobs"].values())
    report["speedup"] = round(sequential / fastest, 3)
    print(f"{label} speedup(first->fastest) = {report['speedup']}x")
    if arguments.json:
        Path(arguments.json).write_text(json.dumps(report, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
