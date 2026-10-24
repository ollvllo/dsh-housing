//! dsh-housing 可移植版本构建器（Vikunja #249，docs/structure.md §3.24）。
//!
//! 边界：**只决定「做什么」**，不碰 Win32——进程 spawn / Job Object / 日志环由
//! `crates/core/src/build.rs` 通过 [`Host`] 提供。于是本 crate 能脱离 Windows 单测
//! （假 host 记录命令、回放输出）。
//!
//! 六步：目录骨架 → portable node → app 安装 → 模板渲染 → 瘦 `instance.json` → 3 条断言。
//! 剔掉了上游 `build-capsule.mjs` 的「home 出厂档案」（dsh 启动时自己建）与「冒烟」
//! （v0.5 由用户验收），理由见 §5.14 D37 / D44。

pub mod app;
pub mod error;
pub mod host;
pub mod manifest;
pub mod node;
pub mod request;
pub mod templates;
pub mod versions;

use std::fs;
use std::path::PathBuf;

pub use app::{AppInfo, DEFAULT_PNPM, DSH_BIN, DSH_PACKAGE};
pub use error::BuildError;
pub use host::{Command, Host, Outcome};
pub use node::RuntimeInfo;
pub use request::{dir_is_non_empty, BuildRequest, Step, SHIPPED_PROFILES};
pub use versions::Catalog;

/// 目录骨架（与上游 `build-capsule.mjs` 的 `DIRS` 同集合）。
pub const SKELETON_DIRS: [&str; 10] = [
    "runtime",
    "app",
    "home",
    "workspace",
    "logs",
    "scripts",
    "cache/download",
    "cache/extract",
    "cache/tmp",
    "cache/npm",
];

/// 一次构建成功后交给 core 的结果（core 用它登记胶囊）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildReport {
    /// `<root>\<name>`
    pub capsule_root: PathBuf,
    /// 登记的胶囊标识（= 目录名）。
    pub capsule_id: String,
    /// 实测 dsh 版本。
    pub dsh_version: String,
    /// 实测 node 版本（带 `v`）。
    pub node_version: String,
    /// runtime 是否是复用的。
    pub runtime_reused: bool,
    /// app 是否复用了已装好的 `node_modules`。
    pub app_reused: bool,
    /// 渲染出的模板相对路径。
    pub templates: Vec<String>,
    /// 是不是干跑（干跑不登记）。
    pub dry_run: bool,
}

/// 跑一遍构建。`dry_run` 时只打印计划，不联网、不写盘、不跑 npm。
pub fn run(request: &BuildRequest, host: &dyn Host) -> Result<BuildReport, BuildError> {
    request.validate()?;
    host.log(&format!(
        "构建开始：{}（dsh {} / node {} / profile {} / 端口 {}）",
        request.name,
        request.dsh_version,
        request.node_version_normalized(),
        request.profile,
        request.port
    ));
    if let Some(hint) = request.naming_hint() {
        host.log(&format!("提示：{hint}"));
    }
    if request.dry_run {
        host.log("[dry] 只打印计划：不联网、不写盘、不跑 npm");
    }
    let capsule_root = request.capsule_root();

    // ── 1/6 目录骨架 ──
    host.step(Step::Skeleton);
    check(host, Step::Skeleton)?;
    if request::dir_is_non_empty(&capsule_root) && !request.overwrite {
        return Err(BuildError::step(
            Step::Skeleton,
            format!(
                "目标目录已存在且非空（{}）。继续会覆盖模板与清单、保留 home\\；确认要继续请勾选「覆盖已有目录」",
                capsule_root.display()
            ),
        ));
    }
    if request.dry_run {
        host.log(&format!(
            "[dry] 建目录骨架 {}（{} 个子目录）",
            capsule_root.display(),
            SKELETON_DIRS.len()
        ));
    } else {
        for dir in SKELETON_DIRS {
            let path = capsule_root.join(dir);
            fs::create_dir_all(&path).map_err(|err| {
                BuildError::step(
                    Step::Skeleton,
                    format!("建目录失败（{}）：{err}", path.display()),
                )
            })?;
        }
        host.log(&format!("目录骨架就绪：{}", capsule_root.display()));
    }

    // ── 2/6 portable node ──
    host.step(Step::Runtime);
    check(host, Step::Runtime)?;
    let runtime = node::ensure_runtime(request, &capsule_root, host)?;
    host.log(&format!(
        "runtime：{}（{}）",
        runtime.version,
        if runtime.reused { "复用" } else { "新建" }
    ));

    // ── 3/6 app 安装 ──
    host.step(Step::App);
    check(host, Step::App)?;
    let app = app::ensure_app(request, &capsule_root, host)?;
    host.log(&format!(
        "app：{DSH_PACKAGE} {}（{}）",
        app.version,
        if app.reused { "复用" } else { "新装" }
    ));

    // ── 4/6 启动器模板 ──
    host.step(Step::Templates);
    check(host, Step::Templates)?;
    let rendered = templates::render(request, &runtime.version, &capsule_root, host)?;

    // ── 5/6 instance.json ──
    host.step(Step::Manifest);
    check(host, Step::Manifest)?;
    let manifest = manifest::instance_json(request, &capsule_root, &runtime, &app);
    if request.dry_run {
        host.log(&format!(
            "[dry] 写 {}（{} 字节序列化）",
            capsule_root.join("instance.json").display(),
            serde_json::to_string_pretty(&manifest)
                .map(|text| text.len())
                .unwrap_or(0)
        ));
    } else {
        let path = manifest::write(&capsule_root, &manifest)?;
        host.log(&format!("清单已写：{}", path.display()));
    }

    // ── 6/6 版本断言 ──
    host.step(Step::Verify);
    check(host, Step::Verify)?;
    if request.dry_run {
        host.log("[dry] 跳过 3 条版本断言（没有产物）");
    } else {
        verify(request, &capsule_root, &runtime, &app, host)?;
    }

    if request.dry_run {
        host.log("[dry] 计划结束：未联网、未写盘");
    } else {
        host.log("构建成功");
    }
    Ok(BuildReport {
        capsule_root,
        capsule_id: request.capsule_id().to_string(),
        dsh_version: app.version.clone(),
        node_version: runtime.version.clone(),
        runtime_reused: runtime.reused,
        app_reused: app.reused,
        templates: rendered,
        dry_run: request.dry_run,
    })
}

/// 3 条断言（§3.24 第 7 步）。
fn verify(
    request: &BuildRequest,
    capsule_root: &std::path::Path,
    runtime: &RuntimeInfo,
    app: &AppInfo,
    host: &dyn Host,
) -> Result<(), BuildError> {
    let node_exe = capsule_root.join("runtime").join("node.exe");
    let observed_node = node::node_version(host, &node_exe, capsule_root)?;
    if observed_node.trim() != runtime.version.trim() {
        return Err(BuildError::step(
            Step::Verify,
            format!(
                "runtime\\node.exe --version = {}，期望 {}",
                observed_node.trim(),
                runtime.version
            ),
        ));
    }
    host.log(&format!(
        "断言 1/3：node.exe --version = {}",
        observed_node.trim()
    ));

    let installed = app::installed_version(capsule_root).ok_or_else(|| {
        BuildError::step(
            Step::Verify,
            format!("读不到 {} 的 package.json", DSH_PACKAGE),
        )
    })?;
    if installed != request.dsh_version || installed != app.version {
        return Err(BuildError::step(
            Step::Verify,
            format!(
                "app 里 {DSH_PACKAGE} = {installed}，期望 {}",
                request.dsh_version
            ),
        ));
    }
    host.log(&format!("断言 2/3：{DSH_PACKAGE} = {installed}"));

    let bin = capsule_root
        .join("app")
        .join("node_modules")
        .join("@deepseek-ai")
        .join("dsh")
        .join("lib")
        .join("bin.js");
    let outcome = host.run(
        &Command::new("dsh --version", &node_exe, capsule_root)
            .arg(&bin)
            .arg("--version")
            .capture(),
    )?;
    if !outcome.ok() {
        return Err(BuildError::step(
            Step::Verify,
            format!(
                "dsh --version 失败（退出码 {}）：{}",
                outcome.code, outcome.stderr_tail
            ),
        ));
    }
    let observed_dsh = outcome.stdout.trim();
    if observed_dsh != app.version {
        return Err(BuildError::step(
            Step::Verify,
            format!("dsh --version = {observed_dsh}，期望 {}", app.version),
        ));
    }
    host.log(&format!("断言 3/3：dsh --version = {observed_dsh}"));

    // 清单必须能被解析（否则 core 登记会失败，早点在这里报）。
    let manifest_path = capsule_root.join("instance.json");
    let text = fs::read_to_string(&manifest_path).map_err(|err| {
        BuildError::step(
            Step::Verify,
            format!("读 {} 失败：{err}", manifest_path.display()),
        )
    })?;
    serde_json::from_str::<serde_json::Value>(&text).map_err(|err| {
        BuildError::step(
            Step::Verify,
            format!("{} 不是合法 JSON：{err}", manifest_path.display()),
        )
    })?;
    Ok(())
}

fn check(host: &dyn Host, step: Step) -> Result<(), BuildError> {
    if host.cancelled() {
        Err(BuildError::cancelled(step))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
