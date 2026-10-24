//! portable node 引导（docs/structure.md §3.24 第 3 步）。
//!
//! 只用系统自带工具：`curl.exe` 下载、`certutil -hashfile` 算 SHA256、`tar.exe` 解压。
//! 校验策略与上游 `build-capsule.mjs` 一致：**每个能取到的 `SHASUMS256.txt` 都必须与本地
//! 哈希一致**，一个都对不上（或一份清单都取不到）就拒绝解压。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::BuildError;
use crate::host::{os, Command, Host, Outcome};
use crate::request::{BuildRequest, Step};

/// node 分发源（按顺序尝试；第二项只作兜底）。
pub const NODE_BASES: [&str; 2] = [
    "https://cdn.npmmirror.com/binaries/node",
    "https://nodejs.org/dist",
];

/// 下载器 / 校验器 / 解压器（系统自带；路径用 `SystemRoot` 拼，别写死盘符）。
pub fn system_tool(name: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
    PathBuf::from(root).join("System32").join(name)
}

/// runtime 就绪后的实测信息（写进 `instance.json` 的 `runtime{}`，充当原本 lock 的审计落脚点）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RuntimeInfo {
    /// `vX.Y.Z`
    pub version: String,
    /// 用到的 zip 名（复用已有 runtime 时为 `None`）。
    pub zip_name: Option<String>,
    /// zip 的 SHA256（与两源清单比对通过的那个）。
    pub zip_sha256: Option<String>,
    /// 这把 zip 从哪个镜像下的。
    pub source: Option<String>,
    /// 是否直接复用了已有 `runtime\`。
    pub reused: bool,
}

/// 确保 `<capsule>\runtime\` 是指定版本的 portable node。
pub fn ensure_runtime(
    request: &BuildRequest,
    capsule_root: &Path,
    host: &dyn Host,
) -> Result<RuntimeInfo, BuildError> {
    let version = request.node_version_normalized();
    let node_exe = capsule_root.join("runtime").join("node.exe");

    if node_exe.is_file() {
        let observed = node_version(host, &node_exe, capsule_root)?;
        if chain(observed.trim()) == chain(&version) {
            host.log(&format!(
                "runtime 复用：{}（{}）",
                observed.trim(),
                node_exe.display()
            ));
            return Ok(RuntimeInfo {
                version: observed.trim().to_string(),
                reused: true,
                ..RuntimeInfo::default()
            });
        }
        host.log(&format!(
            "runtime 版本不符：已有 {}，要求 {} —— 重建",
            observed.trim(),
            version
        ));
    }

    if request.dry_run {
        host.log(&format!(
            "[dry] 将下载 {zip} 并双源校验，然后解压到 {}",
            capsule_root.join("runtime").display(),
            zip = zip_name(&version)
        ));
        return Ok(RuntimeInfo {
            version,
            ..RuntimeInfo::default()
        });
    }

    let download_dir = capsule_root.join("cache").join("download");
    let extract_dir = capsule_root.join("cache").join("extract");
    mkdir(&download_dir)?;
    mkdir(&extract_dir)?;

    let zip = zip_name(&version);
    let zip_path = download_dir.join(&zip);
    let mut source: Option<String> = None;

    if zip_path.is_file() {
        host.log(&format!(
            "复用已下载的 {}（仍会校验哈希）",
            zip_path.display()
        ));
    } else {
        let mut last_error = String::new();
        for base in NODE_BASES {
            let url = format!("{base}/{version}/{zip}");
            host.log(&format!("下载 {url}"));
            match download(host, &url, &zip_path, capsule_root) {
                Ok(outcome) if outcome.ok() => {
                    source = Some(base.to_string());
                    break;
                }
                Ok(outcome) => {
                    last_error = format!("curl 退出码 {}：{}", outcome.code, outcome.stderr_tail)
                }
                Err(err) => last_error = err.to_string(),
            }
            host.log(&format!("镜像失败（{base}）：{last_error}"));
        }
        if !zip_path.is_file() {
            return Err(BuildError::step(
                Step::Runtime,
                format!("node ZIP 所有镜像都下载失败：{last_error}"),
            ));
        }
        let size = fs::metadata(&zip_path).map(|meta| meta.len()).unwrap_or(0);
        host.log(&format!("下载完成：{}（{size} 字节）", zip_path.display()));
    }

    let local_sha = sha256_of(host, &zip_path, capsule_root)?;
    host.log(&format!("本地 SHA256：{local_sha}"));

    let mut expected: Vec<(String, String)> = Vec::new();
    for base in NODE_BASES {
        let url = format!("{base}/{version}/SHASUMS256.txt");
        match fetch_text(host, &url, capsule_root) {
            Ok(text) => match parse_shasums(&text, &zip) {
                Some(sha) => {
                    expected.push((base.to_string(), sha));
                    host.log(&format!("取到校验清单（{base}）"));
                }
                None => host.log(&format!("清单里找不到 {zip}（{base}）")),
            },
            Err(err) => host.log(&format!("取校验清单失败（{base}）：{err}")),
        }
    }
    if expected.is_empty() {
        return Err(BuildError::step(
            Step::Runtime,
            "没有任何 SHA256 清单可用，拒绝解压未经校验的 runtime".to_string(),
        ));
    }
    for (base, sha) in &expected {
        if !chain(sha).eq(&chain(&local_sha)) {
            return Err(BuildError::step(
                Step::Runtime,
                format!("SHA256 不符：本地 {local_sha} ≠ {base} 的 {sha}"),
            ));
        }
    }
    if source.is_none() {
        source = expected.first().map(|(base, _)| base.clone());
    }
    host.log(&format!("SHA256 校验通过（{} 份清单）", expected.len()));

    // 解压：清空 extract → tar -xf → 把内层 node-v*-win-x64 挪成 runtime\。
    remove_dir(&extract_dir)?;
    mkdir(&extract_dir)?;
    let tar = system_tool("tar.exe");
    let outcome = host.run(
        &Command::new("解压 node ZIP", &tar, capsule_root)
            .arg("-xf")
            .arg(os(&zip_path))
            .arg("-C")
            .arg(os(&extract_dir)),
    )?;
    if !outcome.ok() {
        return Err(BuildError::step(
            Step::Runtime,
            format!(
                "解压失败（{}，退出码 {}）：{}",
                tar.display(),
                outcome.code,
                outcome.stderr_tail
            ),
        ));
    }
    let inner = find_inner_dir(&extract_dir)?;
    let runtime_dir = capsule_root.join("runtime");
    remove_dir(&runtime_dir)?;
    if let Err(err) = fs::rename(&inner, &runtime_dir) {
        // 同卷 rename 正常不会失败；跨卷 / 权限异常时退回递归复制。
        host.log(&format!("rename 失败（{err}），改用复制"));
        copy_dir(&inner, &runtime_dir)?;
    }

    let observed = node_version(host, &node_exe, capsule_root)?;
    if !chain(observed.trim()).eq(&chain(&version)) {
        return Err(BuildError::step(
            Step::Runtime,
            format!("runtime 版本不符：{} ≠ {version}", observed.trim()),
        ));
    }
    host.log(&format!("runtime 就绪：{}", observed.trim()));
    Ok(RuntimeInfo {
        version: observed.trim().to_string(),
        zip_name: Some(zip),
        zip_sha256: Some(local_sha),
        source,
        reused: false,
    })
}

/// 跑 `node.exe --version` 并回显。
pub fn node_version(host: &dyn Host, node_exe: &Path, cwd: &Path) -> Result<String, BuildError> {
    let outcome = host.run(
        &Command::new("查询 node 版本", node_exe, cwd)
            .arg("--version")
            .capture(),
    )?;
    if !outcome.ok() {
        return Err(BuildError::step(
            Step::Runtime,
            format!(
                "{} --version 失败（退出码 {}）",
                node_exe.display(),
                outcome.code
            ),
        ));
    }
    Ok(outcome.stdout.trim().to_string())
}

/// `node-v24.22.0-win-x64.zip`
pub fn zip_name(version: &str) -> String {
    format!("node-{version}-win-x64.zip")
}

fn download(host: &dyn Host, url: &str, target: &Path, cwd: &Path) -> Result<Outcome, BuildError> {
    host.run(
        &Command::new(format!("下载 {url}"), system_tool("curl.exe"), cwd)
            .arg("-fsSL")
            .arg("--retry")
            .arg("2")
            .arg("--connect-timeout")
            .arg("20")
            .arg("--max-time")
            .arg("900")
            .arg("-o")
            .arg(os(target))
            .arg(url),
    )
}

fn fetch_text(host: &dyn Host, url: &str, cwd: &Path) -> Result<String, BuildError> {
    let outcome = host.run(
        &Command::new(format!("取 {url}"), system_tool("curl.exe"), cwd)
            .arg("-fsSL")
            .arg("--connect-timeout")
            .arg("20")
            .arg("--max-time")
            .arg("120")
            .arg(url)
            .capture(),
    )?;
    if !outcome.ok() {
        return Err(BuildError::step(
            Step::Runtime,
            format!("取 {url} 失败（退出码 {}）", outcome.code),
        ));
    }
    Ok(outcome.stdout)
}

fn sha256_of(host: &dyn Host, file: &Path, cwd: &Path) -> Result<String, BuildError> {
    let outcome = host.run(
        &Command::new("校验 ZIP 的 SHA256", system_tool("certutil.exe"), cwd)
            .arg("-hashfile")
            .arg(os(file))
            .arg("SHA256")
            .capture(),
    )?;
    if !outcome.ok() {
        return Err(BuildError::step(
            Step::Runtime,
            format!("certutil 计算哈希失败（退出码 {}）", outcome.code),
        ));
    }
    parse_certutil_hash(&outcome.stdout).ok_or_else(|| {
        BuildError::step(
            Step::Runtime,
            format!("certutil 输出里找不到 SHA256 行：{}", outcome.stdout.trim()),
        )
    })
}

/// 从 `certutil -hashfile` 的输出里挑出那行 64 位十六进制。
pub fn parse_certutil_hash(stdout: &str) -> Option<String> {
    stdout.lines().map(str::trim).find_map(|line| {
        let hex = line.replace(' ', "");
        if hex.len() == 64 && hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
            Some(hex.to_ascii_lowercase())
        } else {
            None
        }
    })
}

/// 从 `SHASUMS256.txt` 里取 `zip_name` 那一行的哈希。
pub fn parse_shasums(text: &str, zip_name: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?;
            let name = parts.next()?.trim_start_matches('*');
            if name == zip_name && sha.len() == 64 && sha.chars().all(|ch| ch.is_ascii_hexdigit()) {
                Some(sha.to_ascii_lowercase())
            } else {
                None
            }
        })
}

fn find_inner_dir(extract_dir: &Path) -> Result<PathBuf, BuildError> {
    let entries = fs::read_dir(extract_dir).map_err(|err| {
        BuildError::step(
            Step::Runtime,
            format!("读解压目录失败（{}）：{err}", extract_dir.display()),
        )
    })?;
    let mut found: Option<PathBuf> = None;
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("node-v") {
            if found.is_some() {
                return Err(BuildError::step(
                    Step::Runtime,
                    format!("解压目录里有多个 node-v* 子目录：{}", extract_dir.display()),
                ));
            }
            found = Some(entry.path());
        }
    }
    found.ok_or_else(|| {
        BuildError::step(
            Step::Runtime,
            format!("解压目录里没有 node-v* 子目录：{}", extract_dir.display()),
        )
    })
}

fn mkdir(path: &Path) -> Result<(), BuildError> {
    fs::create_dir_all(path).map_err(|err| {
        BuildError::step(
            Step::Skeleton,
            format!("建目录失败（{}）：{err}", path.display()),
        )
    })
}

fn remove_dir(path: &Path) -> Result<(), BuildError> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BuildError::step(
            Step::Runtime,
            format!("删目录失败（{}）：{err}", path.display()),
        )),
    }
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), BuildError> {
    fs::create_dir_all(to).map_err(|err| {
        BuildError::step(
            Step::Runtime,
            format!("建目录失败（{}）：{err}", to.display()),
        )
    })?;
    for entry in fs::read_dir(from)
        .map_err(|err| BuildError::step(Step::Runtime, format!("读目录失败：{err}")))?
        .flatten()
    {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|err| {
                BuildError::step(
                    Step::Runtime,
                    format!("复制失败（{}）：{err}", entry.path().display()),
                )
            })?;
        }
    }
    Ok(())
}

/// 版本比较用小写化（`V24.22.0` 与 `v24.22.0` 视为同一个）。
fn chain(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

/// `OsString` 便捷构造（对外暴露给测试）。
pub fn arg(value: &str) -> OsString {
    OsString::from(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shasums_parsing() {
        let text = "\
d1e2f3  node-v24.22.0-win-x64.zip
aabbcc  node-v24.22.0-win-arm64.zip

";
        assert_eq!(
            parse_shasums(text, "node-v24.22.0-win-x64.zip"),
            None,
            "非 64 位的行不算"
        );
        let real = format!(
            "{}  node-v24.22.0-win-x64.zip\n{} *node-v24.22.0-win-arm64.zip\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(
            parse_shasums(&real, "node-v24.22.0-win-x64.zip"),
            Some("a".repeat(64))
        );
        assert_eq!(
            parse_shasums(&real, "node-v24.22.0-win-arm64.zip"),
            Some("b".repeat(64)),
            "`*` 前缀（二进制模式）要去掉"
        );
        assert_eq!(parse_shasums(&real, "nope.zip"), None);
    }

    #[test]
    fn certutil_hash_parsing() {
        let stdout = "\
SHA256 的哈希(文件 D:\\x\\node.zip):
0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0
CertUtil: -hashfile 命令成功完成。
";
        assert_eq!(
            parse_certutil_hash(stdout),
            Some("0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c4b5a69788796a5b4c3d2e1f0".to_string())
        );
        assert_eq!(parse_certutil_hash("no hash here"), None);
    }

    #[test]
    fn zip_name_matches_upstream() {
        assert_eq!(zip_name("v24.22.0"), "node-v24.22.0-win-x64.zip");
    }

    #[test]
    fn system_tools_resolve_under_system32() {
        let tar = system_tool("tar.exe");
        assert!(
            tar.to_string_lossy().ends_with(r"System32\tar.exe"),
            "{}",
            tar.display()
        );
    }
}
