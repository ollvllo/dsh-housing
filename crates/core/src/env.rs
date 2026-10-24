//! 环境块与命令行的**纯数据组装**（docs/structure.md §3.5）。
//!
//! 复刻胶囊 `start.cmd` / `start-web.cmd` 的语义，但**不做它们的 fail-fast 校验、
//! 不建目录**（用户 2026-10-19 裁决 D1/D8）：缺 `node.exe` 由 `CreateProcessW` 报错，
//! 缺 profile / workspace 由 dsh 自己报错。
//!
//! 本模块不碰任何进程 / 文件：输入 `InstanceSpec` + 继承环境，输出
//! `CreateProcessW` 需要的命令行缓冲区与环境块缓冲区。这样单测不需要起进程。
//!
//! 两条契约（§3.2 / §3.5）：
//!
//! - **不传 `--host`**：host 由 profile patch 决定，启动器只传 `--profile` 与 `--port`；
//! - 参数顺序：launcher 参数（`--profile`）在前、app 参数（`--port` / `--no-open`）在后。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::model::InstanceSpec;

/// `NO_PROXY` 的固定值（与 `start.cmd` 逐字一致）。
pub const NO_PROXY_VALUE: &str = "localhost,127.0.0.1,deepseek.com,api.deepseek.com";

/// 一个实例的启动计划：`CreateProcessW` 需要什么，这里就有什么。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchPlan {
    /// `lpApplicationName`：`<capsule>\runtime\node.exe`。
    pub program: PathBuf,
    /// `lpCommandLine`：UTF-16 + NUL，可直接交给 `CreateProcessW`。
    pub command_line: Vec<u16>,
    /// `lpCurrentDirectory`：该 profile 的 `workspace`。
    pub cwd: PathBuf,
    /// `lpEnvironment`：UTF-16 环境块，以两个 NUL 结尾（配 `CREATE_UNICODE_ENVIRONMENT`）。
    pub env_block: Vec<u16>,
    /// 命令行里第一个参数（`lib\bin.js`），供日志展示。
    pub script: PathBuf,
}

/// 组装失败的原因（都是数据问题，不涉及 Win32）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvError {
    context: &'static str,
    detail: String,
}

impl EnvError {
    fn new(context: &'static str, detail: impl Into<String>) -> Self {
        Self {
            context,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for EnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.context, self.detail)
    }
}

impl std::error::Error for EnvError {}

/// 生产入口：用本进程的继承环境组装。
pub fn build(spec: &InstanceSpec) -> Result<LaunchPlan, EnvError> {
    build_with(spec, std::env::vars())
}

/// 可注入继承环境的版本（单测用）。
pub fn build_with(
    spec: &InstanceSpec,
    inherited: impl IntoIterator<Item = (String, String)>,
) -> Result<LaunchPlan, EnvError> {
    let root = spec.capsule_root.as_path();
    let runtime = root.join("runtime");
    let node = runtime.join("node.exe");
    let script = root
        .join("app")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");

    let mut entries = EnvEntries::from_inherited(inherited);

    // ── 复刻 start.cmd 的子进程环境（顺序无关，键名按 start.cmd 原样） ──
    entries.set("DSH_HOME", path_text(&root.join("home"), "DSH_HOME")?);
    entries.set("DSH_WEB_PORT", spec.port.to_string());
    entries.set("DSH_TELEMETRY_DISABLED", "1".to_string());
    entries.set(
        "NPM_CONFIG_CACHE",
        path_text(&root.join("cache").join("npm"), "NPM_CONFIG_CACHE")?,
    );
    let cache_tmp = root.join("cache").join("tmp");
    let tmp = path_text(&cache_tmp, "TEMP")?;
    entries.set("TEMP", tmp.to_string());
    entries.set("TMP", tmp.to_string());
    entries.set("NO_PROXY", NO_PROXY_VALUE.to_string());
    entries.set("no_proxy", NO_PROXY_VALUE.to_string());

    // PATH 前置胶囊自带的三段，保留继承值（§3.5：不改系统 PATH）。
    let path_prefix = format!(
        "{};{};{}",
        path_text(&runtime, "PATH")?,
        path_text(&root.join("app").join("node_modules").join("pnpm"), "PATH")?,
        path_text(&root.join("app").join("node_modules").join(".bin"), "PATH")?
    );
    let inherited_path = entries.get("PATH").unwrap_or_default();
    let path = if inherited_path.is_empty() {
        format!("{path_prefix};")
    } else {
        format!("{path_prefix};{inherited_path}")
    };
    entries.set("PATH", path);

    // 实例级 env 覆盖最后叠加（§3.5：改完重启实例才生效）。
    for (key, value) in &spec.env {
        entries.set(key.clone(), value.clone());
    }

    let env_block = entries.to_block()?;

    let command_line = build_command_line(&node, &script, spec)?;

    Ok(LaunchPlan {
        program: node,
        command_line,
        cwd: spec.workspace.clone(),
        env_block,
        script,
    })
}

/// 命令行 = `"<node.exe>" "<bin.js>" --profile <p> --port <n> [--host <ip>] --no-open`。
///
/// `--no-open` 恒定传（§3.6）：现有任务计划本来就带它，由启动器决定何时开浏览器。
///
/// `--host` 只在实例显式设置了 host 时传（v0.3 / §3.22）：缺省不传，host 由 profile patch
/// 决定；显式值必须是具体 IP（`net::normalize_instance_host` 在这里再校验一次，防止手改
/// `capsules.json` 绕过控制台校验）。
fn build_command_line(
    node: &Path,
    script: &Path,
    spec: &InstanceSpec,
) -> Result<Vec<u16>, EnvError> {
    let mut args: Vec<String> = vec![
        path_text(node, "命令行")?.to_string(),
        path_text(script, "命令行")?.to_string(),
        "--profile".to_string(),
        spec.profile.clone(),
        "--port".to_string(),
        spec.port.to_string(),
    ];
    if let Some(host) = spec.host.as_deref() {
        let host = crate::net::normalize_instance_host(host)
            .map_err(|err| EnvError::new("命令行", err))?;
        if let Some(host) = host {
            args.push("--host".to_string());
            args.push(host);
        }
    }
    args.push("--no-open".to_string());

    let text = args
        .iter()
        .map(|arg| quote_arg(arg))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(wide(&text))
}

/// Windows 的 argv 引号规则（CommandLineToArgvW / MS C runtime）。
pub(crate) fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                out.push_str(&"\\".repeat(backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    out.push_str(&"\\".repeat(backslashes * 2));
    out.push('"');
    out
}

fn path_text<'a>(path: &'a Path, context: &'static str) -> Result<&'a str, EnvError> {
    path.to_str()
        .ok_or_else(|| EnvError::new(context, format!("路径不是合法 UTF-8：{}", path.display())))
}

pub(crate) fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 大小写不敏感的环境变量集合（Windows 语义：同名不同大小写是同一个变量）。
struct EnvEntries {
    entries: BTreeMap<String, (String, String)>,
}

impl EnvEntries {
    fn from_inherited(inherited: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut entries = BTreeMap::new();
        for (key, value) in inherited {
            entries.insert(normalize(&key), (key, value));
        }
        Self { entries }
    }

    fn set(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        self.entries.insert(normalize(&key), (key, value.into()));
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .get(&normalize(key))
            .map(|(_, value)| value.as_str())
    }

    /// 生成 `CREATE_UNICODE_ENVIRONMENT` 用的块：`KEY=VALUE\0…\0`，按 key 排序。
    fn to_block(&self) -> Result<Vec<u16>, EnvError> {
        let mut block = Vec::new();
        for (key, value) in self.entries.values() {
            if key.contains('=') && !key.starts_with('=') {
                return Err(EnvError::new(
                    "组装环境块",
                    format!("变量名里不能有 '='：{key:?}"),
                ));
            }
            if key.contains('\0') || value.contains('\0') {
                return Err(EnvError::new(
                    "组装环境块",
                    format!("变量 {key} 的键或值里含 NUL，Windows 环境块放不下"),
                ));
            }
            block.extend(key.encode_utf16());
            block.push('=' as u16);
            block.extend(value.encode_utf16());
            block.push(0);
        }
        block.push(0);
        Ok(block)
    }
}

fn normalize(key: &str) -> String {
    key.to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn spec() -> InstanceSpec {
        InstanceSpec {
            capsule_root: PathBuf::from(r"D:\capsules\0.2.0-rc.2-default"),
            capsule_id: "0.2.0-rc.2-default".to_string(),
            profile: "web".to_string(),
            port: 39412,
            host: None,
            workspace: PathBuf::from(r"D:\capsules\0.2.0-rc.2-default\workspace-web"),
            env: BTreeMap::new(),
            alias: None,
        }
    }

    fn base_env() -> Vec<(String, String)> {
        vec![
            ("SystemRoot".to_string(), r"C:\Windows".to_string()),
            ("Path".to_string(), r"C:\Windows\system32".to_string()),
            ("USERPROFILE".to_string(), r"C:\Users\tester".to_string()),
        ]
    }

    fn parse_block(block: &[u16]) -> Vec<(String, String)> {
        let text = String::from_utf16(block).expect("环境块应是合法 UTF-16");
        text.split('\0')
            .filter(|item| !item.is_empty())
            .map(|item| match item.split_once('=') {
                Some((key, value)) => (key.to_string(), value.to_string()),
                None => panic!("环境项缺少 '='：{item:?}"),
            })
            .collect()
    }

    fn env_map(block: &[u16]) -> HashMap<String, String> {
        parse_block(block)
            .into_iter()
            .map(|(key, value)| (key.to_ascii_uppercase(), value))
            .collect()
    }

    fn command_line_text(plan: &LaunchPlan) -> String {
        assert_eq!(
            *plan.command_line.last().unwrap(),
            0,
            "命令行必须以 NUL 结尾"
        );
        String::from_utf16(&plan.command_line[..plan.command_line.len() - 1])
            .expect("命令行应是合法 UTF-16")
    }

    #[test]
    fn command_line_matches_capsule_contract() {
        let plan = build_with(&spec(), base_env()).unwrap();
        let line = command_line_text(&plan);
        assert_eq!(
            line,
            r#"D:\capsules\0.2.0-rc.2-default\runtime\node.exe D:\capsules\0.2.0-rc.2-default\app\node_modules\@deepseek-ai\dsh\lib\bin.js --profile web --port 39412 --no-open"#
        );
        assert!(
            line.find("--profile").unwrap() < line.find("--port").unwrap(),
            "launcher 参数必须在 app 参数之前"
        );
        assert!(!line.contains("--host"), "缺省不传 --host（§3.2）");
        assert_eq!(
            plan.program,
            PathBuf::from(r"D:\capsules\0.2.0-rc.2-default\runtime\node.exe")
        );
        assert_eq!(
            plan.cwd,
            PathBuf::from(r"D:\capsules\0.2.0-rc.2-default\workspace-web")
        );
    }

    #[test]
    fn explicit_host_is_passed_and_normalized() {
        let mut custom = spec();
        custom.host = Some(" 192.168.3.192 ".to_string());
        let line = command_line_text(&build_with(&custom, base_env()).unwrap());
        assert!(
            line.ends_with("--port 39412 --host 192.168.3.192 --no-open"),
            "{line}"
        );

        custom.host = Some(String::new());
        let line = command_line_text(&build_with(&custom, base_env()).unwrap());
        assert!(!line.contains("--host"), "空串等于不传：{line}");
    }

    #[test]
    fn wildcard_host_is_rejected_before_spawn() {
        let mut custom = spec();
        custom.host = Some("0.0.0.0".to_string());
        let err = build_with(&custom, base_env()).expect_err("wildcard 必须被拒");
        assert!(err.to_string().contains("通配"), "{err}");
    }

    #[test]
    fn env_block_replicates_start_cmd() {
        let plan = build_with(&spec(), base_env()).unwrap();
        let env = env_map(&plan.env_block);
        let root = r"D:\capsules\0.2.0-rc.2-default";

        assert_eq!(env["DSH_HOME"], format!(r"{root}\home"));
        assert_eq!(env["DSH_WEB_PORT"], "39412");
        assert_eq!(env["DSH_TELEMETRY_DISABLED"], "1");
        assert_eq!(env["NPM_CONFIG_CACHE"], format!(r"{root}\cache\npm"));
        assert_eq!(env["TEMP"], format!(r"{root}\cache\tmp"));
        assert_eq!(env["TMP"], format!(r"{root}\cache\tmp"));
        assert_eq!(env["NO_PROXY"], NO_PROXY_VALUE);
        assert_eq!(
            env["PATH"],
            format!(
                r"{root}\runtime;{root}\app\node_modules\pnpm;{root}\app\node_modules\.bin;C:\Windows\system32"
            )
        );
        // 继承的环境变量保留
        assert_eq!(env["SYSTEMROOT"], r"C:\Windows");
        assert_eq!(env["USERPROFILE"], r"C:\Users\tester");
    }

    #[test]
    fn env_override_wins_and_keeps_last_spelling() {
        let mut custom = spec();
        custom
            .env
            .insert("dsh_home".to_string(), r"D:\override\home".to_string());
        custom.env.insert("MY_FLAG".to_string(), "on".to_string());

        let plan = build_with(&custom, base_env()).unwrap();
        let entries = parse_block(&plan.env_block);

        let dsh_home = entries
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case("DSH_HOME"))
            .collect::<Vec<_>>();
        assert_eq!(dsh_home.len(), 1, "同名不同大小写只能留一项");
        assert_eq!(dsh_home[0].0, "dsh_home", "保留覆盖者给出的写法");
        assert_eq!(dsh_home[0].1, r"D:\override\home");

        let env = env_map(&plan.env_block);
        assert_eq!(env["MY_FLAG"], "on");
    }

    #[test]
    fn env_block_is_sorted_and_double_nul_terminated() {
        let plan = build_with(&spec(), base_env()).unwrap();
        assert_eq!(*plan.env_block.last().unwrap(), 0);
        assert_eq!(
            *plan.env_block.get(plan.env_block.len() - 2).unwrap(),
            0,
            "环境块以两个 NUL 结尾"
        );

        let keys: Vec<String> = parse_block(&plan.env_block)
            .into_iter()
            .map(|(key, _)| key.to_ascii_uppercase())
            .collect();
        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "环境块按 key 排序");
    }

    #[test]
    fn args_with_spaces_are_quoted() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg("has space"), "\"has space\"");
        assert_eq!(quote_arg("half\"quote"), "\"half\\\"quote\"");
        // 不含空白的参数不需要引号；需要引号时，结尾的反斜杠要成对。
        assert_eq!(quote_arg("tail\\"), "tail\\");
        assert_eq!(quote_arg("tail space\\"), "\"tail space\\\\\"");
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn nul_in_env_value_is_rejected() {
        let mut custom = spec();
        custom.env.insert("BAD".to_string(), "a\0b".to_string());
        let err = build_with(&custom, base_env()).expect_err("含 NUL 必须报错");
        assert_eq!(err.context, "组装环境块");
    }
}
