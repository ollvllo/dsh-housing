@echo off
rem 打印档案（profile: {{PROFILE}}）实际组合出来的配置树 —— 不叠加任何 --patch。
rem 注意：--dump-config 是 **launcher 参数**，必须排在所有 app 参数之前。
rem 用法：scripts\dump.cmd            输出配置树（stdout）
rem        scripts\dump.cmd > d.yml   落盘后比对
setlocal
set "ROOT=%~dp0..\"
rem 把 scripts\..\ 规范化成真实绝对路径（否则 DSH_HOME 里会带着 scripts\..\ 传给子进程）
pushd "%ROOT%"
set "ROOT=%CD%\"
popd
set "PROFILE={{PROFILE}}"
set "NODE=%ROOT%runtime\node.exe"
set "DSH_BIN=%ROOT%app\node_modules\@deepseek-ai\dsh\lib\bin.js"
set "DSH_HOME=%ROOT%home"
set "DSH_TELEMETRY_DISABLED=1"
set "NO_PROXY=localhost,127.0.0.1,deepseek.com,api.deepseek.com"
set "PATH=%ROOT%runtime;%ROOT%app\node_modules\.bin;%PATH%"

"%NODE%" "%DSH_BIN%" --profile %PROFILE% --dump-config

set "RC=%ERRORLEVEL%"
exit /b %RC%
