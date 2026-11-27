"""Baseline / comparison measurement (design spec §4.2, §4.3).

Builds a corpus, then runs the Python implementation and the Rust release
binary on the same machine and records wall time, peak working set and output
bytes. Run with a Windows Python so both implementations use the same OS:

    C:\\Users\\<user>\\AppData\\Local\\Programs\\Python\\Python310\\python.exe \
        rust\\tests\\benchmark.py

The results are appended to `doc/img2webp-rust-phase0.md`.
"""

from __future__ import annotations

import ctypes
import ctypes.wintypes as wt
import gzip
import json
import shutil
import statistics
import struct
import subprocess
import sys
import time
import zlib
from pathlib import Path

from PIL import Image, PngImagePlugin, features

ROOT = Path(__file__).resolve().parents[2]
BENCH = ROOT / ".bench"
CORPUS = BENCH / "corpus"
PY_IMPL = ROOT / "img2webp.py"
CONFIG = ROOT / "config.ini"
RUST_EXE = ROOT / "rust" / "target" / "release" / "img2webp.exe"

PROCESS_MEMORY_COUNTERS = ctypes.Structure


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
        handle, ctypes.byref(counters), ctypes.sizeof(counters)
    )
    kernel32.CloseHandle(handle)
    return counters.PeakWorkingSetSize if ok else 0


def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data))
    )


def stealth_png(path: Path, depth: int, color: int, payload: bytes, size=(128, 128),
                compression: str = "info", filter_type: int = 0) -> None:
    channels = {2: 3, 4: 2, 6: 4}[color]
    width, height = size
    sample_bytes = depth // 8
    bpp = channels * sample_bytes
    rows = [bytearray([128] * (width * bpp)) for _ in range(height)]
    if compression == "gzip":
        data = gzip.compress(payload)
    elif compression == "zlib":
        data = zlib.compress(payload)
    else:
        data = payload
    signature = (b"stealth_png" if color in (4, 6) else b"stealth_rgb") + (
        b"info" if compression == "info" else b"comp"
    )
    encoded = signature + struct.pack(">I", len(data) * 8) + data
    channel_list = [channels - 1] if color in (4, 6) else [0, 1, 2]
    for index in range(len(encoded) * 8):
        pixel, channel = divmod(index, len(channel_list))
        x, y = divmod(pixel, height)
        offset = x * bpp + channel_list[channel] * sample_bytes + sample_bytes - 1
        rows[y][offset] = (rows[y][offset] & 254) | (
            (encoded[index // 8] >> (7 - index % 8)) & 1
        )
    encoded_rows = bytearray()
    for y, row in enumerate(rows):
        encoded_rows.append(filter_type)
        for x, value in enumerate(row):
            encoded_rows.append(value)
    ihdr = struct.pack(">IIBBBBB", width, height, depth, color, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(encoded_rows)))
        + chunk(b"IEND", b"")
    )


def raw16_png(path: Path, size=(256, 256)) -> None:
    width, height = size
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        for x in range(width):
            rows.extend(struct.pack(">HHH", (x * 251) % 65536, (y * 257) % 65536, 30000))
    ihdr = struct.pack(">IIBBBBB", width, height, 16, 2, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(rows)))
        + chunk(b"IEND", b"")
    )


def build_corpus() -> None:
    shutil.rmtree(CORPUS, ignore_errors=True)
    inputs = CORPUS / "inputs"
    inputs.mkdir(parents=True)
    info = PngImagePlugin.PngInfo()
    info.add_text("parameters", "a photo of a cat\nNegative prompt: blurry\nSteps: 28, Sampler: Euler")
    for index in range(40):
        Image.new(
            "RGBA",
            (256, 256),
            ((index * 6) % 256, (index * 11) % 256, (index * 17) % 256, 255),
        ).save(inputs / f"ai-{index:02d}.png", pnginfo=info)
    for index in range(10):
        stealth_png(inputs / f"stealth-8-{index:02d}.png", 8, 6, f"hidden {index}".encode())
    for index in range(10):
        stealth_png(inputs / f"stealth-16-{index:02d}.png", 16, 6, f"hidden {index}".encode())
    for index in range(10):
        raw16_png(inputs / f"plain16-{index:02d}.png")
    for index in range(10):
        Image.new(
            "RGB", (512, 512), ((index * 20) % 256, (index * 7) % 256, 128)
        ).save(inputs / f"photo-{index:02d}.jpg", quality=90)
    frames = [
        Image.new("RGBA", (128, 128), ((i * 30) % 256, 20, 200, 255))
        for i in range(10)
    ]
    for index in range(5):
        frames[0].save(
            inputs / f"anim-{index}.gif",
            save_all=True,
            append_images=frames[1:],
            duration=50,
            loop=0,
        )
    for index in range(5):
        frames[0].save(
            inputs / f"anim-{index}.png",
            save_all=True,
            append_images=frames[1:],
            duration=50,
            loop=0,
        )
    Image.new("RGB", (3840, 2160), (12, 34, 56)).save(inputs / "large.png")
    for index in range(10):
        (inputs / f"copy-{index:02d}.webp").write_bytes(b"RIFFxxxxWEBP" + bytes(2048))
    for index in range(10):
        (inputs / f"copy-{index:02d}.webm").write_bytes(bytes(8192))
    print(f"corpus: {len(list(inputs.iterdir()))} files")


def write_config(project: Path) -> None:
    text = CONFIG.read_text(encoding="utf-8")
    text = text.replace("copy.jpeg = true", "copy.jpeg = false")
    text = text.replace("copy.other_files = true", "copy.other_files = true")
    (project / "config.ini").write_text(text, encoding="utf-8")


def measure_python(project: Path) -> tuple[float, int]:
    shutil.copy(PY_IMPL, project / "img2webp.py")
    return run_measured([sys.executable, "img2webp.py"], project)


def measure_rust(project: Path) -> tuple[float, int]:
    shutil.copy(RUST_EXE, project / "img2webp.exe")
    return run_measured([str(project / "img2webp.exe")], project)


def run_measured(command: list[str], project: Path) -> tuple[float, int]:
    start = time.perf_counter()
    process = subprocess.Popen(
        command,
        cwd=project,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    peak = 0
    while process.poll() is None:
        peak = max(peak, peak_working_set(process.pid))
        time.sleep(0.01)
    elapsed = time.perf_counter() - start
    peak = max(peak, peak_working_set(process.pid))
    return elapsed, peak


CATEGORIES = {
    "1-png-ai": lambda name: name.startswith("ai-"),
    "2-stealth": lambda name: name.startswith("stealth-"),
    "3-plain16": lambda name: name.startswith("plain16-"),
    "4-jpeg": lambda name: name.startswith("photo-"),
    "5-gif": lambda name: name.startswith("anim-") and name.endswith(".gif"),
    "6-apng": lambda name: name.startswith("anim-") and name.endswith(".png"),
    "7-large": lambda name: name == "large.png",
    "8-copy": lambda name: name.startswith("copy-"),
}


def prepare(project: Path, selector=None) -> None:
    shutil.rmtree(project, ignore_errors=True)
    (project / "inputs").mkdir(parents=True)
    for path in CORPUS.joinpath("inputs").iterdir():
        if selector is not None and not selector(path.name):
            continue
        shutil.copy(path, project / "inputs" / path.name)
    write_config(project)


SWEEP = [
    ("default", {}),
    ("method0", {"method": "0"}),
    ("method2", {"method": "2"}),
    ("method6", {"method": "6"}),
    ("quality80", {"quality": "80"}),
    ("quality100", {"quality": "100"}),
    ("lossless", {"lossless": "true"}),
    ("exact", {"exact": "true"}),
    ("parallel1", {"parallel_jobs": "1"}),
    ("parallel2", {"parallel_jobs": "2"}),
    ("parallel4", {"parallel_jobs": "4"}),
]


def sweep() -> int:
    if not RUST_EXE.exists():
        print("build the release binary first", file=sys.stderr)
        return 2
    selector = lambda name: name.startswith("ai-") or name == "large.png"
    print(f"{'variant':<12} {'median_s':>9} {'peak_MiB':>9} {'output_bytes':>13}")
    rows = []
    for label, patches in SWEEP:
        times = []
        peaks = []
        total = 0
        for _ in range(2):
            project = BENCH / f"sweep-{label}"
            prepare(project, selector)
            text = (project / "config.ini").read_text(encoding="utf-8")
            for key, value in patches.items():
                if f"{key} =" in text:
                    text = "\n".join(
                        f"{key} = {value}" if line.strip().startswith(f"{key} =") else line
                        for line in text.splitlines()
                    )
                elif key.startswith("parallel"):
                    text = text.replace("[preserve]", f"{key} = {value}\n[preserve]")
                else:
                    text = text.replace("[animation]", f"{key} = {value}\n[animation]")
            (project / "config.ini").write_text(text, encoding="utf-8")
            shutil.copy(RUST_EXE, project / "img2webp.exe")
            elapsed, peak = run_measured([str(project / "img2webp.exe")], project)
            times.append(elapsed)
            peaks.append(peak)
            total = sum(
                path.stat().st_size
                for path in (project / "outputs").rglob("*")
                if path.is_file() and not path.name.startswith(".img2webp")
            )
        median = statistics.median(times)
        rows.append((label, median, max(peaks), total))
        print(f"{label:<12} {median:>9.3f} {max(peaks) / 1024 / 1024:>9.1f} {total:>13}")
    (BENCH / "sweep.json").write_text(
        json.dumps(
            [
                {"variant": label, "median_s": time, "peak": peak, "output_bytes": total}
                for label, time, peak, total in rows
            ],
            indent=2,
        ),
        encoding="utf-8",
    )
    return 0


def main() -> int:
    if "--sweep" in sys.argv:
        return sweep()
    if not RUST_EXE.exists():
        print("build rust/target/release/img2webp.exe first", file=sys.stderr)
        return 2
    if not CORPUS.exists():
        build_corpus()
    results = {}
    for label, measure in (("python", measure_python), ("rust", measure_rust)):
        times = []
        peaks = []
        output_bytes = 0
        file_count = 0
        for attempt in range(3):
            project = BENCH / label
            prepare(project)
            elapsed, peak = measure(project)
            outputs = project / "outputs"
            total = 0
            count = 0
            for path in outputs.rglob("*"):
                if path.is_file() and ".sqlite3" not in path.name and not path.name.startswith(
                    ".img2webp"
                ):
                    total += path.stat().st_size
                    count += 1
            times.append(elapsed)
            peaks.append(peak)
            output_bytes = total
            file_count = count
            print(
                f"{label} run {attempt + 1}: {elapsed:.2f}s peak={peak / 1024 / 1024:.1f}MiB "
                f"outputs={count} bytes={total}"
            )
        results[label] = {
            "times": times,
            "median_time": statistics.median(times),
            "peak_bytes": max(peaks),
            "output_bytes": output_bytes,
            "outputs": file_count,
        }
    versions = {
        "python": sys.version.split()[0],
        "pillow": Image.__version__,
        "pillow_libwebp": features.version("webp"),
        "rust_binary_bytes": RUST_EXE.stat().st_size,
    }
    categories = {}
    for name, selector in CATEGORIES.items():
        for label, measure in (("python", measure_python), ("rust", measure_rust)):
            times = []
            peaks = []
            for _ in range(3):
                project = BENCH / f"{label}-{name}"
                prepare(project, selector)
                elapsed, peak = measure(project)
                times.append(elapsed)
                peaks.append(peak)
            categories.setdefault(name, {})[label] = {
                "median_time": statistics.median(times),
                "peak_bytes": max(peaks),
            }
        py = categories[name]["python"]
        rs = categories[name]["rust"]
        print(
            f"{name}: python {py['median_time']:.3f}s / rust {rs['median_time']:.3f}s "
            f"({py['median_time'] / max(rs['median_time'], 1e-9):.2f}x)"
        )

    payload = {
        "results": results,
        "categories": categories,
        "versions": versions,
    }
    print(json.dumps(payload, indent=2))
    (BENCH / "results.json").write_text(json.dumps(payload, indent=2), encoding="utf-8")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
