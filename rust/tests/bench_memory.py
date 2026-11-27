"""Peak-memory measurement for animation inputs (design spec §4.1 H4).

Builds a long APNG, runs the release binary on it and records wall time and the
peak working set of the child process (the same yardstick `benchmark.py` uses).
Run with a Windows Python so the Windows binary is measured from the same OS:

    C:\\Users\\<user>\\AppData\\Local\\Programs\\Python\\Python310\\python.exe \\
        rust\\tests\\bench_memory.py --frames 60 --side 512 --runs 3

The result is printed as JSON so it can be pasted into the evidence report.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.wintypes as wt
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


class ProcessMemoryCounters(ctypes.Structure):
    _fields_ = [
        ("cb", wt.DWORD),
        ("PageFaultCount", wt.DWORD),
        ("PeakWorkingSetSize", ctypes.c_size_t),
        ("WorkingSetSize", ctypes.c_size_t),
        ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPagedPoolUsage", ctypes.c_size_t),
        ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
        ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
        ("PagefileUsage", ctypes.c_size_t),
        ("PeakPagefileUsage", ctypes.c_size_t),
    ]


PROCESS_QUERY_INFORMATION = 0x0400
PROCESS_VM_READ = 0x0010


def peak_working_set(pid: int) -> int:
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    psapi = ctypes.WinDLL("psapi", use_last_error=True)
    handle = kernel32.OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, False, pid)
    if not handle:
        return 0
    counters = ProcessMemoryCounters()
    counters.cb = ctypes.sizeof(ProcessMemoryCounters)
    ok = psapi.GetProcessMemoryInfo(
        handle, ctypes.byref(counters), ctypes.sizeof(ProcessMemoryCounters)
    )
    kernel32.CloseHandle(handle)
    return counters.PeakWorkingSetSize if ok else 0


CONFIG = """[paths]
input_dir = inputs
output_dir = outputs

[webp]
quality = 90

[animation]
missing_duration_ms = 80

[copy]
jpeg = false
avif = true
webp = true
other_files = true

[output]
overwrite_existing = true
reuse_unchanged = true
adopt_untracked = false
db_commit_every = 32

[preserve]
metadata = true
timestamps = true
"""


def build_apng(path: Path, frames: int, side: int) -> None:
    from PIL import Image

    images = []
    for index in range(frames):
        image = Image.new("RGBA", (side, side), ((index * 4) % 256, 40, 200, 255))
        images.append(image)
    images[0].save(
        path,
        save_all=True,
        append_images=images[1:],
        duration=40,
        loop=0,
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--frames", type=int, default=60)
    parser.add_argument("--side", type=int, default=512)
    parser.add_argument("--runs", type=int, default=3)
    arguments = parser.parse_args()
    if not RUST_EXE.exists():
        raise SystemExit(f"missing release binary: {RUST_EXE}")
    with tempfile.TemporaryDirectory(prefix="img2webp-mem-") as temporary:
        project = Path(temporary)
        inputs = project / "inputs"
        inputs.mkdir()
        build_apng(inputs / "long.png", arguments.frames, arguments.side)
        (project / "config.ini").write_text(CONFIG, encoding="utf-8")
        shutil.copy(RUST_EXE, project / "img2webp.exe")
        times = []
        peaks = []
        stdout = ""
        for _ in range(arguments.runs):
            shutil.rmtree(project / "outputs", ignore_errors=True)
            start = time.perf_counter()
            process = subprocess.Popen(
                [str(project / "img2webp.exe")],
                cwd=project,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                encoding="utf-8",
                errors="replace",
            )
            peak = 0
            while process.poll() is None:
                peak = max(peak, peak_working_set(process.pid))
                time.sleep(0.005)
            stdout, stderr = process.communicate()
            elapsed = time.perf_counter() - start
            if process.returncode != 0:
                raise SystemExit(f"binary failed: {stdout}\n{stderr}")
            times.append(elapsed)
            peaks.append(peak)
        output = {
            "frames": arguments.frames,
            "side": arguments.side,
            "input_bytes": (inputs / "long.png").stat().st_size,
            "output_bytes": (project / "outputs" / "long.webp").stat().st_size,
            "median_seconds": round(statistics.median(times), 4),
            "peak_working_set_bytes": max(peaks),
            "peak_working_set_mib": round(max(peaks) / (1024 * 1024), 1),
            "completion_line": stdout.strip().splitlines()[-1],
        }
        print(json.dumps(output, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    sys.exit(main())
