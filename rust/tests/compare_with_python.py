"""Compares the Rust port against the Python implementation.

For every fixture both implementations convert the same input with the same
`config.ini`, then the outputs are compared: exit code, progress output,
destination names, animation metadata, Exif/ICC/XMP and pixels.

Usage (from the repository root):

    .venv/bin/python rust/tests/compare_with_python.py [fixture ...]

Without arguments a default fixture set is used. Exit code is 1 when any
comparison is outside the acceptance criteria (design spec §3.1).
"""

from __future__ import annotations

import argparse
import io
import json
import shutil
import subprocess
import sys
from pathlib import Path

import piexif
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
FIXTURES = ROOT / "rust" / "tests" / "fixtures"
CONFIG = ROOT / "config.ini"
RUST_EXE = ROOT / "rust" / "target" / "release" / "img2webp.exe"

DEFAULT_FIXTURES = [
    "text.png",
    "plain-parameters.png",
    "conflict.png",
    "tail-text.png",
    "anim.gif",
    "once.gif",
    "optimized.gif",
    "background-disposal.gif",
    "apng-standard.png",
    "apng-separate.png",
    "rotated.jpg",
    "cmyk.jpg",
    "plain.jpg",
    "gray.jpg",
    "plain16-gray0.png",
    "plain16-gray2.png",
    "plain16-gray4.png",
    "plain16-gray6.png",
    "stealth-8-6-alpha-zlib-4.png",
    "stealth-8-2-rgb-info-0.png",
    "stealth-16-6-alpha-gzip-2.png",
    "stealth-16-4-alpha-info-4.png",
    "stealth-16-2-rgb-info-0.png",
    "stealth-bad-utf8.png",
    "apng-16bit.png",
    "apng-default-only.png",
    "mpo.jpg",
    "precision12.jpg",
    "crc-mismatch.png",
    "broken.png",
]


def run_python(project: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [sys.executable, "img2webp.py"],
        cwd=project,
        capture_output=True,
        text=True,
        timeout=300,
    )


def run_rust(project: Path) -> subprocess.CompletedProcess:
    return subprocess.run(
        [str(project / "img2webp.exe")],
        cwd=project,
        capture_output=True,
        text=True,
        timeout=300,
    )


def normalize_output(text: str, project: Path) -> list[str]:
    lines = []
    for line in text.splitlines():
        line = line.replace(str(project), "<root>")
        line = line.replace("\\", "/")
        lines.append(line)
    return lines


def image_frames(path: Path):
    with Image.open(path) as image:
        frames = []
        for index in range(getattr(image, "n_frames", 1)):
            image.seek(index)
            info = dict(image.info)
            frame = image.convert("RGBA")
            frames.append((frame.copy(), info))
        return frames


def compare_pixels(expected: Image.Image, actual: Image.Image) -> tuple[int, int]:
    if expected.size != actual.size:
        return (max(expected.size), 255)
    expected_bytes = expected.tobytes()
    actual_bytes = actual.tobytes()
    diff = 0
    worst = 0
    for channel_a, channel_b in zip(expected_bytes, actual_bytes):
        delta = abs(channel_a - channel_b)
        if delta:
            diff += 1
            worst = max(worst, delta)
    return (diff, worst)


def describe_exif(info: dict) -> dict:
    raw = info.get("exif")
    if not raw:
        return {}
    try:
        loaded = piexif.load(raw)
    except Exception as error:  # pragma: no cover - defensive
        return {"error": str(error)}
    out = {}
    user = loaded["Exif"].get(piexif.ExifIFD.UserComment)
    if user is not None:
        try:
            out["UserComment"] = piexif.helper.UserComment.load(user)
        except Exception:
            out["UserComment"] = repr(user)[:80]
    xp = loaded["0th"].get(piexif.ImageIFD.XPComment)
    if xp is not None:
        out["XPComment"] = bytes(xp).decode("utf-16le", "strict").rstrip("\x00")
    out["ifd0_tags"] = sorted(loaded["0th"].keys())
    out["exif_tags"] = sorted(loaded["Exif"].keys())
    out["orientation"] = loaded["0th"].get(piexif.ImageIFD.Orientation)
    return out


# Settings the Python implementation does not know about. They are dropped from
# the Python side (its config parser rejects unknown keys) and the variants that
# need them are compared Rust-only.
RUST_ONLY_KEYS = {
    "lossless": "[webp]",
    "method": "[webp]",
    "exact": "[webp]",
    "parallel_jobs": "[output]",
    "stealth": "[preserve]",
    "stealth_max_pixels": "[preserve]",
}
RUST_ONLY_VARIANTS = {"lossless", "exact", "method0"}


def patched_config(patches: dict[str, str], for_python: bool = False) -> bytes:
    """Builds a config.ini for one side.

    Patches are applied to the existing line when there is one and inserted in
    the right section otherwise, so a variant really changes the setting (the
    first version of this script silently ignored settings that the template did
    not list). The Python side drops the Rust-only keys.
    """
    lines = CONFIG.read_text(encoding="utf-8").splitlines()
    if for_python:
        lines = [
            line
            for line in lines
            if not any(
                line.strip().startswith(f"{key} =") or line.strip().startswith(f"{key}=")
                for key in RUST_ONLY_KEYS
            )
        ]
    for key, value in patches.items():
        if for_python and key in RUST_ONLY_KEYS:
            continue
        replaced = False
        for index, line in enumerate(lines):
            stripped = line.strip()
            if stripped.startswith(f"{key} =") or stripped.startswith(f"{key}="):
                lines[index] = f"{key} = {value}"
                replaced = True
                break
        if replaced:
            continue
        section = RUST_ONLY_KEYS.get(key)
        if section is None:
            raise SystemExit(f"cannot place config key {key}")
        lines.insert(lines.index(section) + 1, f"{key} = {value}")
    return ("\n".join(lines) + "\n").encode("utf-8")


# Differences that the acceptance criteria allow, with the reason. Anything not
# listed here fails the run; `--strict` also fails the listed ones.
EXPECTED_DIFFERENCES = {
    ("apng-16bit.png", ""): (
        "16bit APNG is unsupported by design (Rust fails the job, Python converts)"
    ),
    ("cmyk-icc.jpg", "jpeg-convert"): (
        "ICC profile bytes differ in the sRGB creation timestamp (4 bytes); pixels match"
    ),
}


def tag_of(label: str) -> str:
    if " [" not in label:
        return ""
    return label.rsplit(" [", 1)[1].rstrip("]")


def compare_fixture(name: str, root: Path, patches: dict[str, str] = None, tag: str = "") -> dict:
    patches = patches or {}
    label = f"{name}{(' [' + tag + ']') if tag else ''}"
    source = FIXTURES / name
    py_project = root / "py" / (Path(name).stem + tag.replace(" ", "-"))
    rs_project = root / "rs" / (Path(name).stem + tag.replace(" ", "-"))
    for project in (py_project, rs_project):
        (project / "inputs").mkdir(parents=True, exist_ok=True)
        shutil.copy(source, project / "inputs" / name)
        (project / "config.ini").write_bytes(
            patched_config(patches, for_python=project is py_project)
        )
    shutil.copy(ROOT / "img2webp.py", py_project / "img2webp.py")
    shutil.copy(RUST_EXE, rs_project / "img2webp.exe")

    py_result = run_python(py_project)
    rs_result = run_rust(rs_project)
    report = {
        "fixture": label,
        "exit": [py_result.returncode, rs_result.returncode],
        "stdout_match": normalize_output(py_result.stdout, py_project)
        == normalize_output(rs_result.stdout, rs_project),
        "py_stdout": py_result.stdout.strip().splitlines()[-2:],
        "rs_stdout": rs_result.stdout.strip().splitlines()[-2:],
        "py_stderr": py_result.stderr.strip().splitlines()[:3],
        "rs_stderr": rs_result.stderr.strip().splitlines()[:3],
        "outputs": [],
    }
    py_outputs = sorted(
        (py_project / "outputs").rglob("*")
        if (py_project / "outputs").exists()
        else []
    )
    rs_outputs = sorted(
        (rs_project / "outputs").rglob("*")
        if (rs_project / "outputs").exists()
        else []
    )
    def visible(paths, project):
        names = []
        for path in paths:
            if not path.is_file():
                continue
            relative = path.relative_to(project / "outputs").as_posix()
            if ".sqlite3" in relative or relative.startswith(".img2webp"):
                continue
            names.append(relative)
        return sorted(names)

    py_names = visible(py_outputs, py_project)
    rs_names = visible(rs_outputs, rs_project)
    report["names"] = [py_names, rs_names]
    for relative in py_names:
        py_path = py_project / "outputs" / relative
        rs_path = rs_project / "outputs" / relative
        entry = {"file": relative}
        if not rs_path.exists():
            entry["missing"] = True
            report["outputs"].append(entry)
            continue
        py_bytes = py_path.read_bytes()
        rs_bytes = rs_path.read_bytes()
        entry["same_bytes"] = py_bytes == rs_bytes
        if py_path.suffix.lower() != ".webp":
            entry["bytes_equal"] = py_bytes == rs_bytes
            report["outputs"].append(entry)
            continue
        py_frames = image_frames(py_path)
        rs_frames = image_frames(rs_path)
        entry["frames"] = [len(py_frames), len(rs_frames)]
        entry["loop"] = [py_frames[0][1].get("loop"), rs_frames[0][1].get("loop")]
        entry["durations"] = [
            [info.get("duration") for _, info in py_frames],
            [info.get("duration") for _, info in rs_frames],
        ]
        entry["exif"] = [describe_exif(py_frames[0][1]), describe_exif(rs_frames[0][1])]
        entry["icc_equal"] = py_frames[0][1].get("icc_profile") == rs_frames[0][1].get("icc_profile")
        entry["icc_length"] = [
            len(py_frames[0][1].get("icc_profile") or b""),
            len(rs_frames[0][1].get("icc_profile") or b""),
        ]
        entry["xmp_equal"] = py_frames[0][1].get("xmp") == rs_frames[0][1].get("xmp")
        pixel_results = []
        for (py_frame, _), (rs_frame, _) in zip(py_frames, rs_frames):
            pixel_results.append(compare_pixels(py_frame, rs_frame))
        entry["pixels"] = pixel_results
        report["outputs"].append(entry)
    return report


def main() -> int:
    variants = [
        ("", {}),
        ("jpeg-convert", {"jpeg": "false"}),
        ("lossless", {"lossless": "true"}),
        ("metadata-off", {"metadata": "false"}),
        ("exact", {"exact": "true"}),
        ("method0", {"method": "0"}),
    ]
    variant_fixtures = {
        "": DEFAULT_FIXTURES,
        "jpeg-convert": ["rotated.jpg", "cmyk.jpg", "cmyk-icc.jpg", "plain.jpg", "gray.jpg"],
        "lossless": ["text.png", "sample.png", "plain16-gray2.png", "anim.gif"],
        "metadata-off": ["text.png", "anim.gif"],
        "exact": ["text.png"],
        "method0": ["text.png"],
    }
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--strict",
        action="store_true",
        help="also fail on the differences listed in EXPECTED_DIFFERENCES",
    )
    parser.add_argument("fixtures", nargs="*", help="fixture names (default: all)")
    arguments = parser.parse_args()
    selected = arguments.fixtures
    if not RUST_EXE.exists():
        print(f"missing release binary: {RUST_EXE}", file=sys.stderr)
        print("build with: cargo build --release --no-default-features", file=sys.stderr)
        return 2
    reports = []
    skipped: list[str] = []
    temporary = ROOT / ".compare-tmp"
    shutil.rmtree(temporary, ignore_errors=True)
    temporary.mkdir(parents=True)
    try:
        root = temporary
        for tag, patches in variants:
            fixtures = selected or variant_fixtures[tag]
            for name in fixtures:
                if not (FIXTURES / name).exists():
                    print(f"missing fixture {name}", file=sys.stderr)
                    continue
                if tag in RUST_ONLY_VARIANTS:
                    # The Python implementation cannot express these settings, so
                    # only the Rust side can run them (covered by tests/e2e.rs).
                    skipped.append(f"{name} [{tag}]")
                    continue
                reports.append(compare_fixture(name, root, patches, tag))
    finally:
        shutil.rmtree(temporary, ignore_errors=True)
    failures = 0
    expected_differences = 0
    for report in reports:
        problems = []
        if report["exit"][0] != report["exit"][1]:
            problems.append(f"exit {report['exit']}")
        if report["names"][0] != report["names"][1]:
            problems.append(f"names {report['names']}")
        for entry in report["outputs"]:
            if entry.get("missing"):
                problems.append(f"{entry['file']}: missing in Rust output")
                continue
            if "bytes_equal" in entry and not entry["bytes_equal"]:
                problems.append(f"{entry['file']}: copy bytes differ")
            if entry.get("frames") and entry["frames"][0] != entry["frames"][1]:
                problems.append(f"{entry['file']}: frames {entry['frames']}")
            if entry.get("loop") and entry["loop"][0] != entry["loop"][1]:
                problems.append(f"{entry['file']}: loop {entry['loop']}")
            if entry.get("durations") and entry["durations"][0] != entry["durations"][1]:
                problems.append(f"{entry['file']}: durations {entry['durations']}")
            if entry.get("exif") and entry["exif"][0] != entry["exif"][1]:
                problems.append(f"{entry['file']}: exif {json.dumps(entry['exif'], ensure_ascii=False)}")
            if entry.get("icc_equal") is False:
                problems.append(
                    f"{entry['file']}: icc differs (lengths {entry['icc_length']})"
                )
            if entry.get("xmp_equal") is False:
                problems.append(f"{entry['file']}: xmp differs")
            for index, (diff, worst) in enumerate(entry.get("pixels") or []):
                if diff:
                    problems.append(
                        f"{entry['file']}: frame {index} pixel diff count={diff} max={worst}"
                    )
        expected_reason = EXPECTED_DIFFERENCES.get(
            (report["fixture"].split(" [")[0], tag_of(report["fixture"]))
        )
        if not problems:
            status = "OK"
        elif expected_reason and not arguments.strict:
            status = "EXPECTED"
            expected_differences += 1
        else:
            status = "DIFF"
            failures += 1
        print(f"[{status}] {report['fixture']}")
        print(f"    py: exit={report['exit'][0]} {report['py_stdout']}")
        print(f"    rs: exit={report['exit'][1]} {report['rs_stdout']}")
        if report["py_stderr"]:
            print(f"    py stderr: {report['py_stderr']}")
        if report["rs_stderr"]:
            print(f"    rs stderr: {report['rs_stderr']}")
        identical = sum(1 for entry in report["outputs"] if entry.get("same_bytes"))
        compared = sum(1 for entry in report["outputs"] if "same_bytes" in entry)
        if compared:
            print(f"    byte-identical outputs: {identical}/{compared}")
        for problem in problems:
            print(f"    ! {problem}")
        if status == "EXPECTED":
            print(f"    ~ expected: {expected_reason}")
    for label in skipped:
        print(f"[RUST-ONLY] {label} (Python 版に該当設定が無いため比較対象外)")
    equivalent = len(reports) - failures - expected_differences
    print(
        f"\n{equivalent}/{len(reports)} fixtures equivalent"
        f" (+{expected_differences} expected differences,"
        f" {len(skipped)} rust-only cases excluded)"
    )
    return 0 if failures == 0 else 1


if __name__ == "__main__":
    raise SystemExit(main())
