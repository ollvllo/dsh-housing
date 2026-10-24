# 胶囊 {{CAPSULE_NAME}} —— dsh {{DSH_VERSION}}（{{PROFILE}} 出厂默认档案）

> 便携运行时胶囊：自带 portable Node、实例本地 dsh、独立 `home\`；不装全局包、不改系统 PATH。
> 端口 `127.0.0.1:{{PORT}}`（只绑回环）。
> 构建时间：{{BUILT_AT}}；构建器：dsh-housing 控制台里的「构建可移植版本」。

## 目录

```text
runtime\      portable Node {{NODE_VERSION}}（官方 win-x64 ZIP，SHA256 已校验；immutable-by-policy）
app\          实例本地 dsh {{DSH_VERSION}} + pnpm（package.json / package-lock.json 锁版本）
home\         唯一 DSH_HOME（profiles / 会话 / 设置 / 凭据）
workspace\    进程 cwd 与 dsh 注册的工作目录（会话桶按它的路径编码存进 home\sessions）
cache\        npm cache / pnpm store / TEMP（可随时删）
logs\         运维日志（start-YYYY-MM-DD.log 由 scripts\tee.mjs 双写）
scripts\      tee.mjs / dump.cmd / doctor.cmd
start.cmd     启动器（纯 CMD + CRLF）
activate-env.cmd  把胶囊的 node / dsh / DSH_HOME 装进当前 cmd 会话
instance.json     实例意图：版本、端口、档案、分岔流程
```

## 日常命令（cmd）

```cmd
cd /d E:\my-dsh-versions\{{CAPSULE_NAME}}
start.cmd                       :: 起 Web UI（127.0.0.1:{{PORT}}；带 token 的 URL 在输出里）
start.cmd --no-open             :: 同上，不开浏览器
call activate-env.cmd           :: 把胶囊环境装进当前窗口，之后手工跑 dsh 子命令
scripts\doctor.cmd              :: 只读自检
scripts\dump.cmd > cache\d.yml  :: 打本档案组合后的配置树
taskkill /F /PID <netstat 里 {{PORT}} 那一行的 PID>   :: 停服（或直接关窗口）
```

## 纪律

- `home\` 是状态：会话数据不跨版本迁移；`home\.credentials.yaml`（模型 key）由用户自己填，**不读不写不复制**。
- `runtime\` / `app\` 是构建产物：坏了重建（在控制台里重新构建即可，可复用 `cache\npm`，很快），别手改。
- 分岔新实例 = 停机 → 全量复制整个胶囊 → 改副本的 `instance.json` / `start.cmd`（instanceId、端口、workspace）→ 从副本起。
  `home\` 会连凭据一起复制：只留本机受控目录、不进 Git、不外发。
- `logs\` 里会带 dsh 打印的访问 URL（**含 token**）—— 这是 stdout 落盘的必然结果，属本机受控目录。
