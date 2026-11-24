from __future__ import annotations

import gzip
import struct
import tempfile
import unittest
import zlib
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

import piexif
import piexif.helper
from PIL import Image, PngImagePlugin

import img2webp as app


def chunk(kind: bytes, data: bytes) -> bytes:
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def make_png(path: Path, depth: int, color: int, mode: str, payload: bytes,
             compression: str = "info", filter_type: int = 0, length: int | None = None,
             size: tuple[int, int] = (40, 30)) -> None:
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
        b"info" if compression == "info" else b"comp")
    encoded = signature + struct.pack(">I", len(data) * 8 if length is None else length) + data
    channel_list = [channels - 1] if mode == "alpha" else [0, 1, 2]
    for index in range(len(encoded) * 8):
        pixel, channel = divmod(index, len(channel_list))
        x, y = divmod(pixel, height)
        offset = x * bpp + channel_list[channel] * sample_bytes + sample_bytes - 1
        rows[y][offset] = (rows[y][offset] & 254) | ((encoded[index // 8] >> (7 - index % 8)) & 1)
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
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr)
                     + chunk(b"IDAT", compressed[:13]) + chunk(b"IDAT", compressed[13:])
                     + chunk(b"IEND", b""))


class StealthTests(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        self.inputs, self.outputs = root / "inputs", root / "outputs"
        self.inputs.mkdir()
        self.config = app.Config(self.inputs, self.outputs, 80, 100, False, True, True,
                                 False, True, True, False, 32, True, True)

    def run_all(self, config: app.Config | None = None) -> None:
        active = config or self.config
        for job in app.plan_jobs(active)[0]:
            app.run_job(job, active)

    def test_stealth_variants_are_saved_in_webp(self) -> None:
        for depth, color, mode in ((8, 2, "rgb"), (8, 6, "rgb"), (8, 6, "alpha"), (8, 4, "alpha"),
                                   (16, 2, "rgb"), (16, 6, "rgb"), (16, 6, "alpha"),
                                   (16, 4, "alpha")):
            for compression in ("info", "gzip", "zlib"):
                for filter_type in range(5):
                    name = f"{depth}-{color}-{mode}-{compression}-{filter_type}"
                    text = f"生成情報 {name}"
                    make_png(self.inputs / f"{name}.png", depth, color, mode,
                             text.encode(), compression, filter_type)
        self.run_all()
        for source in self.inputs.glob("*.png"):
            with self.subTest(source=source.name), Image.open(self.outputs / (source.stem + ".webp")) as image:
                comment = piexif.load(image.info["exif"])["Exif"][piexif.ExifIFD.UserComment]
                self.assertEqual(piexif.helper.UserComment.load(comment), f"生成情報 {source.stem}")

    def test_invalid_payload_and_disabled_metadata(self) -> None:
        make_png(self.inputs / "invalid.png", 16, 6, "alpha", b"abc", length=7)
        make_png(self.inputs / "disabled.png", 8, 2, "rgb", b"secret")
        self.run_all(replace(self.config, preserve_metadata=False))
        with Image.open(self.outputs / "disabled.webp") as image:
            self.assertNotIn("exif", image.info)
        with Image.open(self.inputs / "invalid.png") as image:
            self.assertEqual(app.stealth_text(image, self.inputs / "invalid.png"), "")

    def test_stealth_coexists_with_existing_metadata(self) -> None:
        source = self.inputs / "combined.png"
        make_png(source, 8, 6, "alpha", "hidden 文".encode())
        with Image.open(source) as original:
            pixels = original.copy()
        info = PngImagePlugin.PngInfo()
        info.add_text("parameters", "visible prompt")
        existing = piexif.helper.UserComment.dump("existing", encoding="unicode")
        pixels.save(source, pnginfo=info, exif=piexif.dump({"Exif": {piexif.ExifIFD.UserComment: existing}}))
        self.run_all()
        with Image.open(self.outputs / "combined.webp") as image:
            exif = piexif.load(image.info["exif"])
        self.assertEqual(exif["Exif"][piexif.ExifIFD.UserComment], existing)
        self.assertEqual(bytes(exif["0th"][piexif.ImageIFD.XPComment]),
                         "visible prompt\nhidden 文".encode("utf-16le") + b"\0\0")

    def test_invalid_compressed_and_utf8_payloads_are_ignored(self) -> None:
        for name, payload, compression in (("bad-utf8", b"\xff", "info"),
                                            ("bad-compressed", b"\xff", "gzip")):
            path = self.inputs / f"{name}.png"
            make_png(path, 8, 6, "alpha", payload, compression)
            if name == "bad-compressed":
                # Replace compressed data with malformed data while retaining its signature.
                make_png(path, 8, 6, "alpha", b"\xff", "info")
                with Image.open(path) as image:
                    pixels = image.copy()
                # Change the "info" signature to "comp" in the alpha bitstream.
                for bit, value in enumerate(b"comp"):
                    for offset in range(8):
                        index = (11 + bit) * 8 + offset
                        x, y = divmod(index, pixels.height)
                        rgba = list(pixels.getpixel((x, y)))
                        rgba[3] = (rgba[3] & 254) | ((value >> (7 - offset)) & 1)
                        pixels.putpixel((x, y), tuple(rgba))
                pixels.save(path)
            with Image.open(path) as image:
                self.assertEqual(app.stealth_text(image, path), "")

    def test_payload_limits_and_truncated_stream(self) -> None:
        path = self.inputs / "limited.png"
        make_png(path, 16, 6, "alpha", b"a" * 100, "gzip")
        with patch.object(app, "MAX_STEALTH_BYTES", 64), Image.open(path) as image:
            self.assertEqual(app.stealth_text(image, path), "")
        make_png(path, 16, 6, "alpha", b"data", length=40000)
        with Image.open(path) as image:
            self.assertEqual(app.stealth_text(image, path), "")
        contents = path.read_bytes()
        path.write_bytes(contents[:-8])
        self.assertIsNone(app.png_16bit_pixels(path))
        with Image.open(path) as image:
            self.assertEqual(app.stealth_text(image, path), "")

    def test_large_payload_and_header_only_scan(self) -> None:
        for depth, color, mode in ((8, 2, "rgb"), (8, 6, "alpha"),
                                   (16, 2, "rgb"), (16, 6, "alpha")):
            path = self.inputs / f"{depth}-{color}-{mode}.png"
            payload = b"payload" * 3000
            make_png(path, depth, color, mode, payload, filter_type=4, size=(512, 512))
            with self.subTest(depth=depth, mode=mode), Image.open(path) as image:
                self.assertEqual(app.stealth_text(image, path), payload.decode())
        path = self.inputs / "plain.png"
        make_png(path, 16, 6, "alpha", b"data", filter_type=4, size=(200, 200))
        # Remove both possible signatures without changing the PNG structure.
        with patch.object(app, "png_16bit_pixels", wraps=app.png_16bit_pixels) as decoder:
            original = app.png_16bit_pixels(path, header_only=True)
            self.assertIsNotNone(original)
            decoded, width, height, channels = original
            decoded[2] |= 1
            decoded[8] |= 1
            decoder.return_value = (decoded, width, height, channels)
            decoder.reset_mock()
            with Image.open(path) as image:
                self.assertEqual(app.stealth_text(image, path), "")
            decoder.assert_called_once_with(path, header_only=True)


if __name__ == "__main__":
    unittest.main()
