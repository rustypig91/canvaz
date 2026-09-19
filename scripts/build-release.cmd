@echo off
rem Allow the unsigned build script for this process only.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-release.ps1" %*
exit /b %errorlevel%
