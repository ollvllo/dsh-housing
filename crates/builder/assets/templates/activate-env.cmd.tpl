@echo off
title dsh capsule env ({{CAPSULE_NAME}})

rem ===========================================================================
rem dsh 本地胶囊 —— 环境激活脚本（实例 {{CAPSULE_NAME}}）
rem
rem 作用：把「胶囊自带的 Node」和「胶囊自己的 DSH_HOME」装进**当前 cmd 会话**，
rem       之后手敲的 node / npm / pnpm / dsh 命令都走胶囊内的那一份；
rem       不写系统或用户的环境变量、不碰系统 PATH、不读全局 npm 配置。
rem
rem 与 start.cmd 的关系：**取同一组值**
rem   （DSH_HOME、PATH 前插三项、NPM_CONFIG_CACHE、TEMP/TMP、NO_PROXY、遥测开关），
rem   差别只在作用域 —— 启动器只把环境交给它自己的子进程，本脚本把环境交给
rem   调用它的那个会话，用来手工跑 dsh 子命令（dump / plugin / session 检索等）。
rem   本脚本**不设** DSH_WEB_PORT：端口属于「起哪个档案」，由启动器或命令行决定。
rem
rem 用法（cmd.exe）：
rem   call activate-env.cmd             在当前会话激活（**必须带 call**，否则变量出不去）
rem   call activate-env.cmd -no-temp    同上，但不动 TEMP/TMP
rem   activate-env.cmd -h               帮助
rem   双击 / 直接运行                    自动新开一个已激活的 cmd 窗口
rem
rem 验证：
rem   where node        → <本胶囊>\runtime\node.exe 排第一
rem   echo %DSH_HOME%    → <本胶囊>\home
rem
rem 退出码：0 = 已激活（或已新开窗口）；1 = 校验失败（缺 runtime / 缺 dsh bin / 缺 home）。
rem ===========================================================================

set "_dsh_no_temp="
set "_dsh_standalone="
if /i "%~1"=="-no-temp" set "_dsh_no_temp=1"
if /i "%~1"=="-h" goto :usage
if /i "%~1"=="--help" goto :usage
if /i "%~1"=="/?" goto :usage
if not "%~1"=="" if not defined _dsh_no_temp (
  echo [activate] error: 未知参数 "%~1" 1>&2
  echo            用法见 activate-env.cmd -h 1>&2
  exit /b 1
)

rem ── 判定是否「带 call 调用」：没带 call 变量出不了本进程，末尾改开新窗口 ──
call :detect_standalone

rem ── 定位实例根：%~dp0 → 规范化绝对路径（同 scripts\doctor.cmd 的做法）──
pushd "%~dp0" 2>nul
if errorlevel 1 (
  echo [activate] error: 无法从 "%~dp0" 定位胶囊目录 1>&2
  goto :fail
)
set "_dsh_root=%CD%"
popd
set "_dsh_node=%_dsh_root%\runtime\node.exe"
set "_dsh_dsh=%_dsh_root%\app\node_modules\@deepseek-ai\dsh\lib\bin.js"
set "_dsh_home=%_dsh_root%\home"

rem ── fail-fast 校验：缺就响亮失败，绝不回退到系统上的 node / dsh ──
if not exist "%_dsh_node%" (
  echo [activate] error: 缺胶囊自带 runtime: "%_dsh_node%" 1>&2
  goto :fail
)
if not exist "%_dsh_dsh%" (
  echo [activate] error: 缺 dsh bin: "%_dsh_dsh%" 1>&2
  goto :fail
)
if not exist "%_dsh_home%\profiles" (
  echo [activate] error: 缺 DSH_HOME: "%_dsh_home%" 1>&2
  goto :fail
)

rem ── 重复激活幂等：同一个胶囊再来一次不重复前插 PATH ──
if /i "%DSH_CAPSULE_ROOT%"=="%_dsh_root%" set "_dsh_already=1"

rem ── 写进当前会话（刻意不用 setlocal：变量要留在调用者会话里）──
set "DSH_HOME=%_dsh_home%"
set "DSH_CAPSULE_ROOT=%_dsh_root%"
set "DSH_CAPSULE_NODE=%_dsh_node%"
set "DSH_CAPSULE_DSH_BIN=%_dsh_dsh%"
set "DSH_TELEMETRY_DISABLED=1"
set "NPM_CONFIG_CACHE=%_dsh_root%\cache\npm"
set "NO_PROXY=localhost,127.0.0.1,deepseek.com,api.deepseek.com"
set "no_proxy=%NO_PROXY%"
if not defined _dsh_no_temp (
  if not exist "%_dsh_root%\cache\tmp" mkdir "%_dsh_root%\cache\tmp" 2>nul
  set "TEMP=%_dsh_root%\cache\tmp"
  set "TMP=%_dsh_root%\cache\tmp"
)
if not defined _dsh_already set "PATH=%_dsh_root%\runtime;%_dsh_root%\app\node_modules\pnpm;%_dsh_root%\app\node_modules\.bin;%PATH%"

echo [activate] instance : {{CAPSULE_NAME}}
echo [activate] root     : %_dsh_root%
echo [activate] DSH_HOME : %DSH_HOME%
<nul set /p "=[activate] node     : "
"%_dsh_node%" --version
<nul set /p "=[activate] dsh      : "
"%_dsh_node%" "%_dsh_dsh%" --version
<nul set /p "=[activate] profiles : "
for /d %%P in ("%_dsh_home%\profiles\*") do <nul set /p "=%%~nxP "
echo.
if defined _dsh_already (
  echo [activate] PATH     : 本会话已激活过，不重复前插
) else (
  echo [activate] PATH     : 已前插 runtime\; app\node_modules\pnpm\; app\node_modules\.bin\
)
if defined _dsh_no_temp (
  echo [activate] TEMP/TMP : 未改 ^(-no-temp^)
) else (
  echo [activate] TEMP/TMP : %_dsh_root%\cache\tmp
)
echo [activate] 例（打配置树）: node "%DSH_CAPSULE_DSH_BIN%" --profile {{PROFILE}} --dump-config
echo [activate] 提示：launcher 参数 ^(--profile / --patch / --dump-config^) 必须排在 app 参数前面
echo.

rem ── 收尾：清掉内部变量，调用者会话里只留上面那组 ──
set "_dsh_root="
set "_dsh_node="
set "_dsh_dsh="
set "_dsh_home="
set "_dsh_already="
set "_dsh_no_temp="

if defined _dsh_standalone (
  set "_dsh_standalone="
  echo [activate] 未带 call（双击 / 直接运行）：变量出不了本进程，改开一个已激活的 cmd
  echo [activate] 用完在那个窗口里 exit 即可
  cmd /k
  exit /b 0
)
set "_dsh_standalone="
exit /b 0

rem ===========================================================================
rem 子过程区：主流程在上面就 exit /b 了，正常不会落进来
rem ===========================================================================

:fail
set "_dsh_root="
set "_dsh_node="
set "_dsh_dsh="
set "_dsh_home="
set "_dsh_already="
set "_dsh_no_temp="
set "_dsh_standalone="
exit /b 1

:detect_standalone
rem 判定「是不是被 call 的」：cmdcmdline 里有本脚本名、且没有 call → 双击 / cmd /c。
rem 必须用**延迟展开**读它：cmdcmdline 自身带引号和 &，用 %cmdcmdline% 展开会被二次解析。
rem 也不用 find.exe：PATH 上若有 MSYS 的 find 会被劫持。
rem 注意：本文件若改名，下面那行里的 activate-env.cmd 要一起改。
setlocal enabledelayedexpansion
set "_cc=!cmdcmdline!"
set "_sa="
if not "!_cc!"=="!_cc:activate-env.cmd=!" set "_sa=1"
if not "!_cc!"=="!_cc:call=!" set "_sa="
endlocal & set "_dsh_standalone=%_sa%"
exit /b 0

:usage
echo 用法（cmd.exe）：
echo   call activate-env.cmd             在当前会话激活（必须带 call）
echo   call activate-env.cmd -no-temp    激活但不动 TEMP/TMP
echo   activate-env.cmd -h               本帮助
echo.
echo 作用：把 DSH_HOME 与 PATH 前置指向本胶囊自带的 node / pnpm / dsh；
echo       只改当前 cmd 会话，不写系统或用户的环境变量。
echo 验证：where node   应指向 ^<胶囊^>\runtime\node.exe
echo       echo %%DSH_HOME%%   应为 ^<胶囊^>\home
exit /b 0
