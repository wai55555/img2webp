@echo off
setlocal DisableDelayedExpansion
rem Prefer the Rust binary (img2webp.exe next to this file, or the release
rem build under rust\target\release). Fall back to the Python version while the
rem binary has not been deployed yet, so the launcher keeps working during the
rem migration (see doc/img2webp-rust-migration.md).
if exist "%~dp0img2webp.exe" (
    "%~dp0img2webp.exe"
) else if exist "%~dp0rust\target\release\img2webp.exe" (
    "%~dp0rust\target\release\img2webp.exe"
) else (
    python "%~dp0img2webp.py"
)
set "result=%errorlevel%"
if not "%result%"=="0" pause
exit /b %result%
