@echo off
rem 编译并组装产物目录 dist\ (只保留运行所需: exe + web + plugins + 已有 config.json)
cd /d %~dp0
where cargo >nul 2>nul
if errorlevel 1 (
    echo 未找到 cargo, 请先安装 Rust: https://rustup.rs/
    pause
    exit /b 1
)
echo [1/3] 编译中...
cargo build --release
if errorlevel 1 (
    echo 编译失败
    pause
    exit /b 1
)
echo [2/3] 关闭运行中的实例...
taskkill /IM wmpvp-rank-overlay.exe /F >nul 2>&1
echo [3/3] 组装 dist\...
if not exist dist\plugins mkdir dist\plugins
copy /Y target\release\wmpvp-rank-overlay.exe dist\ >nul
xcopy /E /I /Y web dist\web >nul
rem 本地覆盖层(不入库, 存在则覆盖同名页面)
if exist web.local xcopy /E /I /Y web.local dist\web >nul
xcopy /E /I /Y plugins dist\plugins >nul
echo.
echo 完成: dist\wmpvp-rank-overlay.exe
pause
