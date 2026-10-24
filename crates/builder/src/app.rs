//! app 安装（docs/structure.md §3.24 第 4 步）。
//!
//! 只做三件事：写 `app\package.json`（两个依赖都精确 pin）、用**胶囊自己的 npm**
//! 在 `app\` 里 `install`、复核装出来的 `@deepseek-ai/dsh` 版本。
//! npm 的 stderr 警告**不算失败**——那是它给用户看的，不是构建判据。

use std::fs;
use std::path::Path;

use crate::error::BuildError;
use crate::host::{os, Command, Host, Outcome};
use crate::request::{BuildRequest, Step};

/// `app\` 里固定安装的 pnpm 版本（dsh 的 Web UI 装插件靠它）。
pub const DEFAULT_PNPM: &str = "12.4.1";

/// `@deepseek-ai/dsh` 的包标识。
pub const DSH_PACKAGE: &str = "@deepseek-ai/dsh";
/// `instance.json` 的 `app.bin`（相对 `app\node_modules\<package>`）。
pub const DSH_BIN: &str = "lib/bin.js";

/// app 就绪后的实测信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub version: String,
    pub pnpm_version: String,
    /// 是否复用了已装好的 `node_modules`。
    pub reused: bool,
}

/// 确保 `<capsule>\app\node_modules` 里是指定版本的 dsh（+ pnpm）。
pub fn ensure_app(
    request: &BuildRequest,
    capsule_root: &Path,
    host: &dyn Host,
) -> Result<AppInfo, BuildError> {
    let app_dir = capsule_root.join("app");
    if !request.dry_run {
        fs::create_dir_all(&app_dir).map_err(|err| {
            BuildError::step(
                Step::App,
                format!("建 app 目录失败（{}）：{err}", app_dir.display()),
            )
        })?;
        fs::write(app_dir.join("package.json"), package_json(request))
            .map_err(|err| BuildError::step(Step::App, format!("写 package.json 失败：{err}")))?;
    }

    let installed = installed_version(capsule_root);
    if !request.force_app {
        if let Some(version) = installed.as_deref() {
            if version == request.dsh_version {
                host.log(&format!(
                    "app 复用：{DSH_PACKAGE} {version}（{}）",
                    app_dir.join("node_modules").display()
                ));
                return Ok(AppInfo {
                    version: version.to_string(),
                    pnpm_version: DEFAULT_PNPM.to_string(),
                    reused: true,
                });
            }
            host.log(&format!(
                "app 里 dsh 版本不符：已有 {version}，要求 {} —— 重装",
                request.dsh_version
            ));
        }
    }

    let npm_cmd = capsule_root.join("runtime").join("npm.cmd");
    if request.dry_run {
        host.log(&format!(
            "[dry] 将在 {} 里跑 {} install（{} + pnpm@{}，registry {}）",
            app_dir.display(),
            npm_cmd.display(),
            request.dsh_version,
            DEFAULT_PNPM,
            request.registry_or_default()
        ));
        return Ok(AppInfo {
            version: request.dsh_version.clone(),
            pnpm_version: DEFAULT_PNPM.to_string(),
            reused: false,
        });
    }
    if !npm_cmd.is_file() {
        return Err(BuildError::step(
            Step::App,
            format!("缺 {}（先确保 runtime 到位）", npm_cmd.display()),
        ));
    }

    if request.force_app {
        let modules = app_dir.join("node_modules");
        match fs::remove_dir_all(&modules) {
            Ok(()) => host.log(&format!("已删除 {}（--force-app）", modules.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(BuildError::step(
                    Step::App,
                    format!("删 node_modules 失败（{}）：{err}", modules.display()),
                ))
            }
        }
    }

    host.log(&format!(
        "npm install 开始（{} + pnpm@{}）——这一步要联网，通常 1–5 分钟",
        request.dsh_version, DEFAULT_PNPM
    ));
    let env = npm_env(request, capsule_root);
    // `cmd.exe` 必须是绝对路径：`CreateProcessW` 拿到 lpApplicationName 时**不搜 PATH**（实测 Win32 2）。
    let cmd_exe = crate::node::system_tool("cmd.exe");
    let outcome = host.run(
        &Command::new("npm install", cmd_exe, &app_dir)
            .arg("/c")
            .arg(os(&npm_cmd))
            .arg("install")
            .arg("--no-audit")
            .arg("--no-fund")
            .env_into(env)
            .path_prepend(capsule_root.join("runtime"))
            .path_prepend(capsule_root.join("app").join("node_modules").join(".bin")),
    )?;
    if !outcome.ok() {
        return Err(BuildError::step(
            Step::App,
            format!(
                "npm install 失败（退出码 {}）：\n{}",
                outcome.code,
                tail(&outcome, 12)
            ),
        ));
    }

    let version = installed_version(capsule_root).ok_or_else(|| {
        BuildError::step(
            Step::App,
            format!(
                "npm install 跑完了但没找到 {DSH_PACKAGE} 的 package.json（{}）",
                app_dir.join("node_modules").join(DSH_PACKAGE).display()
            ),
        )
    })?;
    if version != request.dsh_version {
        return Err(BuildError::step(
            Step::App,
            format!("装完的 dsh 版本不符：{version} ≠ {}", request.dsh_version),
        ));
    }
    host.log(&format!(
        "app 就绪：{DSH_PACKAGE} {version}，pnpm {DEFAULT_PNPM}"
    ));
    Ok(AppInfo {
        version,
        pnpm_version: DEFAULT_PNPM.to_string(),
        reused: false,
    })
}

/// `app\package.json` 的内容（两个依赖精确 pin；不写 scripts）。
pub fn package_json(request: &BuildRequest) -> String {
    let value = serde_json::json!({
        "name": format!("dsh-portable-{}", request.name),
        "private": true,
        "description": "instance-local dsh CLI + pnpm（版本 pin 死；启动不联网、不装包）",
        "dependencies": {
            DSH_PACKAGE: request.dsh_version.clone(),
            "pnpm": DEFAULT_PNPM,
        }
    });
    format!("{value}\n")
}

/// 读 `app\node_modules\@deepseek-ai\dsh\package.json` 的 `version`。
pub fn installed_version(capsule_root: &Path) -> Option<String> {
    let path = capsule_root
        .join("app")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("package.json");
    let text = fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("version")?.as_str().map(str::to_string)
}

fn npm_env(request: &BuildRequest, capsule_root: &Path) -> Vec<(String, String)> {
    let cache = capsule_root.join("cache");
    vec![
        (
            "NPM_CONFIG_CACHE".to_string(),
            cache.join("npm").display().to_string(),
        ),
        ("TEMP".to_string(), cache.join("tmp").display().to_string()),
        ("TMP".to_string(), cache.join("tmp").display().to_string()),
        (
            "NPM_CONFIG_REGISTRY".to_string(),
            request.registry_or_default(),
        ),
        // npm 的进度条在管道里只会变成一堆乱码，关掉。
        ("NPM_CONFIG_PROGRESS".to_string(), "false".to_string()),
        ("NPM_CONFIG_FUND".to_string(), "false".to_string()),
        ("NPM_CONFIG_AUDIT".to_string(), "false".to_string()),
    ]
}

/// 错误信息里只带输出尾部（完整输出已经在日志文件里）。
fn tail(outcome: &Outcome, lines: usize) -> String {
    let mut all: Vec<&str> = Vec::new();
    all.extend(outcome.stdout.lines());
    all.extend(outcome.stderr_tail.lines());
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn request() -> BuildRequest {
        BuildRequest {
            name: "0.2.1-rc.3-demo".to_string(),
            dsh_version: "0.2.1-rc.3".to_string(),
            node_version: "v24.22.0".to_string(),
            profile: "web".to_string(),
            port: 39414,
            root: PathBuf::from(r"D:\Images\DeepseekHarness"),
            registry: None,
            overwrite: false,
            force_app: false,
            dry_run: true,
            built_at: "2026-10-24 14:05:00 +08:00".to_string(),
        }
    }

    #[test]
    fn package_json_pins_both_dependencies() {
        let text = package_json(&request());
        let value: serde_json::Value = serde_json::from_str(&text).expect("应是合法 JSON");
        assert_eq!(value["dependencies"]["@deepseek-ai/dsh"], "0.2.1-rc.3");
        assert_eq!(value["dependencies"]["pnpm"], DEFAULT_PNPM);
        assert_eq!(value["private"], true);
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn npm_env_points_at_capsule_cache() {
        let env = npm_env(&request(), Path::new(r"D:\c\0.2.1-rc.3-demo"));
        let map: std::collections::BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(map["NPM_CONFIG_CACHE"], r"D:\c\0.2.1-rc.3-demo\cache\npm");
        assert_eq!(
            map["NPM_CONFIG_REGISTRY"],
            "https://registry.npmmirror.com/"
        );
        assert_eq!(map["NPM_CONFIG_PROGRESS"], "false");
    }

    #[test]
    fn installed_version_is_none_for_missing_tree() {
        assert_eq!(
            installed_version(Path::new(r"D:\definitely\missing\capsule")),
            None
        );
    }
}
