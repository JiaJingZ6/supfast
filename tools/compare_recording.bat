@echo off
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0compare_recording.ps1" %*
exit /b %ERRORLEVEL%
