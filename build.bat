@echo off
chcp 65001 >nul
setlocal
cd /d "%~dp0"

set "TC=H:\DIY\rea_sup\rust-toolchain\root"
set "SC=%TC%\lib\rustlib\x86_64-pc-windows-gnu\bin\self-contained"

if not exist "%TC%\bin\cargo.exe" (
  echo [错误] 找不到便携 Rust: %TC%
  pause & exit /b 1
)
if not exist "%SC%\as.exe" (
  echo [警告] self-contained 里没有 as.exe，dlltool 会失败（见 README 的工具链坑）
)
if not exist "%SC%\gcc.exe" (
  echo [警告] self-contained 里没有 gcc.exe 垫片，windres 会失败（tools\gcc_shim.rs）
)
if not exist "icons\icon.ico" (
  echo [1/4] 生成图标...
  python make_icon.py
)

set "PATH=%TC%\bin;%SC%;%PATH%"
set "CARGO_HOME=%USERPROFILE%\.cargo"

echo [2/4] 编译 release...
cargo build --release
if errorlevel 1 goto fail

echo [3/4] 组装 dist\...
if not exist "dist" mkdir "dist"
copy /y "target\release\supfast.exe" "dist\supfast.exe" >nul
rem gnu 目标是动态加载 WebView2，必须带上这个 dll
copy /y "target\release\WebView2Loader.dll" "dist\WebView2Loader.dll" >nul

set "MODSRC=%~dp0..\mod"
if not exist "%MODSRC%\avs2bdnxml.exe" set "MODSRC=H:\DIY\mod"
if exist "%MODSRC%\avs2bdnxml.exe" (
  if not exist "dist\mod" mkdir "dist\mod"
  copy /y "%MODSRC%\avs2bdnxml.exe" "dist\mod\" >nul
  xcopy /e /i /y "%MODSRC%\csri" "dist\mod\csri" >nul
  echo     mod 来自 %MODSRC%
) else (
  echo     [警告] 没找到 mod 目录，dist\ 里缺 avs2bdnxml.exe
)

echo [4/4] 完成: dist\supfast.exe
for %%F in ("dist\supfast.exe") do echo      exe 大小: %%~zF 字节
echo.
echo 冒烟测试:
dist\supfast.exe --engine analyze --ass "%~dp0..\test\test2.ass" --json "%TEMP%\supfast_probe.json"
type "%TEMP%\supfast_probe.json"
pause
exit /b 0

:fail
echo 编译失败
pause
exit /b 1
