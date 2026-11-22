@echo off
setlocal DisableDelayedExpansion
python "%~dp0img2webp.py"
set "result=%errorlevel%"
if not "%result%"=="0" pause
exit /b %result%
