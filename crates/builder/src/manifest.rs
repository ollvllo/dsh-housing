//! 瘦 `instance.json`（docs/structure.md §3.24 第 6 步）。
//!
//! 只写启动器真正要读的字段（`capsule.rs` 的解析形状），外加 `runtime{}` 里那几项
//! 审计信息——原本这些在 `capsule.lock.json` 里，v0.5 起不写 lock 了（§5.14 D41）。

use std::fs;
use std::path::{Path, PathBuf};

use crate::app::{AppInfo, DSH_BIN, DSH_PACKAGE};
use crate::error::BuildError;
use crate::node::RuntimeInfo;
use crate::request::{BuildRequest, Step};

/// 启动器复刻 `start.cmd` 时只认回环（局域网是另一个经评审的决定）。
pub const DEFAULT_HOST: &str = "127.0.0.1";

/// shipped profile 对应的 bundle 列表（dsh `PROFILE_TEMPLATES` 的副本，只用于展示/清单）。
pub fn bundles_for(profile: &str) -> Vec<&'static str> {
    match profile {
        "web" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-web-app"],
        "headless" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-headless"],
        "acp" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-acp-app"],
        "sdk" => vec!["@deepseek-ai/dsh-base", "@deepseek-ai/dsh-sdk-app"],
        "sdk-minimal" => vec!["@deepseek-ai/dsh-sdk-minimal"],
        _ => vec!["@deepseek-ai/dsh-base"],
    }
}

/// 组装 `instance.json` 的内容。
pub fn instance_json(
    request: &BuildRequest,
    capsule_root: &Path,
    runtime: &RuntimeInfo,
    app: &AppInfo,
) -> serde_json::Value {
    let node_version = runtime.version.trim_start_matches(['v', 'V']).to_string();
    let bundles = bundles_for(&request.profile);
    serde_json::json!({
        "schema": 1,
        "instanceId": request.capsule_id(),
        "capsuleRoot": capsule_root.display().to_string(),
        "purpose": format!(
            "dsh 可移植版本 {}（{} 出厂默认档案、不叠加 --patch；由 dsh-housing 构建器生成）",
            request.name, request.profile
        ),
        "runtime": {
            "kind": "node-portable-zip",
            "version": node_version,
            "archive": runtime.zip_name,
            "sha256": runtime.zip_sha256,
            "source": runtime.source,
            "checksumVerified": runtime.zip_sha256.is_some(),
            "sha256Note": "zip 的 SHA256 已与镜像的 SHASUMS256.txt 比对；解压后不再单独记 node.exe 哈希",
        },
        "app": {
            "package": DSH_PACKAGE,
            "version": app.version,
            "bin": DSH_BIN,
            "pnpm": app.pnpm_version,
            "note": "实例本地安装（app\\node_modules）；不用 npm -g，不改系统 PATH",
        },
        "profile": request.profile,
        "host": DEFAULT_HOST,
        "hostNote": "只绑回环。局域网暴露是另一个经评审的决定（控制台的实例 host 可覆盖）。",
        "port": request.port,
        "workspace": capsule_root.join("workspace").display().to_string(),
        "homeNote": "home\\ 是唯一 DSH_HOME（档案 / 插件 / 会话 / 设置 / 凭据全在里面）；复制整个目录即连凭据一起复制，只允许留在本机受控目录",
        "patches": [],
        "bundles": bundles,
        "stateModel": "complete-instance-clone",
        "builtAt": request.built_at,
        "builder": "dsh-housing v0.5 crates/builder（node 引导 + app 安装 + 模板 + 瘦清单）",
    })
}

/// 写 `<capsule>\instance.json`（先写临时文件再改名，避免半份 json）。
pub fn write(capsule_root: &Path, value: &serde_json::Value) -> Result<PathBuf, BuildError> {
    let target = capsule_root.join("instance.json");
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(value).map_err(|err| {
            BuildError::step(Step::Manifest, format!("序列化 instance.json 失败：{err}"))
        })?
    );
    let temp = capsule_root.join("instance.json.tmp");
    fs::write(&temp, text).map_err(|err| {
        BuildError::step(Step::Manifest, format!("写 {} 失败：{err}", temp.display()))
    })?;
    fs::rename(&temp, &target).map_err(|err| {
        BuildError::step(
            Step::Manifest,
            format!("改名 {} → {} 失败：{err}", temp.display(), target.display()),
        )
    })?;
    Ok(target)
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
            dry_run: false,
            built_at: "2026-10-24 14:05:00 +08:00".to_string(),
        }
    }

    fn runtime() -> RuntimeInfo {
        RuntimeInfo {
            version: "v24.22.0".to_string(),
            zip_name: Some("node-v24.22.0-win-x64.zip".to_string()),
            zip_sha256: Some("a".repeat(64)),
            source: Some("https://cdn.npmmirror.com/binaries/node".to_string()),
            reused: false,
        }
    }

    fn app() -> AppInfo {
        AppInfo {
            version: "0.2.1-rc.3".to_string(),
            pnpm_version: "12.4.1".to_string(),
            reused: false,
        }
    }

    #[test]
    fn instance_json_shape_matches_launcher_contract() {
        let value = instance_json(
            &request(),
            Path::new(r"D:\c\0.2.1-rc.3-demo"),
            &runtime(),
            &app(),
        );
        assert_eq!(value["schema"], 1);
        assert_eq!(value["instanceId"], "0.2.1-rc.3-demo");
        assert_eq!(value["profile"], "web");
        assert_eq!(value["port"], 39414);
        assert_eq!(value["runtime"]["kind"], "node-portable-zip");
        assert_eq!(
            value["runtime"]["version"], "24.22.0",
            "runtime.version 不带 v"
        );
        assert_eq!(value["app"]["package"], "@deepseek-ai/dsh");
        assert_eq!(value["app"]["version"], "0.2.1-rc.3");
        assert_eq!(value["app"]["bin"], "lib/bin.js");
        assert_eq!(value["bundles"][1], "@deepseek-ai/dsh-web-app");
        assert!(value["workspace"].as_str().unwrap().ends_with(r"workspace"));
    }

    #[test]
    fn reused_runtime_leaves_audit_fields_empty() {
        let reused = RuntimeInfo {
            version: "v24.22.0".to_string(),
            ..RuntimeInfo::default()
        };
        let value = instance_json(&request(), Path::new(r"D:\c"), &reused, &app());
        assert!(value["runtime"]["sha256"].is_null());
        assert_eq!(value["runtime"]["checksumVerified"], false);
    }

    #[test]
    fn bundles_cover_shipped_profiles() {
        for profile in crate::request::SHIPPED_PROFILES {
            if *profile == "desktop" {
                continue;
            }
            assert!(!bundles_for(profile).is_empty(), "{profile}");
        }
        assert_eq!(bundles_for("web").len(), 2);
        assert_eq!(
            bundles_for("sdk-minimal"),
            vec!["@deepseek-ai/dsh-sdk-minimal"]
        );
    }
}
