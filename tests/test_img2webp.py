from __future__ import annotations

import configparser
import os
import re
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from dataclasses import replace
from io import StringIO
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import Mock, patch

import piexif
import piexif.helper
from PIL import Image, ImageCms, PngImagePlugin

import img2webp as app


class Img2WebpTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.inputs = self.root / "inputs"
        self.outputs = self.root / "outputs"
        self.inputs.mkdir()
        self.config = app.Config(
            input_dir=self.inputs,
            output_dir=self.outputs,
            quality=80,
            missing_duration_ms=100,
            copy_jpeg=False,
            copy_avif=True,
            copy_webp=True,
            copy_other_files=False,
            overwrite_existing=True,
            reuse_unchanged=True,
            adopt_untracked=False,
            db_commit_every=32,
            preserve_metadata=True,
            preserve_timestamps=True,
        )

    def run_all(self, config: app.Config | None = None) -> tuple[list[app.Job], int]:
        active = config or self.config
        jobs, skipped = app.plan_jobs(active)
        for job in jobs:
            app.run_job(job, active)
        return jobs, skipped

    def run_cli(self, config: app.Config | None = None) -> tuple[int, str, str]:
        active = config or self.config
        parser = configparser.ConfigParser()
        parser["paths"] = {"input_dir": str(active.input_dir), "output_dir": str(active.output_dir)}
        parser["webp"] = {"quality": str(active.quality)}
        parser["animation"] = {"missing_duration_ms": str(active.missing_duration_ms)}
        parser["copy"] = {
            "jpeg": str(active.copy_jpeg),
            "avif": str(active.copy_avif), "webp": str(active.copy_webp),
            "other_files": str(active.copy_other_files),
        }
        parser["output"] = {
            "overwrite_existing": str(active.overwrite_existing),
            "reuse_unchanged": str(active.reuse_unchanged),
            "adopt_untracked": str(active.adopt_untracked),
            "db_commit_every": str(active.db_commit_every),
        }
        parser["preserve"] = {
            "metadata": str(active.preserve_metadata),
            "timestamps": str(active.preserve_timestamps),
        }
        with (self.root / "config.ini").open("w", encoding="utf-8") as stream:
            parser.write(stream)
        stdout, stderr = StringIO(), StringIO()
        with patch.object(app, "PROJECT_DIR", self.root), redirect_stdout(stdout), redirect_stderr(stderr):
            code = app.main()
        return code, stdout.getvalue(), stderr.getvalue()

    def test_png_metadata_and_timestamps(self) -> None:
        source = self.inputs / "sample.png"
        png_info = PngImagePlugin.PngInfo()
        png_info.add_text("parameters", "a cat\nSteps: 20")
        png_info.add_text("Description", "NovelAI text")
        Image.new("RGBA", (4, 3), (100, 30, 20, 0)).save(source, pnginfo=png_info)
        original_time = 1_700_000_000_000_000_000
        os.utime(source, ns=(original_time, original_time))

        self.run_all()
        output = self.outputs / "sample.webp"
        with Image.open(output) as image:
            comment = piexif.load(image.info["exif"])["Exif"][piexif.ExifIFD.UserComment]
            self.assertEqual(piexif.helper.UserComment.load(comment), "a cat\nSteps: 20\nDescription: NovelAI text")
            self.assertEqual(image.getchannel("A").getextrema(), (0, 0))
        self.assertEqual(output.stat().st_mtime_ns, original_time)

    def test_metadata_conflict_keeps_original_comment(self) -> None:
        source = self.inputs / "conflict.png"
        png_info = PngImagePlugin.PngInfo()
        png_info.add_text("parameters", "new prompt")
        existing = piexif.helper.UserComment.dump("original comment", encoding="unicode")
        exif = piexif.dump({"Exif": {piexif.ExifIFD.UserComment: existing}})
        Image.new("RGB", (3, 3), "blue").save(source, pnginfo=png_info, exif=exif)

        self.run_all()
        with Image.open(self.outputs / "conflict.webp") as image:
            result = piexif.load(image.info["exif"])
        self.assertEqual(result["Exif"][piexif.ExifIFD.UserComment], existing)
        self.assertEqual(bytes(result["0th"][piexif.ImageIFD.XPComment]), "new prompt".encode("utf-16le") + b"\x00\x00")

    def test_icc_and_xmp_are_carried_over(self) -> None:
        source = self.inputs / "profile.png"
        profile = ImageCms.ImageCmsProfile(ImageCms.createProfile("sRGB")).tobytes()
        xmp = '<x:xmpmeta xmlns:x="adobe:ns:meta/">metadata</x:xmpmeta>'
        png_info = PngImagePlugin.PngInfo()
        png_info.add_itxt("XML:com.adobe.xmp", xmp)
        Image.new("RGB", (3, 3), "green").save(source, pnginfo=png_info, icc_profile=profile)

        self.run_all()
        with Image.open(self.outputs / "profile.webp") as image:
            self.assertEqual(image.info["icc_profile"], profile)
            self.assertEqual(image.info["xmp"], xmp.encode("utf-8"))

    def test_metadata_can_be_disabled_for_conversion(self) -> None:
        source = self.inputs / "plain.png"
        png_info = PngImagePlugin.PngInfo()
        png_info.add_text("parameters", "prompt")
        Image.new("RGB", (3, 3)).save(source, pnginfo=png_info)

        self.run_all(replace(self.config, preserve_metadata=False))
        with Image.open(self.outputs / "plain.webp") as image:
            self.assertNotIn("exif", image.info)

    def test_gif_animation_and_missing_loop(self) -> None:
        first = Image.new("RGBA", (6, 6), "red")
        second = Image.new("RGBA", (6, 6), "blue")
        first.save(self.inputs / "loop.gif", save_all=True, append_images=[second], duration=[70, 130], loop=2, comment=b"gif prompt")
        first.save(self.inputs / "once.gif", save_all=True, append_images=[second], duration=[70, 130])

        self.run_all()
        with Image.open(self.outputs / "loop.webp") as image:
            self.assertEqual(image.n_frames, 2)
            self.assertEqual(image.info["loop"], 2)
            comment = piexif.load(image.info["exif"])["Exif"][piexif.ExifIFD.UserComment]
            self.assertEqual(piexif.helper.UserComment.load(comment), "gif prompt")
            image.seek(0)
            image.load()
            first_duration = image.info["duration"]
            image.seek(1)
            image.load()
            second_duration = image.info["duration"]
            self.assertEqual((first_duration, second_duration), (70, 130))
        with Image.open(self.outputs / "once.webp") as image:
            self.assertEqual(image.info["loop"], 1)

    def test_apng_skips_separate_default_image(self) -> None:
        default = Image.new("RGBA", (6, 6), "white")
        red = Image.new("RGBA", (6, 6), "red")
        blue = Image.new("RGBA", (6, 6), "blue")
        default.save(
            self.inputs / "animated.png", save_all=True, default_image=True,
            append_images=[red, blue], duration=[70, 130], loop=2,
        )

        self.run_all()
        with Image.open(self.outputs / "animated.webp") as image:
            self.assertEqual(image.n_frames, 2)
            self.assertEqual(image.info["loop"], 2)
            image.seek(0)
            self.assertGreater(image.getpixel((0, 0))[0], image.getpixel((0, 0))[2])

    def test_copy_settings_and_timestamps(self) -> None:
        files = {"original.avif": b"AVIF bytes", "original.webp": b"WebP bytes", "movie.webm": b"WebM bytes"}
        original_time = 1_700_000_000_000_000_000
        for name, content in files.items():
            source = self.inputs / name
            source.write_bytes(content)
            os.utime(source, ns=(original_time, original_time))
        config = replace(self.config, copy_other_files=True)

        jobs, skipped = self.run_all(config)
        self.assertEqual((len(jobs), skipped), (3, 0))
        for name, content in files.items():
            output = self.outputs / name
            self.assertEqual(output.read_bytes(), content)
            self.assertEqual(output.stat().st_mtime_ns, original_time)

        self.assertIsNone(app.run_job(jobs[0], replace(config, overwrite_existing=False)))

    def test_copy_without_timestamps_uses_new_time(self) -> None:
        source = self.inputs / "movie.webm"
        source.write_bytes(b"movie")
        os.utime(source, ns=(1_500_000_000_000_000_000, 1_500_000_000_000_000_000))
        self.run_all(replace(self.config, copy_other_files=True, preserve_timestamps=False))
        self.assertGreater((self.outputs / source.name).stat().st_mtime_ns, 1_500_000_000_000_000_000)

    def test_jpeg_copy_option_preserves_bytes_and_reuses_on_quality_change(self) -> None:
        original_time = 1_700_000_000_000_000_000
        sources = []
        for suffix in (".jpg", ".jpeg"):
            source = self.inputs / f"photo-{suffix[1:]}{suffix}"
            Image.new("RGB", (4, 3), "red").save(source, format="JPEG")
            os.utime(source, ns=(original_time, original_time))
            sources.append(source)
        config = replace(self.config, copy_jpeg=True)

        code, stdout, _ = self.run_cli(config)
        self.assertEqual(code, 0)
        self.assertIn("コピー 2", stdout)
        for source in sources:
            output = self.outputs / source.name
            self.assertEqual(output.read_bytes(), source.read_bytes())
            self.assertEqual(output.stat().st_mtime_ns, original_time)
            self.assertFalse((self.outputs / f"{source.stem}.webp").exists())

        with patch.object(app, "convert_image", wraps=app.convert_image) as convert, patch.object(
            app.shutil, "copyfile", wraps=app.shutil.copyfile
        ) as copy:
            code, stdout, _ = self.run_cli(replace(config, quality=90))
        self.assertEqual(code, 0)
        self.assertEqual(convert.call_count, 0)
        self.assertEqual(copy.call_count, 0)
        self.assertIn("再利用 2", stdout)

        code, stdout, _ = self.run_cli(replace(config, copy_jpeg=False))
        self.assertEqual(code, 0)
        self.assertIn("変換 2", stdout)
        for source in sources:
            self.assertTrue((self.outputs / f"{source.stem}.webp").exists())

    def test_collision_names_include_source_extension_only_when_needed(self) -> None:
        Image.new("RGB", (2, 2), "red").save(self.inputs / "sample.png")
        Image.new("RGB", (2, 2), "blue").save(self.inputs / "sample.jpg")
        (self.inputs / "sample.webp").write_bytes(b"copied webp")
        (self.inputs / "sample.png.webp").write_bytes(b"copied name collision")
        jobs, _ = app.plan_jobs(self.config)
        names = {job.relative_source.name: job.destination.name for job in jobs}
        self.assertEqual(names, {
            "sample.png": "sample.png-2.webp",
            "sample.jpg": "sample.jpg.webp",
            "sample.webp": "sample.webp",
            "sample.png.webp": "sample.png.webp",
        })

    def test_failed_conversion_keeps_existing_output(self) -> None:
        (self.inputs / "broken.png").write_bytes(b"not a PNG")
        self.outputs.mkdir()
        existing = self.outputs / "broken.webp"
        existing.write_bytes(b"existing output")
        job = app.plan_jobs(self.config)[0][0]
        with self.assertRaises(OSError):
            app.run_job(job, replace(self.config, overwrite_existing=True))
        self.assertEqual(existing.read_bytes(), b"existing output")
        self.assertEqual(list(self.outputs.glob(".img2webp-*")), [])

    def test_resume_skips_completed_output(self) -> None:
        Image.new("RGB", (2, 2), "red").save(self.inputs / "first.png")
        self.assertEqual(self.run_cli()[0], 0)
        Image.new("RGB", (2, 2), "blue").save(self.inputs / "second.png")

        with patch.object(app, "convert_image", wraps=app.convert_image) as convert:
            code, stdout, _ = self.run_cli()

        self.assertEqual(code, 0)
        self.assertEqual(convert.call_count, 1)
        self.assertIn("再利用 1", stdout)
        self.assertTrue((self.outputs / "first.webp").exists())
        self.assertTrue((self.outputs / "second.webp").exists())

    def test_quality_change_rebuilds_images_but_reuses_copies(self) -> None:
        Image.new("RGB", (2, 2), "red").save(self.inputs / "image.png")
        (self.inputs / "original.avif").write_bytes(b"original bytes")
        self.assertEqual(self.run_cli()[0], 0)

        with patch.object(app, "convert_image", wraps=app.convert_image) as convert, patch.object(
            app.shutil, "copyfile", wraps=app.shutil.copyfile
        ) as copy:
            code, stdout, _ = self.run_cli(replace(self.config, quality=90))

        self.assertEqual(code, 0)
        self.assertEqual(convert.call_count, 1)
        self.assertEqual(copy.call_count, 0)
        self.assertIn("再利用 1", stdout)

    def test_source_or_output_change_rebuilds_tracked_file(self) -> None:
        source = self.inputs / "image.png"
        Image.new("RGB", (2, 2), "red").save(source)
        self.assertEqual(self.run_cli()[0], 0)
        Image.new("RGB", (3, 2), "blue").save(source)

        with patch.object(app, "convert_image", wraps=app.convert_image) as convert:
            self.assertEqual(self.run_cli()[0], 0)
        self.assertEqual(convert.call_count, 1)

        output = self.outputs / "image.webp"
        output.write_bytes(b"modified outside")
        with patch.object(app, "convert_image", wraps=app.convert_image) as convert:
            self.assertEqual(self.run_cli()[0], 0)
        self.assertEqual(convert.call_count, 1)
        with Image.open(output) as image:
            self.assertEqual(image.size, (3, 2))

    def test_untracked_output_requires_explicit_adoption(self) -> None:
        source = self.inputs / "image.png"
        Image.new("RGB", (2, 2), "red").save(source)
        self.outputs.mkdir()
        existing = self.outputs / "image.webp"
        existing.write_bytes(b"old output")

        code, stdout, _ = self.run_cli(replace(self.config, overwrite_existing=False))
        self.assertEqual(code, 0)
        self.assertIn("上書きは無効", stdout)
        self.assertEqual(existing.read_bytes(), b"old output")

        with patch.object(app, "convert_image", wraps=app.convert_image) as convert:
            self.assertEqual(self.run_cli()[0], 0)
        self.assertEqual(convert.call_count, 1)

        (self.outputs / app.STATE_DATABASE_NAME).unlink()
        with patch.object(app, "convert_image", wraps=app.convert_image) as convert:
            code, stdout, _ = self.run_cli(replace(self.config, adopt_untracked=True))
        self.assertEqual(code, 0)
        self.assertEqual(convert.call_count, 0)
        self.assertIn("登録 1", stdout)
        self.assertEqual(self.run_cli()[0], 0)

    def test_graceful_interrupt_commits_pending_records(self) -> None:
        Image.new("RGB", (2, 2), "red").save(self.inputs / "first.png")
        Image.new("RGB", (2, 2), "blue").save(self.inputs / "second.png")
        original_run_job = app.run_job
        calls = 0

        def interrupt_on_second(job: app.Job, config: app.Config) -> app.JobResult | None:
            nonlocal calls
            calls += 1
            if calls == 2:
                raise KeyboardInterrupt
            return original_run_job(job, config)

        with patch.object(app, "run_job", side_effect=interrupt_on_second):
            code, _, _ = self.run_cli(replace(self.config, db_commit_every=32))
        self.assertEqual(code, 130)

        with patch.object(app, "convert_image", wraps=app.convert_image) as convert:
            code, stdout, _ = self.run_cli()
        self.assertEqual(code, 0)
        self.assertEqual(convert.call_count, 1)
        self.assertIn("再利用 1", stdout)

    def test_mismatched_extension_is_reported(self) -> None:
        source = self.inputs / "wrong.png"
        Image.new("RGB", (2, 2)).save(source, format="JPEG")
        job = app.plan_jobs(self.config)[0][0]
        with self.assertRaises(app.ConversionError):
            app.run_job(job, self.config)
        self.assertFalse((self.outputs / "wrong.webp").exists())

    def test_jpeg_orientation_and_cmyk(self) -> None:
        exif = piexif.dump({"0th": {piexif.ImageIFD.Orientation: 6}})
        Image.new("RGB", (2, 3), "green").save(self.inputs / "rotated.jpg", exif=exif)
        Image.new("CMYK", (2, 3), (0, 200, 0, 0)).save(self.inputs / "cmyk.jpg")

        self.run_all()
        with Image.open(self.outputs / "rotated.webp") as image:
            self.assertEqual(image.size, (3, 2))
            self.assertNotIn(piexif.ImageIFD.Orientation, image.getexif())
        with Image.open(self.outputs / "cmyk.webp") as image:
            self.assertEqual(image.mode, "RGB")

    def test_config_validation(self) -> None:
        config_text = (app.PROJECT_DIR / "config.ini").read_text(encoding="utf-8")
        config_text = config_text.replace("input_dir = inputs", f"input_dir = {self.inputs}")
        config_text = config_text.replace("output_dir = outputs", f"output_dir = {self.outputs}")
        config_path = self.root / "config.ini"
        config_path.write_text(config_text, encoding="utf-8")
        loaded = app.load_config(config_path)
        self.assertTrue(loaded.copy_jpeg)
        self.assertTrue(0 <= loaded.quality <= 100)
        self.assertGreater(loaded.db_commit_every, 0)
        invalid_text = re.sub(r"(?m)^quality\s*=\s*\d+\s*$", "quality = 101", config_text)
        self.assertNotEqual(invalid_text, config_text)
        config_path.write_text(invalid_text, encoding="utf-8")
        with self.assertRaises(app.ConfigError):
            app.load_config(config_path)
        invalid_batch = re.sub(r"(?m)^db_commit_every\s*=\s*\d+\s*$", "db_commit_every = 0", config_text)
        self.assertNotEqual(invalid_batch, config_text)
        config_path.write_text(invalid_batch, encoding="utf-8")
        with self.assertRaises(app.ConfigError):
            app.load_config(config_path)

    def test_first_run_creates_missing_input_and_output_directories(self) -> None:
        self.inputs.rmdir()

        code, stdout, stderr = self.run_cli()

        self.assertEqual(code, 0, stderr)
        self.assertTrue(self.inputs.is_dir())
        self.assertTrue(self.outputs.is_dir())
        self.assertIn("失敗 0", stdout)
        self.assertEqual(self.run_cli()[0], 0)

    def test_input_path_is_file_reports_config_error_without_modifying_it(self) -> None:
        self.inputs.rmdir()
        self.inputs.write_bytes(b"keep this file")

        code, _, stderr = self.run_cli()

        self.assertEqual(code, 2)
        self.assertIn("入力フォルダにファイル", stderr)
        self.assertEqual(self.inputs.read_bytes(), b"keep this file")
        self.assertFalse(self.outputs.exists())

    def test_bad_config_encoding_reports_error_without_creating_directories(self) -> None:
        self.inputs.rmdir()
        config_path = self.root / "config.ini"
        config_path.write_bytes(b"\xff")
        stdout, stderr = StringIO(), StringIO()

        with patch.object(app, "PROJECT_DIR", self.root), redirect_stdout(stdout), redirect_stderr(stderr):
            code = app.main()

        self.assertEqual(code, 2)
        self.assertIn("設定エラー", stderr.getvalue())
        self.assertFalse(self.inputs.exists())
        self.assertFalse(self.outputs.exists())

    def test_interrupt_during_initial_scan_closes_state_database(self) -> None:
        connection = Mock()

        with patch.object(app.sqlite3, "connect", return_value=connection), patch.object(
            app, "initialize_state"
        ), patch.object(app, "plan_jobs", side_effect=KeyboardInterrupt):
            code, _, stderr = self.run_cli()

        self.assertEqual(code, 130)
        self.assertIn("中断しました", stderr)
        connection.close.assert_called_once()

    def test_cli_continues_after_bad_image(self) -> None:
        Image.new("RGB", (2, 2), "red").save(self.inputs / "good.png")
        (self.inputs / "bad.png").write_bytes(b"bad image")
        result, stdout, stderr = self.run_cli()

        self.assertEqual(result, 1)
        self.assertTrue((self.outputs / "good.webp").exists())
        self.assertIn("失敗 1", stdout)
        self.assertIn("bad.png", stderr)

    def test_windows_creation_time_uses_available_access_flag(self) -> None:
        handle = Mock()
        win32con = SimpleNamespace(
            GENERIC_WRITE=0x40000000,
            FILE_SHARE_READ=1,
            FILE_SHARE_WRITE=2,
            FILE_SHARE_DELETE=4,
            OPEN_EXISTING=3,
        )
        win32file = SimpleNamespace(CreateFile=Mock(return_value=handle), SetFileTime=Mock())
        pywintypes = SimpleNamespace(Time=Mock(side_effect=lambda value: value))
        modules = {"win32con": win32con, "win32file": win32file, "pywintypes": pywintypes}
        output = self.root / "out.webp"
        source_stat = SimpleNamespace(st_birthtime=123, st_ctime=456)

        with patch.object(app.os, "name", "nt"), patch.dict(sys.modules, modules):
            app.set_windows_creation_time(output, source_stat)

        self.assertEqual(win32file.CreateFile.call_args.args[1], win32con.GENERIC_WRITE)
        win32file.SetFileTime.assert_called_once_with(handle, 123, None, None)
        handle.Close.assert_called_once()


if __name__ == "__main__":
    unittest.main()
