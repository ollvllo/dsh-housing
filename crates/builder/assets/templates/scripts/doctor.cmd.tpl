@echo off
rem 环境自检：只读，不改任何文件、不起服务。
rem 用法：scripts\doctor.cmd
setlocal
set "ROOT=%~dp0..\"
rem 把 scripts\..\ 规范化成真实绝对路径（否则 DSH_HOME 里会带着 scripts\..\ 传给子进程）；
rem 用 pushd/%CD% 而不是 %%~fI —— 后者对带尾反斜杠的输入会保留尾斜杠，规范化后变成双反斜杠。
pushd "%ROOT%"
set "ROOT=%CD%\"
popd
set "PROFILE={{PROFILE}}"
set "WEB_PORT={{PORT}}"
set "NODE=%ROOT%runtime\node.exe"
set "DSH_BIN=%ROOT%app\node_modules\@deepseek-ai\dsh\lib\bin.js"
set "DSH_HOME=%ROOT%home"
set "PATH=%ROOT%runtime;%ROOT%app\node_modules\pnpm;%ROOT%app\node_modules\.bin;%PATH%"

echo [doctor] instance      : {{CAPSULE_NAME}}
echo [doctor] instance root : "%ROOT%"
echo [doctor] DSH_HOME      : "%DSH_HOME%"
echo [doctor] profile       : %PROFILE%
echo [doctor] web port      : %WEB_PORT%  ^(只绑 127.0.0.1^)
echo.

if exist "%NODE%" (
  <nul set /p "=[doctor] node          : "
  "%NODE%" --version
) else (
  echo [doctor] FAIL missing runtime: "%NODE%"
)
if exist "%DSH_BIN%" (
  <nul set /p "=[doctor] dsh           : "
  "%NODE%" "%DSH_BIN%" --version
) else (
  echo [doctor] FAIL missing dsh bin: "%DSH_BIN%"
)
if exist "%DSH_HOME%\profiles\%PROFILE%\package.json" (
  echo [doctor] profile files : OK
) else (
  echo [doctor] FAIL missing profile package.json
)
if exist "%ROOT%scripts\tee.mjs" (
  echo [doctor] tee helper    : OK
) else (
  echo [doctor] FAIL missing scripts\tee.mjs
)
if exist "%ROOT%app\node_modules\pnpm\pnpm.exe" (
  <nul set /p "=[doctor] pnpm          : "
  "%ROOT%app\node_modules\pnpm\pnpm.exe" --version
) else (
  echo [doctor] WARN missing pnpm.exe ^(Web UI 里装插件会失败^)
)

echo.
echo [doctor] 端口占用（空 = 空闲）:
netstat -ano | findstr /c:":%WEB_PORT%" | findstr /i "LISTENING"
echo.
echo [doctor] 完（自检不启动任何服务）
exit /b 0
