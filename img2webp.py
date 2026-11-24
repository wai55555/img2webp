"""Convert PNG, optional JPEG, and GIF images to WebP; copy selected files."""

from __future__ import annotations

import configparser
import io
import json
import os
import shutil
import sqlite3
import stat
import sys
import tempfile
import zlib
from collections import Counter
from contextlib import closing
from dataclasses import dataclass
from pathlib import Path

import piexif
import piexif.helper
from PIL import Image, ImageCms, features


PROJECT_DIR = Path(__file__).resolve().parent
STATE_DATABASE_NAME = ".img2webp.sqlite3"
PIPELINE_VERSION = 2
CONVERT_FORMATS = {".png": "PNG", ".jpg": "JPEG", ".jpeg": "JPEG", ".gif": "GIF"}
ORIENTATION_OPERATIONS = {
    2: Image.Transpose.FLIP_LEFT_RIGHT,
    3: Image.Transpose.ROTATE_180,
    4: Image.Transpose.FLIP_TOP_BOTTOM,
    5: Image.Transpose.TRANSPOSE,
    6: Image.Transpose.ROTATE_270,
    7: Image.Transpose.TRANSVERSE,
    8: Image.Transpose.ROTATE_90,
}
CONFIG_KEYS = {
    "paths": {"input_dir", "output_dir"},
    "webp": {"quality"},
    "animation": {"missing_duration_ms"},
    "copy": {"jpeg", "avif", "webp", "other_files"},
    "output": {"overwrite_existing", "reuse_unchanged", "adopt_untracked", "db_commit_every"},
    "preserve": {"metadata", "timestamps"},
}


class ConfigError(ValueError):
    pass


class ConversionError(ValueError):
    pass


@dataclass(frozen=True)
class Config:
    input_dir: Path
    output_dir: Path
    quality: int
    missing_duration_ms: int
    copy_jpeg: bool
    copy_avif: bool
    copy_webp: bool
    copy_other_files: bool
    overwrite_existing: bool
    reuse_unchanged: bool
    adopt_untracked: bool
    db_commit_every: int
    preserve_metadata: bool
    preserve_timestamps: bool


@dataclass(frozen=True)
class Job:
    source: Path
    relative_source: Path
    action: str
    destination: Path


@dataclass(frozen=True)
class JobResult:
    warnings: list[str]
    source_stat: os.stat_result
    output_stat: os.stat_result


def load_config(path: Path) -> Config:
    parser = configparser.ConfigParser(interpolation=None)
    try:
        if not parser.read(path, encoding="utf-8-sig"):
            raise ConfigError(f"設定ファイルがありません: {path}")
    except configparser.Error as exc:
        raise ConfigError(f"config.ini を読めません: {exc}") from exc

    unknown_sections = set(parser.sections()) - set(CONFIG_KEYS)
    if unknown_sections or parser.defaults():
        raise ConfigError(f"不明な設定セクション: {', '.join(sorted(unknown_sections)) or 'DEFAULT'}")
    for section, required_keys in CONFIG_KEYS.items():
        if not parser.has_section(section):
            raise ConfigError(f"設定セクションがありません: [{section}]")
        found_keys = set(parser.options(section))
        missing = required_keys - found_keys
        unknown = found_keys - required_keys
        if missing or unknown:
            detail = f"不足: {', '.join(sorted(missing))}" if missing else f"不明: {', '.join(sorted(unknown))}"
            raise ConfigError(f"[{section}] {detail}")

    def get_bool(section: str, key: str) -> bool:
        try:
            return parser.getboolean(section, key)
        except ValueError as exc:
            raise ConfigError(f"[{section}] {key} は true または false にしてください") from exc

    def get_int(section: str, key: str) -> int:
        try:
            return parser.getint(section, key)
        except ValueError as exc:
            raise ConfigError(f"[{section}] {key} は整数にしてください") from exc

    def get_path(key: str) -> Path:
        raw = parser.get("paths", key).strip()
        if not raw:
            raise ConfigError(f"[paths] {key} が空です")
        candidate = Path(raw)
        return (candidate if candidate.is_absolute() else PROJECT_DIR / candidate).resolve()

    input_dir = get_path("input_dir")
    output_dir = get_path("output_dir")
    if input_dir == output_dir or input_dir in output_dir.parents or output_dir in input_dir.parents:
        raise ConfigError("入力フォルダと出力フォルダは別々の階層にしてください")
    if input_dir.exists() and not input_dir.is_dir():
        raise ConfigError(f"入力フォルダにファイルが指定されています: {input_dir}")
    quality = get_int("webp", "quality")
    if not 0 <= quality <= 100:
        raise ConfigError("[webp] quality は 0～100 にしてください")
    duration = get_int("animation", "missing_duration_ms")
    if duration <= 0:
        raise ConfigError("[animation] missing_duration_ms は正の整数にしてください")
    commit_every = get_int("output", "db_commit_every")
    if commit_every <= 0:
        raise ConfigError("[output] db_commit_every は正の整数にしてください")

    return Config(
        input_dir=input_dir,
        output_dir=output_dir,
        quality=quality,
        missing_duration_ms=duration,
        copy_jpeg=get_bool("copy", "jpeg"),
        copy_avif=get_bool("copy", "avif"),
        copy_webp=get_bool("copy", "webp"),
        copy_other_files=get_bool("copy", "other_files"),
        overwrite_existing=get_bool("output", "overwrite_existing"),
        reuse_unchanged=get_bool("output", "reuse_unchanged"),
        adopt_untracked=get_bool("output", "adopt_untracked"),
        db_commit_every=commit_every,
        preserve_metadata=get_bool("preserve", "metadata"),
        preserve_timestamps=get_bool("preserve", "timestamps"),
    )


def classify(path: Path, config: Config) -> str:
    suffix = path.suffix.lower()
    if suffix in {".jpg", ".jpeg"}:
        return "copy" if config.copy_jpeg else "convert"
    if suffix in CONVERT_FORMATS:
        return "convert"
    if suffix == ".avif":
        return "copy" if config.copy_avif else "skip"
    if suffix == ".webp":
        return "copy" if config.copy_webp else "skip"
    return "copy" if config.copy_other_files else "skip"


def collision_key(path: Path) -> str:
    return path.as_posix().casefold()


def with_number(path: Path, number: int) -> Path:
    return path.with_name(f"{path.stem}-{number}{path.suffix}")


def plan_jobs(config: Config) -> tuple[list[Job], int]:
    sources = sorted(
        (path for path in config.input_dir.rglob("*") if path.is_file() and not path.is_symlink()),
        key=lambda path: (collision_key(path.relative_to(config.input_dir)), str(path)),
    )
    proposed: list[tuple[Path, Path, str, Path]] = []
    skipped = 0
    for source in sources:
        relative = source.relative_to(config.input_dir)
        action = classify(source, config)
        if action == "skip":
            skipped += 1
            continue
        destination = relative.with_suffix(".webp") if action == "convert" else relative
        proposed.append((source, relative, action, destination))

    counts = Counter(collision_key(item[3]) for item in proposed)
    used: set[str] = {
        collision_key(Path(STATE_DATABASE_NAME + suffix))
        for suffix in ("", "-wal", "-shm", "-journal")
    }
    jobs: list[Job] = []
    # Copy targets keep their original names; converted targets move when needed.
    ordered = sorted(proposed, key=lambda item: (item[2] == "convert", collision_key(item[3]), collision_key(item[1])))
    for source, relative, action, destination in ordered:
        if action == "convert" and counts[collision_key(destination)] > 1:
            destination = destination.with_name(f"{relative.stem}{relative.suffix.lower()}.webp")
        base = destination
        number = 2
        while collision_key(destination) in used:
            destination = with_number(base, number)
            number += 1
        used.add(collision_key(destination))
        jobs.append(Job(source, relative, action, config.output_dir / destination))
    return sorted(jobs, key=lambda job: collision_key(job.relative_source)), skipped


def settings_signature(job: Job, config: Config) -> str:
    settings: dict[str, object] = {
        "pipeline": PIPELINE_VERSION,
        "platform": os.name,
        "action": job.action,
        "timestamps": config.preserve_timestamps,
    }
    if job.action == "convert":
        settings.update(
            quality=config.quality,
            metadata=config.preserve_metadata,
        )
        if job.relative_source.suffix.lower() in {".png", ".gif"}:
            settings["missing_duration_ms"] = config.missing_duration_ms
    return json.dumps(settings, sort_keys=True, separators=(",", ":"))


def initialize_state(connection: sqlite3.Connection) -> None:
    connection.execute("PRAGMA journal_mode = WAL")
    connection.execute("PRAGMA synchronous = NORMAL")
    version = connection.execute("PRAGMA user_version").fetchone()[0]
    if version not in (0, 1):
        raise ConfigError(f"未対応の処理履歴DBバージョンです: {version}")
    connection.execute(
        """CREATE TABLE IF NOT EXISTS completed_jobs (
            source_path TEXT PRIMARY KEY,
            destination_path TEXT NOT NULL,
            source_size INTEGER NOT NULL,
            source_mtime_ns INTEGER NOT NULL,
            source_ctime_ns INTEGER NOT NULL,
            output_size INTEGER NOT NULL,
            output_mtime_ns INTEGER NOT NULL,
            settings TEXT NOT NULL
        )"""
    )
    connection.execute("PRAGMA user_version = 1")
    connection.commit()


def get_record(connection: sqlite3.Connection, job: Job) -> tuple[object, ...] | None:
    return connection.execute(
        """SELECT destination_path, source_size, source_mtime_ns, source_ctime_ns,
                  output_size, output_mtime_ns, settings
           FROM completed_jobs WHERE source_path = ?""",
        (job.relative_source.as_posix(),),
    ).fetchone()


def record_values(job: Job, config: Config, source_stat: os.stat_result, output_stat: os.stat_result) -> tuple[object, ...]:
    return (
        job.destination.relative_to(config.output_dir).as_posix(),
        source_stat.st_size, source_stat.st_mtime_ns, source_stat.st_ctime_ns,
        output_stat.st_size, output_stat.st_mtime_ns,
        settings_signature(job, config),
    )


def is_tracked_unchanged(connection: sqlite3.Connection, job: Job, config: Config) -> bool:
    record = get_record(connection, job)
    if record is None:
        return False
    try:
        output_stat = job.destination.stat()
    except FileNotFoundError:
        return False
    if not stat.S_ISREG(output_stat.st_mode):
        return False
    return record == record_values(job, config, job.source.stat(), output_stat)


def record_completed(
    connection: sqlite3.Connection, job: Job, config: Config,
    source_stat: os.stat_result, output_stat: os.stat_result,
) -> None:
    connection.execute(
        """INSERT INTO completed_jobs
           (source_path, destination_path, source_size, source_mtime_ns,
            source_ctime_ns, output_size, output_mtime_ns, settings)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?)
           ON CONFLICT(source_path) DO UPDATE SET
            destination_path = excluded.destination_path,
            source_size = excluded.source_size,
            source_mtime_ns = excluded.source_mtime_ns,
            source_ctime_ns = excluded.source_ctime_ns,
            output_size = excluded.output_size,
            output_mtime_ns = excluded.output_mtime_ns,
            settings = excluded.settings""",
        (job.relative_source.as_posix(), *record_values(job, config, source_stat, output_stat)),
    )


def text_value(value: object) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, bytes):
        try:
            return value.decode("utf-8")
        except UnicodeDecodeError:
            return value.decode("latin-1")
    return ""


MAX_STEALTH_BYTES = 8 * 1024 * 1024
MAX_PNG_BYTES = 512 * 1024 * 1024


def png_16bit_pixels(path: Path, header_only: bool = False) -> tuple[bytearray, int, int, int] | None:
    """Decode non-interlaced 16-bit PNG scanlines without discarding low bits."""
    with path.open("rb") as stream:
        header = stream.read(33)
        if (len(header) != 33 or header[:8] != b"\x89PNG\r\n\x1a\n"
                or header[8:16] != b"\x00\x00\x00\x0dIHDR"):
            return None
        width = int.from_bytes(header[16:20], "big")
        height = int.from_bytes(header[20:24], "big")
        depth, color, compression, filtering, interlace = header[24:29]
        channels = {2: 3, 4: 2, 6: 4}.get(color)
        if (depth != 16 or channels is None or compression or filtering or interlace
                or not 0 < width <= 16384 or not 0 < height <= 16384):
            return None
        stride = width * channels * 2
        expected = (stride + 1) * height
        if expected > MAX_PNG_BYTES:
            return None
        decoder = zlib.decompressobj()
        decoded = bytearray()
        idat_size = 0
        saw_idat = False
        ended_idat = False
        try:
            while True:
                chunk_header = stream.read(8)
                if len(chunk_header) != 8:
                    return None
                length = int.from_bytes(chunk_header[:4], "big")
                kind = chunk_header[4:]
                if kind == b"IDAT":
                    if ended_idat or idat_size + length > MAX_PNG_BYTES:
                        return None
                    saw_idat = True
                    idat_size += length
                    remaining = length
                    while remaining:
                        block = stream.read(min(remaining, 65536))
                        if not block:
                            return None
                        remaining -= len(block)
                        decoded.extend(decoder.decompress(block, expected + 1 - len(decoded)))
                        if len(decoded) > expected or decoder.unconsumed_tail:
                            return None
                else:
                    if saw_idat:
                        ended_idat = True
                    stream.seek(length, os.SEEK_CUR)
                if len(stream.read(4)) != 4:
                    return None
                if kind == b"IEND":
                    break
        except zlib.error:
            return None
        if not saw_idat or not decoder.eof or len(decoded) != expected:
            return None

    bpp = channels * 2
    # Only the first 152 column-major bits are needed to identify a header.
    # Filter dependencies require all preceding rows and columns in that area.
    rows = min(height, 152) if header_only else height
    columns = min(width, (152 + height - 1) // height) if header_only else width
    for y in range(rows):
        row = y * (stride + 1)
        method = decoded[row]
        if method > 4:
            return None
        if method == 0:
            continue
        for x in range(columns * bpp):
            i = row + 1 + x
            left = decoded[i - bpp] if x >= bpp else 0
            above = decoded[i - stride - 1] if y else 0
            upper_left = decoded[i - stride - 1 - bpp] if y and x >= bpp else 0
            if method == 1:
                predictor = left
            elif method == 2:
                predictor = above
            elif method == 3:
                predictor = (left + above) // 2
            else:
                p = left + above - upper_left
                distances = (abs(p - left), abs(p - above), abs(p - upper_left))
                predictor = (left, above, upper_left)[distances.index(min(distances))]
            decoded[i] = (decoded[i] + predictor) & 255
    return decoded, width, height, channels


def stealth_text(image: Image.Image, path: Path) -> str:
    """Read column-major Stealth PNG Info before lossy conversion."""
    if image.format != "PNG":
        return ""
    with path.open("rb") as stream:
        header = stream.read(29)
    if len(header) < 29:
        return ""
    depth_16 = header[24] == 16
    header_only_16 = depth_16
    if depth_16:
        raw = png_16bit_pixels(path, header_only=True)
        if raw is None:
            return ""
        decoded, width, height, channel_count = raw
        stride = width * channel_count * 2 + 1
        def sample(x: int, y: int, channel: int) -> int:
            return decoded[y * stride + 1 + x * channel_count * 2 + channel * 2 + 1]
        modes = (([channel_count - 1], b"stealth_png"),) if channel_count == 2 else (
            (([3], b"stealth_png"), ([0, 1, 2], b"stealth_rgb")) if channel_count == 4
            else (([0, 1, 2], b"stealth_rgb"),)
        )
    else:
        if image.mode not in {"RGB", "RGBA", "LA"}:
            return ""
        width, height = image.size
        if width > 16384 or height > 16384 or width * height * len(image.getbands()) > MAX_PNG_BYTES:
            return ""
        pixels = image.load()
        def sample(x: int, y: int, channel: int) -> int:
            return pixels[x, y][channel]
        modes = []
        if image.mode in {"RGBA", "LA"}:
            modes.append(([-1], b"stealth_png"))
        if image.mode in {"RGB", "RGBA"}:
            modes.append(([0, 1, 2], b"stealth_rgb"))
    for channels, prefix in modes:
        capacity = width * height * len(channels)
        def read_bytes(start: int, count: int) -> bytes:
            result = bytearray()
            for byte_index in range(start, start + count):
                value = 0
                for bit in range(8):
                    index = byte_index * 8 + bit
                    pixel, channel = divmod(index, len(channels))
                    value = (value << 1) | (sample(pixel // height, pixel % height, channels[channel]) & 1)
                result.append(value)
            return bytes(result)
        if capacity < 152:
            continue
        header = read_bytes(0, 19)
        if header[:15] not in (prefix + b"info", prefix + b"comp"):
            continue
        length = int.from_bytes(header[15:], "big")
        if length % 8 or length > MAX_STEALTH_BYTES * 8 or 152 + length > capacity:
            continue
        if header_only_16:
            raw = png_16bit_pixels(path)
            if raw is None:
                return ""
            decoded = raw[0]
            header_only_16 = False
        if length == 0:
            payload = b""
        else:
            # Pillow performs transpose, channel selection and bit packing in C.
            end_bit = 152 + length
            columns = min(width, (end_bit + height * len(channels) - 1) // (height * len(channels)))
            if depth_16:
                low_bytes = bytearray()
                for y in range(height):
                    start = y * stride + 2
                    low_bytes.extend(decoded[start:start + columns * channel_count * 2:2])
                mode = {2: "LA", 3: "RGB", 4: "RGBA"}[channel_count]
                source_pixels = Image.frombytes(mode, (columns, height), bytes(low_bytes))
            else:
                source_pixels = image.crop((0, 0, columns, height))
            transposed = source_pixels.transpose(Image.Transpose.TRANSPOSE)
            plane = transposed.getchannel("A") if prefix == b"stealth_png" else transposed.convert("RGB")
            bits = plane.tobytes()[152:end_bit]
            bit_image = Image.frombytes("L", (len(bits), 1), bits)
            payload = bit_image.point([255 if i & 1 else 0 for i in range(256)], mode="1").tobytes()
        try:
            if header[11:15] == b"comp":
                decoder = zlib.decompressobj(47)
                payload = decoder.decompress(payload, MAX_STEALTH_BYTES + 1)
                if len(payload) > MAX_STEALTH_BYTES or not decoder.eof:
                    continue
            return payload.decode("utf-8")
        except (UnicodeDecodeError, zlib.error):
            continue
    return ""


def source_text(image: Image.Image, initial_info: dict[str, object]) -> str:
    if image.format == "PNG":
        lines: list[str] = []
        parameters = initial_info.get("parameters")
        if isinstance(parameters, str) and parameters:
            lines.append(parameters)
        for key, value in initial_info.items():
            if key == "parameters" or key.lower() in {"xmp", "xml:com.adobe.xmp"}:
                continue
            if isinstance(value, str) and value:
                lines.append(f"{key}: {value}")
        return "\n".join(lines)
    if image.format in {"JPEG", "GIF"}:
        return text_value(initial_info.get("comment"))
    return ""


def empty_exif() -> dict[str, object]:
    return {"0th": {}, "Exif": {}, "GPS": {}, "Interop": {}, "1st": {}, "thumbnail": None}


def has_exif_data(exif_dict: dict[str, object]) -> bool:
    return any(exif_dict[ifd] for ifd in ("0th", "Exif", "GPS", "Interop", "1st")) or bool(exif_dict["thumbnail"])


def prepare_metadata(
    image: Image.Image, initial_info: dict[str, object], preserve: bool, source: Path
) -> tuple[dict[str, object] | None, bytes | None, bytes | None, int | None]:
    raw_exif = initial_info.get("exif")
    if raw_exif and not isinstance(raw_exif, bytes):
        raise ConversionError("Exif データの形式を読めません")
    try:
        exif_dict = piexif.load(raw_exif) if raw_exif else empty_exif()
    except (ValueError, TypeError, KeyError) as exc:
        if preserve:
            raise ConversionError(f"Exif を読み取れません: {exc}") from exc
        exif_dict = empty_exif()

    orientation = exif_dict["0th"].get(piexif.ImageIFD.Orientation)
    if orientation is None:
        orientation = image.getexif().get(piexif.ImageIFD.Orientation)
    if orientation in ORIENTATION_OPERATIONS:
        exif_dict["0th"].pop(piexif.ImageIFD.Orientation, None)

    if not preserve:
        return None, None, None, orientation

    candidate = source_text(image, initial_info)
    hidden = stealth_text(image, source)
    if hidden:
        candidate = "\n".join(filter(None, (candidate, hidden)))
    existing_comment = exif_dict["Exif"].get(piexif.ExifIFD.UserComment)
    if candidate:
        if existing_comment:
            if piexif.ImageIFD.XPComment in exif_dict["0th"]:
                raise ConversionError("UserComment と XPComment が使用済みで、追加テキストを保持できません")
            exif_dict["0th"][piexif.ImageIFD.XPComment] = candidate.encode("utf-16le") + b"\x00\x00"
        else:
            exif_dict["Exif"][piexif.ExifIFD.UserComment] = piexif.helper.UserComment.dump(candidate, encoding="unicode")

    icc = initial_info.get("icc_profile")
    if icc is not None and not isinstance(icc, bytes):
        raise ConversionError("ICC プロファイルの形式を読めません")
    xmp = initial_info.get("xmp") or initial_info.get("XML:com.adobe.xmp")
    if isinstance(xmp, str):
        xmp = xmp.encode("utf-8")
    if xmp is not None and not isinstance(xmp, bytes):
        raise ConversionError("XMP データの形式を読めません")
    return exif_dict, icc, xmp, orientation


def orient(frame: Image.Image, orientation: int | None) -> Image.Image:
    operation = ORIENTATION_OPERATIONS.get(orientation)
    return frame.transpose(operation) if operation is not None else frame


def normalize_color(frame: Image.Image, icc: bytes | None) -> tuple[Image.Image, bytes | None]:
    source_profile = None
    color_space = None
    if icc:
        try:
            source_profile = ImageCms.ImageCmsProfile(io.BytesIO(icc))
            color_space = source_profile.profile.xcolor_space.strip()
        except (OSError, ValueError, ImageCms.PyCMSError) as exc:
            raise ConversionError(f"ICC プロファイルを読めません: {exc}") from exc
    if frame.mode in {"CMYK", "L", "LA"} and source_profile:
        expected_space = "CMYK" if frame.mode == "CMYK" else "GRAY"
        if color_space != expected_space:
            raise ConversionError("画像の色空間と ICC プロファイルが一致しません")
        try:
            srgb = ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB"))
            color = frame.getchannel("L") if frame.mode == "LA" else frame
            converted = ImageCms.profileToProfile(color, source_profile, srgb, outputMode="RGB")
            if frame.mode == "LA":
                converted.putalpha(frame.getchannel("A"))
            return converted, srgb.tobytes()
        except (OSError, ValueError, ImageCms.PyCMSError) as exc:
            raise ConversionError(f"ICC を使った色変換に失敗しました: {exc}") from exc
    if frame.mode == "CMYK":
        return frame.convert("RGB"), None
    if frame.mode in {"RGB", "RGBA"}:
        if icc and color_space != "RGB":
            raise ConversionError("画像の色空間と ICC プロファイルが一致しません")
        return frame, icc
    has_alpha = "A" in frame.getbands() or "transparency" in frame.info
    converted = frame.convert("RGBA" if has_alpha else "RGB")
    if icc:
        if color_space != "RGB":
            raise ConversionError("変換後の RGB 画像に元の ICC プロファイルを適用できません")
    return converted, icc


def verify_metadata(path: Path, exif_dict: dict[str, object] | None, icc: bytes | None, xmp: bytes | None) -> list[str]:
    warnings: list[str] = []
    with Image.open(path) as result:
        if exif_dict is not None and has_exif_data(exif_dict):
            try:
                expected = piexif.load(piexif.dump(exif_dict))
                actual = piexif.load(result.info.get("exif", b""))
            except (ValueError, TypeError, KeyError) as exc:
                raise ConversionError(f"出力の Exif を読み戻せません: {exc}") from exc
            comment_tag = piexif.ExifIFD.UserComment
            xp_tag = piexif.ImageIFD.XPComment
            if actual["Exif"].get(comment_tag) != expected["Exif"].get(comment_tag):
                raise ConversionError("出力の UserComment が元データと一致しません")
            if actual["0th"].get(xp_tag) != expected["0th"].get(xp_tag):
                raise ConversionError("出力の XPComment が元データと一致しません")
            for ifd in ("0th", "Exif", "GPS", "Interop", "1st"):
                if actual[ifd] != expected[ifd]:
                    warnings.append(f"Exif {ifd} の一部が変わりました")
            if actual["thumbnail"] != expected["thumbnail"]:
                warnings.append("Exif サムネイルが変わりました")
        if icc and result.info.get("icc_profile") != icc:
            warnings.append("ICC プロファイルを完全には保持できませんでした")
        if xmp and result.info.get("xmp") != xmp:
            warnings.append("XMP を完全には保持できませんでした")
    return warnings


def convert_image(source: Path, destination: Path, config: Config) -> list[str]:
    warnings: list[str] = []
    with Image.open(source) as image:
        expected_format = CONVERT_FORMATS[source.suffix.lower()]
        if image.format != expected_format:
            raise ConversionError(f"拡張子は {expected_format} ですが、実際の形式は {image.format} です")
        initial_info = image.info.copy()
        exif_dict, icc, xmp, orientation = prepare_metadata(image, initial_info, config.preserve_metadata, source)
        color_icc = initial_info.get("icc_profile")
        if color_icc is not None and not isinstance(color_icc, bytes):
            raise ConversionError("ICC プロファイルの形式を読めません")
        is_animated = getattr(image, "n_frames", 1) > 1
        start_frame = 1 if image.format == "PNG" and initial_info.get("default_image") else 0
        frames: list[Image.Image] = []
        durations: list[int] = []
        output_icc = icc
        for index in range(start_frame, getattr(image, "n_frames", 1)):
            image.seek(index)
            duration = image.info.get("duration", config.missing_duration_ms)
            if duration is None:
                duration = config.missing_duration_ms
            if not isinstance(duration, (int, float)) or duration < 0:
                raise ConversionError(f"フレーム {index} の表示時間が不正です")
            frame = orient(image.copy(), orientation)
            frame, frame_icc = normalize_color(frame, color_icc)
            output_icc = frame_icc
            frames.append(frame)
            durations.append(round(duration))
        if not frames:
            raise ConversionError("保存できるフレームがありません")

        save_options: dict[str, object] = {"format": "WEBP", "quality": config.quality}
        if config.preserve_metadata:
            if exif_dict is not None and has_exif_data(exif_dict):
                try:
                    save_options["exif"] = piexif.dump(exif_dict)
                except (ValueError, TypeError, KeyError) as exc:
                    raise ConversionError(f"Exif を保存用に変換できません: {exc}") from exc
            if output_icc:
                save_options["icc_profile"] = output_icc
            if xmp:
                save_options["xmp"] = xmp
        if is_animated and len(frames) > 1:
            loop = initial_info.get("loop")
            if loop is None:
                loop = 1 if image.format == "GIF" else 0
            if not isinstance(loop, int) or loop < 0:
                raise ConversionError("アニメーションのループ回数が不正です")
            save_options.update(save_all=True, append_images=frames[1:], duration=durations, loop=loop)
        frames[0].save(destination, **save_options)
        if config.preserve_metadata:
            warnings.extend(verify_metadata(destination, exif_dict, output_icc, xmp))
    return warnings


def set_windows_creation_time(path: Path, source_stat: os.stat_result) -> None:
    if os.name != "nt":
        return
    import pywintypes
    import win32con
    import win32file

    creation = getattr(source_stat, "st_birthtime", source_stat.st_ctime)
    handle = win32file.CreateFile(
        str(path), win32con.GENERIC_WRITE,
        win32con.FILE_SHARE_READ | win32con.FILE_SHARE_WRITE | win32con.FILE_SHARE_DELETE,
        None, win32con.OPEN_EXISTING, 0, None,
    )
    try:
        win32file.SetFileTime(handle, pywintypes.Time(creation), None, None)
    finally:
        handle.Close()


def run_job(job: Job, config: Config) -> JobResult | None:
    destination = job.destination
    if destination.exists() and not config.overwrite_existing:
        return None
    source_stat = job.source.stat()
    destination.parent.mkdir(parents=True, exist_ok=True)
    descriptor, temporary_name = tempfile.mkstemp(prefix=".img2webp-", suffix=destination.suffix, dir=destination.parent)
    os.close(descriptor)
    temporary = Path(temporary_name)
    try:
        if job.action == "copy":
            shutil.copyfile(job.source, temporary)
            warnings: list[str] = []
        else:
            warnings = convert_image(job.source, temporary, config)
        if config.preserve_timestamps:
            os.utime(temporary, ns=(source_stat.st_atime_ns, source_stat.st_mtime_ns))
            set_windows_creation_time(temporary, source_stat)
        source_after = job.source.stat()
        if (
            source_stat.st_size, source_stat.st_mtime_ns, source_stat.st_ctime_ns
        ) != (
            source_after.st_size, source_after.st_mtime_ns, source_after.st_ctime_ns
        ):
            raise ConversionError("処理中に入力ファイルが変更されました")
        os.replace(temporary, destination)
        return JobResult(warnings, source_stat, destination.stat())
    finally:
        temporary.unlink(missing_ok=True)


def main() -> int:
    connection: sqlite3.Connection | None = None
    try:
        config = load_config(PROJECT_DIR / "config.ini")
        if not features.check("webp"):
            raise ConfigError("Pillow に WebP 保存機能がありません")
        config.input_dir.mkdir(parents=True, exist_ok=True)
        config.output_dir.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(config.output_dir / STATE_DATABASE_NAME)
        initialize_state(connection)
        jobs, skipped = plan_jobs(config)
    except (ConfigError, OSError, UnicodeError, sqlite3.DatabaseError) as exc:
        if connection is not None:
            connection.close()
        print(f"設定エラー: {exc}", file=sys.stderr)
        return 2
    except KeyboardInterrupt:
        if connection is not None:
            connection.close()
        print("中断しました。", file=sys.stderr)
        return 130

    counts = Counter(skipped=skipped, converted=0, copied=0, adopted=0, failed=0)
    pending_writes = 0

    def save_record(job: Job, source_stat: os.stat_result, output_stat: os.stat_result) -> None:
        nonlocal pending_writes
        record_completed(connection, job, config, source_stat, output_stat)
        pending_writes += 1
        if pending_writes >= config.db_commit_every:
            connection.commit()
            pending_writes = 0

    try:
        with closing(connection):
            try:
                for job in jobs:
                    try:
                        if config.reuse_unchanged and is_tracked_unchanged(connection, job, config):
                            counts["skipped"] += 1
                            print(f"SKIP {job.relative_source}: 前回成功時の記録と一致")
                            continue
                        if (
                            config.reuse_unchanged and config.adopt_untracked
                            and job.destination.is_file() and get_record(connection, job) is None
                        ):
                            save_record(job, job.source.stat(), job.destination.stat())
                            counts["adopted"] += 1
                            print(f"ADOPT {job.relative_source}: 既存の出力を現在の設定として登録")
                            continue
                        result = run_job(job, config)
                        if result is None:
                            counts["skipped"] += 1
                            print(f"SKIP {job.relative_source}: 出力があり、上書きは無効です")
                            continue
                        save_record(job, result.source_stat, result.output_stat)
                        counts["converted" if job.action == "convert" else "copied"] += 1
                        print(f"{'CONVERT' if job.action == 'convert' else 'COPY'} {job.relative_source} -> {job.destination.relative_to(config.output_dir)}")
                        for warning in result.warnings:
                            print(f"WARN {job.relative_source}: {warning}", file=sys.stderr)
                    except sqlite3.DatabaseError:
                        raise
                    except Exception as exc:
                        counts["failed"] += 1
                        print(f"ERROR {job.relative_source}: {type(exc).__name__}: {exc}", file=sys.stderr)
            finally:
                connection.commit()
    except KeyboardInterrupt:
        print("中断しました。記録済みのファイルは次回再利用されます。", file=sys.stderr)
        return 130
    except sqlite3.DatabaseError as exc:
        print(f"処理履歴DBエラー: {exc}", file=sys.stderr)
        return 2
    print(f"完了: 変換 {counts['converted']} / コピー {counts['copied']} / 再利用 {counts['skipped']} / 登録 {counts['adopted']} / 失敗 {counts['failed']}")
    return 1 if counts["failed"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
