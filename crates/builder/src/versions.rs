//! 版本目录：面板下拉的数据源（docs/structure.md §3.24 / §5.14 D43）。
//!
//! 用系统 `curl.exe` 取两份 JSON：npm registry 的 `@deepseek-ai/dsh` packument
//! （只要 `dist-tags` 与版本列表）与 node 的 `index.json`（只要 v24 线）。
//! 结果缓存在 `.dsh-housing\builder\versions.json`；抓不到就回缓存，缓存也没有就空列表
//! ——**构建本身不依赖它**，面板退化为手填。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::BuildError;
use crate::host::{Command, Host};
use crate::node::system_tool;

/// 缓存多久算新鲜（秒）。
pub const CACHE_TTL_SECONDS: u64 = 3600;
/// node 版本只保留 LTS 主线（与上游 `build-capsule.mjs` 的 v24 线一致）。
pub const NODE_MAJOR_LINE: &str = "v24.";
/// node 版本列表最多回多少条（面板下拉够用）。
pub const NODE_LIST_LIMIT: usize = 12;

/// node `index.json` 的镜像（第一项是 npmmirror，实测 200）。
pub const NODE_INDEX_URLS: [&str; 2] = [
    "https://cdn.npmmirror.com/binaries/node/index.json",
    "https://nodejs.org/dist/index.json",
];

/// 版本目录。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    /// 抓取时间（Unix 秒；0 = 从未抓到）。
    pub fetched_at: u64,
    /// `latest` / `next` / `alpha` …
    pub dsh_tags: BTreeMap<String, String>,
    /// 发布过的版本（新的在前）。
    pub dsh_versions: Vec<String>,
    /// v24 线的 node 版本（新的在前）。
    pub node_versions: Vec<String>,
}

/// `dsh-housing\builder\versions.json` 的默认位置。
pub fn cache_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("builder").join("versions.json")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 缓存是否还在 TTL 内。
pub fn is_fresh(catalog: &Catalog, now: u64) -> bool {
    catalog.fetched_at > 0 && now.saturating_sub(catalog.fetched_at) < CACHE_TTL_SECONDS
}

/// 读缓存（坏了当没有）。
pub fn load(path: &Path) -> Option<Catalog> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// 写缓存（顺带建目录；失败只告警，不影响构建）。
pub fn save(path: &Path, catalog: &Catalog) -> Result<(), BuildError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            BuildError::invalid(format!(
                "建版本目录缓存目录失败（{}）：{err}",
                parent.display()
            ))
        })?;
    }
    let text = serde_json::to_string_pretty(catalog)
        .map_err(|err| BuildError::invalid(format!("序列化版本目录失败：{err}")))?;
    fs::write(path, format!("{text}\n")).map_err(|err| {
        BuildError::invalid(format!("写版本目录缓存失败（{}）：{err}", path.display()))
    })
}

/// 抓一次版本目录。
///
/// 两份 JSON 各自独立：一边失败不影响另一边；**两边都失败**才报错。
pub fn fetch(host: &dyn Host, registry: &str, cwd: &Path) -> Result<Catalog, BuildError> {
    let mut catalog = Catalog {
        fetched_at: now_unix(),
        ..Catalog::default()
    };
    let mut errors: Vec<String> = Vec::new();

    let packument_url = format!("{registry}@deepseek-ai/dsh");
    match curl_text(host, &packument_url, cwd, 60) {
        Ok(text) => match parse_dsh_packument(&text) {
            Ok((tags, versions)) => {
                catalog.dsh_tags = tags;
                catalog.dsh_versions = versions;
            }
            Err(err) => errors.push(format!("解析 {packument_url} 失败：{err}")),
        },
        Err(err) => errors.push(format!("取 {packument_url} 失败：{err}")),
    }

    let mut node_ok = false;
    for url in NODE_INDEX_URLS {
        match curl_text(host, url, cwd, 120) {
            Ok(text) => match parse_node_index(&text) {
                Ok(versions) => {
                    catalog.node_versions = versions;
                    node_ok = true;
                    break;
                }
                Err(err) => errors.push(format!("解析 {url} 失败：{err}")),
            },
            Err(err) => errors.push(format!("取 {url} 失败：{err}")),
        }
    }
    if catalog.dsh_versions.is_empty() && !node_ok {
        return Err(BuildError::invalid(format!(
            "版本目录抓取失败：{}",
            errors.join("；")
        )));
    }
    for line in &errors {
        host.log(&format!("版本目录：{line}"));
    }
    Ok(catalog)
}

fn curl_text(host: &dyn Host, url: &str, cwd: &Path, max_time: u64) -> Result<String, BuildError> {
    let outcome = host.run(
        &Command::new(format!("取版本目录 {url}"), system_tool("curl.exe"), cwd)
            .arg("-fsSL")
            .arg("--connect-timeout")
            .arg("20")
            .arg("--max-time")
            .arg(max_time.to_string())
            .arg(url)
            .capture(),
    )?;
    if !outcome.ok() {
        return Err(BuildError::invalid(format!(
            "curl 退出码 {}（stdout {} 字节；stderr：{}）",
            outcome.code,
            outcome.stdout.len(),
            outcome.stderr_tail.trim()
        )));
    }
    if outcome.stdout.trim().is_empty() {
        return Err(BuildError::invalid(format!(
            "curl 退出码 0 但 stdout 为空（stderr：{}）",
            outcome.stderr_tail.trim()
        )));
    }
    Ok(outcome.stdout)
}

/// 解析 npm packument：`dist-tags` + `versions` 的键（新的在前）。
pub fn parse_dsh_packument(text: &str) -> Result<(BTreeMap<String, String>, Vec<String>), String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let mut tags = BTreeMap::new();
    if let Some(map) = value.get("dist-tags").and_then(|tags| tags.as_object()) {
        for (key, tag) in map {
            if let Some(version) = tag.as_str() {
                tags.insert(key.clone(), version.to_string());
            }
        }
    }
    let mut versions: Vec<String> = value
        .get("versions")
        .and_then(|versions| versions.as_object())
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default();
    versions.reverse(); // packument 的键按发布时间升序
    Ok((tags, versions))
}

/// 解析 node `index.json`：只留 v24 线，最多 [`NODE_LIST_LIMIT`] 条。
pub fn parse_node_index(text: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
    let array = value
        .as_array()
        .ok_or_else(|| "index.json 不是数组".to_string())?;
    let mut versions: Vec<String> = Vec::new();
    for item in array {
        if let Some(version) = item.get("version").and_then(|version| version.as_str()) {
            if version.starts_with(NODE_MAJOR_LINE) {
                versions.push(version.to_string());
            }
        }
    }
    versions.truncate(NODE_LIST_LIMIT);
    Ok(versions)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PACKUMENT: &str = r#"{
        "dist-tags": {"latest": "0.2.1-rc.2", "next": "0.2.1-rc.3", "alpha": "0.2.1-alpha.5"},
        "versions": {
            "0.2.0-rc.2": {"version": "0.2.0-rc.2"},
            "0.2.1-rc.2": {"version": "0.2.1-rc.2"},
            "0.2.1-rc.3": {"version": "0.2.1-rc.3"}
        }
    }"#;

    #[test]
    fn packument_parsing_keeps_tags_and_versions_newest_first() {
        let (tags, versions) = parse_dsh_packument(PACKUMENT).expect("应能解析");
        assert_eq!(tags["next"], "0.2.1-rc.3");
        assert_eq!(tags.len(), 3);
        assert_eq!(versions, vec!["0.2.1-rc.3", "0.2.1-rc.2", "0.2.0-rc.2"]);
    }

    #[test]
    fn node_index_parsing_filters_v24_line() {
        let text = r#"[
            {"version": "v25.1.0", "lts": false},
            {"version": "v24.22.1", "lts": "Krypton"},
            {"version": "v24.22.0", "lts": "Krypton"},
            {"version": "v23.0.0", "lts": false}
        ]"#;
        assert_eq!(
            parse_node_index(text).expect("应能解析"),
            vec!["v24.22.1", "v24.22.0"]
        );
    }

    #[test]
    fn node_index_limit_applies() {
        let items: Vec<String> = (0..20)
            .map(|i| format!(r#"{{"version": "v24.{i}.0", "lts": "Krypton"}}"#))
            .collect();
        let text = format!("[{}]", items.join(","));
        let versions = parse_node_index(&text).expect("应能解析");
        assert_eq!(versions.len(), NODE_LIST_LIMIT);
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(parse_dsh_packument("not json").is_err());
        assert!(parse_node_index("{}").is_err());
    }

    #[test]
    fn freshness_uses_ttl() {
        let catalog = Catalog {
            fetched_at: 1_000,
            ..Catalog::default()
        };
        assert!(is_fresh(&catalog, 1_000 + CACHE_TTL_SECONDS - 1));
        assert!(!is_fresh(&catalog, 1_000 + CACHE_TTL_SECONDS));
        assert!(
            !is_fresh(&Catalog::default(), 10_000),
            "从未抓到过 = 不新鲜"
        );
    }
}
