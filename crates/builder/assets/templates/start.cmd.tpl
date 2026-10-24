@echo off
title Deepseek Harness {{CAPSULE_NAME}}

rem ===========================================================================
rem dsh 本地胶囊启动器 —— 实例 {{CAPSULE_NAME}}
rem
rem   profile = {{PROFILE}}（出厂默认档案；不叠加任何 --patch）
rem   端口    = {{PORT}}（改动时同步改实例登记，别与其它实例撞）
rem   工作目录 = workspace\（dsh 把工作目录注册在 home\storages\workspace.json，
rem              会话按它的路径编码分桶存进 home\sessions\）
rem
rem 用法：
rem   start.cmd              起 Web UI（只绑 127.0.0.1:{{PORT}}）
rem   start.cmd --no-open    同上，但不尝试打开浏览器
rem   call activate-env.cmd  把胶囊的 node / dsh / DSH_HOME 装进当前 cmd 会话
rem   scripts\dump.cmd       打印本 profile 实际组合出来的配置树
rem   scripts\doctor.cmd     只读自检（不启动任何服务）
rem
rem 日志：dsh 的 stdout/stderr 由 scripts\tee.mjs 双写 —— logs\start-YYYY-MM-DD.log 与控制台各一份。
rem 退出码：校验失败 = 1；正常退出 = dsh 自己的退出码（tee.mjs 原样透传）。
rem ===========================================================================
setlocal
set "ROOT=%~dp0"

rem ── 实例参数：改这里（改完重新登记，端口别与其它实例撞） ──
set "PROFILE={{PROFILE}}"
set "WEB_PORT={{PORT}}"
set "WORKSPACE=%ROOT%workspace"

set "NODE=%ROOT%runtime\node.exe"
set "DSH_BIN=%ROOT%app\node_modules\@deepseek-ai\dsh\lib\bin.js"

rem ── 子进程环境：只影响本进程树，绝不改系统/用户 PATH ──
set "DSH_HOME=%ROOT%home"
set "DSH_WEB_PORT=%WEB_PORT%"
set "DSH_TELEMETRY_DISABLED=1"
set "NPM_CONFIG_CACHE=%ROOT%cache\npm"
set "TEMP=%ROOT%cache\tmp"
set "TMP=%ROOT%cache\tmp"
set "NO_PROXY=localhost,127.0.0.1,deepseek.com,api.deepseek.com"
set "no_proxy=%NO_PROXY%"
set "PATH=%ROOT%runtime;%ROOT%app\node_modules\pnpm;%ROOT%app\node_modules\.bin;%PATH%"

rem ── fail-fast 校验（缺失就响亮失败，绝不回退到系统上的 node/dsh） ──
if not exist "%NODE%" (
  echo [start] error: missing bundled runtime: "%NODE%" 1>&2
  exit /b 1
)
if not exist "%DSH_BIN%" (
  echo [start] error: missing dsh bin: "%DSH_BIN%" 1>&2
  exit /b 1
)
if not exist "%DSH_HOME%\profiles\%PROFILE%\package.json" (
  echo [start] error: missing profile "%PROFILE%": "%DSH_HOME%\profiles\%PROFILE%" 1>&2
  exit /b 1
)
if not exist "%ROOT%scripts\tee.mjs" (
  echo [start] error: missing tee helper: "%ROOT%scripts\tee.mjs" 1>&2
  exit /b 1
)

if not exist "%ROOT%cache\tmp" mkdir "%ROOT%cache\tmp"
if not exist "%WORKSPACE%" mkdir "%WORKSPACE%"
cd /d "%WORKSPACE%"
if errorlevel 1 (
  echo [start] error: cannot cd to workspace "%WORKSPACE%" 1>&2
  exit /b 1
)

echo [start] instance={{CAPSULE_NAME}} profile=%PROFILE%
echo [start] DSH_HOME="%DSH_HOME%"
echo [start] workspace="%WORKSPACE%"
echo [start] web=http://127.0.0.1:%WEB_PORT%/  (带 token 的完整 URL 见 dsh 自己的输出)

rem ── 启动：launcher 参数（--profile）在前，app 参数（--port 等）在后 ──
"%NODE%" "%ROOT%scripts\tee.mjs" --dir "%ROOT%logs" --prefix start -- ^
  "%NODE%" "%DSH_BIN%" ^
  --profile %PROFILE% ^
  --port %WEB_PORT% %*

set "RC=%ERRORLEVEL%"
exit /b %RC%
