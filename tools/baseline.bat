@echo off
rem supfast 对拍基准 launcher（真机播放验证前置）
rem   Usage: baseline.bat [-Samples fade,split,test2,drama] [-MaxPng 8] [-NoPng]
rem   Full parameters: see header of baseline.ps1
setlocal
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0baseline.ps1" %*
exit /b %ERRORLEVEL%
