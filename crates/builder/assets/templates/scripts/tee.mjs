// tee.mjs —— 把 dsh 进程的 stdout / stderr 同时写到「日志文件 + 控制台」（cmd 没有内建 tee）。
//
// 用法（只给 start-web.cmd / start-chat.cmd 调用，不面向人工）：
//   runtime\node.exe scripts\tee.mjs --dir <logsDir> --prefix <name> -- <cmd> [args...]
//
// 输出：
//   - 日志文件 = <logsDir>\<prefix>-YYYY-MM-DD.log（本地日期；追加，同一天多次启动接在同一份里）
//   - 控制台   = 子进程 stdout / stderr 原样透传（字节级：不加行号、不改顺序、不重排）
//   - 退出码   = 子进程退出码；参数错误 / 子进程起不来 = 2
//
// 依赖：只用 node 内置模块 —— capsule 自包含，启动路径不装包。
//
// 为什么不用 PowerShell 的 Tee-Object：Windows PowerShell 5.1 写文件默认 UTF-16LE、
// 经管道的中文按 OEM 代码页解码易乱码，且 Ctrl+C 时父进程先走会留下孤儿 dsh 进程。
// 这里做「字节级双写 + 原样透传退出码」，行为对齐 POSIX 的 `cmd | tee -a log`。
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';

function pad2(value) {
  return String(value).padStart(2, '0');
}

function tzSuffix(now) {
  const minutes = -now.getTimezoneOffset();
  const sign = minutes < 0 ? '-' : '+';
  const abs = Math.abs(minutes);
  return `${sign}${pad2(Math.floor(abs / 60))}:${pad2(abs % 60)}`;
}

function stamp(now = new Date()) {
  return `${now.getFullYear()}-${pad2(now.getMonth() + 1)}-${pad2(now.getDate())}`
    + ` ${pad2(now.getHours())}:${pad2(now.getMinutes())}:${pad2(now.getSeconds())}`
    + ` ${tzSuffix(now)}`;
}

function localDay(now = new Date()) {
  return `${now.getFullYear()}-${pad2(now.getMonth() + 1)}-${pad2(now.getDate())}`;
}

function usage(message) {
  process.stderr.write(
    `${message}\n用法: node tee.mjs --dir <logsDir> --prefix <name> -- <cmd> [args...]\n`,
  );
}

function parseArgs(argv) {
  let logsDir = null;
  let prefix = null;
  let index = 0;
  for (; index < argv.length; index += 1) {
    const arg = argv[index];
    if (arg === '--') {
      index += 1;
      break;
    }
    if (arg === '--dir' && index + 1 < argv.length) {
      logsDir = argv[index + 1];
      index += 1;
      continue;
    }
    if (arg === '--prefix' && index + 1 < argv.length) {
      prefix = argv[index + 1];
      index += 1;
      continue;
    }
    return { error: `tee: 不认识的参数: ${arg}` };
  }
  if (logsDir === null) return { error: 'tee: 缺少 --dir' };
  if (prefix === null) return { error: 'tee: 缺少 --prefix' };
  const cmd = argv.slice(index);
  if (cmd.length === 0) return { error: 'tee: -- 后面没有命令' };
  return { logsDir, prefix, cmd };
}

const parsed = parseArgs(process.argv.slice(2));
if (parsed.error) {
  usage(parsed.error);
  process.exit(2);
}
const { logsDir, prefix, cmd } = parsed;

fs.mkdirSync(logsDir, { recursive: true });
const logPath = path.join(logsDir, `${prefix}-${localDay()}.log`);
const logFd = fs.openSync(logPath, 'a');

const shownCmd = cmd.map((token) => (/\s/.test(token) ? `"${token}"` : token)).join(' ');
const banner = `[tee] log   : ${logPath}\n`
  + `[tee] start : ${stamp()}  cwd=${process.cwd()}\n`
  + `[tee] cmd   : ${shownCmd}\n`;
fs.writeSync(logFd, banner);
process.stdout.write(banner);

// ── 字节级双写 ─────────────────────────────────────────────────────────────
// 日志侧永远写原始块（文件里就是子进程输出的字节流）；控制台侧只在「末尾疑似被
// 切断的 UTF-8 序列」上留手：把不完整的那几个字节攒到下一块再一起写，免得一个
// 多字节字符被拆成两次写、在控制台上显示成乱码。
function incompleteUtf8Tail(buf) {
  if (buf.length === 0) return 0;
  for (let back = 1; back <= 3 && back <= buf.length; back += 1) {
    const byte = buf[buf.length - back];
    if ((byte & 0x80) === 0) return 0; // ASCII：边界就在这里，完整
    if ((byte & 0xc0) === 0xc0) { // 序列首字节
      const need = (byte & 0xe0) === 0xc0 ? 2
        : (byte & 0xf0) === 0xe0 ? 3
          : (byte & 0xf8) === 0xf0 ? 4 : 1;
      return need > back ? back : 0; // 首字节起算不足 need：整段留着
    }
    // 0x80..0xbf 是续字节：继续往前找首字节
  }
  return 3; // 连 3 个续字节（非法输入）：保守留 3
}

function writeScreen(stream, tail, chunk) {
  const buf = tail.length === 0 ? chunk : Buffer.concat([tail, chunk]);
  const keep = incompleteUtf8Tail(buf);
  const cut = buf.length - keep;
  if (cut > 0) stream.write(buf.subarray(0, cut));
  return cut > 0 ? buf.subarray(cut) : buf;
}

const child = spawn(cmd[0], cmd.slice(1), {
  stdio: ['inherit', 'pipe', 'pipe'],
  windowsHide: false,
});

let tailOut = Buffer.alloc(0);
let tailErr = Buffer.alloc(0);
let finished = false;

child.stdout.on('data', (chunk) => {
  fs.writeSync(logFd, chunk);
  tailOut = writeScreen(process.stdout, tailOut, chunk);
});

child.stderr.on('data', (chunk) => {
  fs.writeSync(logFd, chunk);
  tailErr = writeScreen(process.stderr, tailErr, chunk);
});

// close = 两个 stdio 都关完、data 都交付完了。把留手的尾字节放出去、关日志、
// 带上真实退出码。用 process.exitCode 而不是 process.exit()，让 node 先把
// 标准输出 flush 干净再退（stdout 被重定向到文件时尤其重要）。
function finish(code) {
  if (finished) return;
  finished = true;
  if (tailOut.length > 0) process.stdout.write(tailOut);
  if (tailErr.length > 0) process.stderr.write(tailErr);
  try {
    fs.closeSync(logFd);
  } catch {
    // 已经关掉了就无所谓
  }
  process.exitCode = code;
}

child.on('close', (code) => finish(typeof code === 'number' ? code : 1));

child.on('error', (error) => {
  const message = `tee: 子进程起不来: ${error.message}\n`;
  try {
    fs.writeSync(logFd, message); // 日志里也要留下「这次为什么什么都没起」
  } catch {
    // 写不进去就算了，屏幕上还有一份
  }
  process.stderr.write(message);
  finish(2);
});

// Ctrl+C：控制台把同一个 Ctrl+C 交给整个进程组（dsh 也收到），所以第一下不自杀 ——
// 等 dsh 自己收尾退出，好把最后几行输出和真实退出码带出来；连按两下才硬退。
let interrupts = 0;
process.on('SIGINT', () => {
  interrupts += 1;
  if (interrupts >= 2) process.exit(130);
});
