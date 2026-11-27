"""Generates the binary fixtures used by the Rust test-suite.

Run with the project virtualenv from the repository root:

    .venv/bin/python rust/tests/fixtures/generate_fixtures.py

The fixtures double as the Phase 0 corpus for behaviour comparisons against the
Python implementation (design spec §4.2 / §6 Phase 0).
"""

from __future__ import annotations

import gzip
import struct
import zlib
from pathlib import Path

import piexif
import piexif.helper
from PIL import Image, ImageCms, PngImagePlugin

HERE = Path(__file__).resolve().parent


def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data))
    )


def make_png(
    path: Path,
    depth: int,
    color: int,
    mode: str,
    payload: bytes,
    compression: str = "info",
    filter_type: int = 0,
    length: int | None = None,
    size: tuple[int, int] = (40, 30),
) -> None:
    """Creates a raw PNG with a Stealth PNG Info payload in the low bits."""
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
    signature = (b"stealth_png" if mode == "alpha" else b"stealth_rgb") + (
        b"info" if compression == "info" else b"comp"
    )
    encoded = signature + struct.pack(
        ">I", len(data) * 8 if length is None else length
    ) + data
    channel_list = [channels - 1] if mode == "alpha" else [0, 1, 2]
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
            left = row[x - bpp] if x >= bpp else 0
            above = rows[y - 1][x] if y else 0
            upper_left = rows[y - 1][x - bpp] if y and x >= bpp else 0
            if filter_type == 0:
                predictor = 0
            elif filter_type == 1:
                predictor = left
            elif filter_type == 2:
                predictor = above
            elif filter_type == 3:
                predictor = (left + above) // 2
            else:
                p = left + above - upper_left
                predictor = min((left, above, upper_left), key=lambda v: abs(p - v))
            encoded_rows.append((value - predictor) & 255)
    ihdr = struct.pack(">IIBBBBB", width, height, depth, color, 0, 0, 0)
    compressed = zlib.compress(bytes(encoded_rows))
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", compressed[:13])
        + chunk(b"IDAT", compressed[13:])
        + chunk(b"IEND", b"")
    )


def plain_png(path: Path, depth: int, color: int, size=(7, 5)) -> None:
    """Creates a 16-bit PNG without any stealth payload."""
    import random

    random.seed(depth + color)
    channels = {0: 1, 2: 3, 4: 2, 6: 4}[color]
    width, height = size
    rows = bytearray()
    for _ in range(height):
        rows.append(0)
        for _ in range(width * channels):
            rows.extend(struct.pack(">H", random.randrange(0, 65536)))
    ihdr = struct.pack(">IIBBBBB", width, height, depth, color, 0, 0, 0)
    path.write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(rows)))
        + chunk(b"IEND", b"")
    )


def write_text_png() -> None:
    info = PngImagePlugin.PngInfo()
    info.add_text("parameters", "a cat\nSteps: 20")
    info.add_text("Description", "NovelAI text")
    info.add_itxt("XML:com.adobe.xmp", '<x:xmpmeta xmlns:x="adobe:ns:meta/">m</x:xmpmeta>')
    exif = piexif.dump({"0th": {piexif.ImageIFD.Orientation: 1}})
    profile = ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()
    Image.new("RGBA", (4, 3), (100, 30, 20, 0)).save(
        HERE / "text.png", pnginfo=info, exif=exif, icc_profile=profile
    )


def write_tail_text_png() -> None:
    """A PNG whose text chunks appear after IDAT."""
    base = HERE / "text.png"
    data = base.read_bytes()
    pos = 8
    out = bytearray(data[:8])
    idat_end = None
    while pos < len(data):
        length = struct.unpack(">I", data[pos : pos + 4])[0]
        kind = data[pos + 4 : pos + 8]
        total = 12 + length
        if kind == b"IDAT":
            idat_end = len(out) + total
        out.extend(data[pos : pos + total])
        pos += total
    # Insert a tEXt chunk after the last IDAT.
    extra = chunk(b"tEXt", b"TailKey\0tail value")
    assert idat_end is not None
    out = out[:idat_end] + extra + out[idat_end:]
    (HERE / "tail-text.png").write_bytes(bytes(out))


def write_gifs() -> None:
    first = Image.new("RGBA", (6, 6), "red")
    second = Image.new("RGBA", (6, 6), "blue")
    first.save(
        HERE / "anim.gif",
        save_all=True,
        append_images=[second],
        duration=[70, 130],
        loop=2,
        comment=b"gif prompt",
    )
    first.save(HERE / "once.gif", save_all=True, append_images=[second], duration=[70, 130])

    frames = []
    for colour, box in (
        ("red", (0, 0, 8, 8)),
        ("blue", (2, 2, 4, 4)),
        ("green", (0, 0, 8, 8)),
    ):
        image = Image.new("RGB", (8, 8), "white")
        image.paste(Image.new("RGB", (box[2], box[3]), colour), (box[0], box[1]))
        frames.append(image.convert("P"))
    frames[0].save(
        HERE / "optimized.gif",
        save_all=True,
        append_images=frames[1:],
        duration=[100, 200, 300],
        loop=0,
        disposal=[2, 3, 1],
        optimize=True,
    )

    # Background-colour disposal: the canvas keeps an opaque background colour
    # while a transparent frame is disposed to background (BUG-01 candidate).
    canvas = Image.new("P", (8, 8), 0)
    canvas.putpalette([255, 0, 0, 0, 0, 255, 0, 255, 0] + [0] * 759)
    frame_a = canvas.copy()
    for x in range(8):
        frame_a.putpixel((x, 0), 1)
    frame_b = Image.new("P", (8, 8), 0)
    frame_b.putpalette(canvas.getpalette())
    frame_b.paste(2, (3, 3, 6, 6))
    frame_a.save(
        HERE / "background-disposal.gif",
        save_all=True,
        append_images=[frame_b],
        duration=[100, 100],
        loop=0,
        disposal=[2, 1],
        transparency=0,
    )


def write_jpegs() -> None:
    exif = piexif.dump({"0th": {piexif.ImageIFD.Orientation: 6}})
    Image.new("RGB", (2, 3), "green").save(HERE / "rotated.jpg", exif=exif)
    Image.new("CMYK", (4, 4), (0, 200, 0, 0)).save(HERE / "cmyk.jpg")
    Image.new("RGB", (4, 4), "red").save(HERE / "plain.jpg")
    Image.new("L", (4, 4), 128).save(HERE / "gray.jpg")
    try:
        first = Image.new("RGB", (4, 4), "red")
        second = Image.new("RGB", (4, 4), "blue")
        first.save(
            HERE / "mpo.jpg",
            format="MPO",
            save_all=True,
            append_images=[second],
        )
    except Exception as exc:  # pragma: no cover - Pillow build dependent
        print(f"MPO fixture skipped: {exc}")

    # JPEG whose SOF precision is not 8: patch the SOS/SOF precision byte.
    data = bytearray((HERE / "plain.jpg").read_bytes())
    pos = 2
    while pos + 4 < len(data):
        if data[pos] != 0xFF:
            break
        marker = data[pos + 1]
        length = struct.unpack(">H", data[pos + 2 : pos + 4])[0]
        if marker in (0xC0, 0xC1, 0xC2):
            data[pos + 4] = 12
            break
        pos += 2 + length
    (HERE / "precision12.jpg").write_bytes(bytes(data))


def write_apng() -> None:
    frames = [Image.new("RGBA", (6, 6), c) for c in ("white", "red", "blue")]
    frames[0].save(
        HERE / "apng-standard.png",
        save_all=True,
        append_images=[frames[1], frames[2]],
        duration=[70, 130, 50],
        loop=2,
    )
    frames[0].save(
        HERE / "apng-separate.png",
        save_all=True,
        default_image=True,
        append_images=[frames[1], frames[2]],
        duration=[70, 130],
        loop=2,
    )
    # acTL declares an animation but no fcTL/fdAT chunks exist: the default
    # image is the only content, so the frame loop ends up empty.
    width = height = 6
    rows = bytearray()
    for _ in range(height):
        rows.append(0)
        for _ in range(width):
            rows.extend(b"\xff\xff\xff\xff")
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    (HERE / "apng-default-only.png").write_bytes(
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"acTL", struct.pack(">II", 1, 0))
        + chunk(b"IDAT", zlib.compress(bytes(rows)))
        + chunk(b"IEND", b"")
    )


def write_16bit_apng() -> None:
    """A 16-bit APNG (unsupported by the Rust version by design)."""
    width = height = 4
    rows = bytearray()
    for y in range(height):
        rows.append(0)
        for x in range(width):
            rows.extend(struct.pack(">HHHH", x * 4000, y * 4000, 0x8000, 0xFFFF))
    ihdr = struct.pack(">IIBBBBB", width, height, 16, 6, 0, 0, 0)
    actl = struct.pack(">II", 1, 0)
    fctl = struct.pack(">IIIIIHHBB", 0, width, height, 0, 0, 1, 10, 0, 0)
    data = (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"acTL", actl)
        + chunk(b"fcTL", fctl)
        + chunk(b"IDAT", zlib.compress(bytes(rows)))
        + chunk(b"IEND", b"")
    )
    (HERE / "apng-16bit.png").write_bytes(data)


def write_stealth() -> None:
    for depth, color, mode in (
        (8, 2, "rgb"),
        (8, 6, "rgb"),
        (8, 6, "alpha"),
        (8, 4, "alpha"),
        (16, 2, "rgb"),
        (16, 6, "rgb"),
        (16, 6, "alpha"),
        (16, 4, "alpha"),
    ):
        for compression in ("info", "gzip", "zlib"):
            for filter_type in range(5):
                name = f"stealth-{depth}-{color}-{mode}-{compression}-{filter_type}.png"
                text = f"生成情報 {depth}-{color}-{mode}-{compression}-{filter_type}"
                make_png(
                    HERE / name,
                    depth,
                    color,
                    mode,
                    text.encode(),
                    compression,
                    filter_type,
                )
    make_png(HERE / "stealth-bad-length.png", 16, 6, "alpha", b"abc", length=7)
    make_png(HERE / "stealth-bad-utf8.png", 8, 6, "alpha", b"\xff")
    for depth, color in ((16, 0), (16, 2), (16, 4), (16, 6)):
        plain_png(HERE / f"plain16-gray{color}.png", depth, color)


def write_misc() -> None:
    (HERE / "broken.png").write_bytes(b"not a PNG at all")
    Image.new("RGB", (2, 2), "red").save(HERE / "sample.png")
    Image.new("RGB", (3, 3), "blue").save(HERE / "sample.jpg")
    (HERE / "sample.webp").write_bytes(b"copied webp")
    (HERE / "sample.png.webp").write_bytes(b"copied name collision")
    info = PngImagePlugin.PngInfo()
    info.add_text("parameters", "prompt only")
    Image.new("RGB", (3, 3)).save(HERE / "plain-parameters.png", pnginfo=info)
    conflict = piexif.helper.UserComment.dump("original comment", encoding="unicode")
    exif = piexif.dump({"Exif": {piexif.ExifIFD.UserComment: conflict}})
    info = PngImagePlugin.PngInfo()
    info.add_text("parameters", "new prompt")
    Image.new("RGB", (3, 3), "blue").save(
        HERE / "conflict.png", pnginfo=info, exif=exif
    )


def main() -> None:
    HERE.mkdir(parents=True, exist_ok=True)
    write_text_png()
    write_tail_text_png()
    write_gifs()
    write_jpegs()
    write_apng()
    write_16bit_apng()
    write_stealth()
    write_misc()
    print(f"fixtures written to {HERE}")


if __name__ == "__main__":
    main()
