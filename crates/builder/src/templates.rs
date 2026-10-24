//! 启动器模板渲染（docs/structure.md §3.24 第 5 步）。
//!
//! 模板真源是上游 `DshMaintainerAgentWorkspace/scripts/capsule-templates/`，这里放的是
//! **逐字节副本**（`crates/builder/assets/templates/`）。改模板要改上游那份再同步过来，
//! 别只改这里——两份漂移时 `docs/structure.md` §6 记的 sha256 前缀能查出来。

use std::fs;
use std::path::Path;

use crate::error::BuildError;
use crate::host::Host;
use crate::request::{BuildRequest, Step};

/// 一个要渲染的模板文件。
struct Template {
    /// 目标相对路径（`.tpl` 会被去掉）。
    target: &'static str,
    /// 源文件名（只在日志里用）。
    source: &'static str,
    content: &'static [u8],
    /// 是否强制 CRLF（`.cmd` / `.bat`）。
    crlf: bool,
    /// 是否逐字节复制（不替换占位符）。
    raw: bool,
}

const TEMPLATES: &[Template] = &[
    Template {
        target: "start.cmd",
        source: "start.cmd.tpl",
        content: include_bytes!("../assets/templates/start.cmd.tpl"),
        crlf: true,
        raw: false,
    },
    Template {
        target: "activate-env.cmd",
        source: "activate-env.cmd.tpl",
        content: include_bytes!("../assets/templates/activate-env.cmd.tpl"),
        crlf: true,
        raw: false,
    },
    Template {
        target: "README.md",
        source: "README.md.tpl",
        content: include_bytes!("../assets/templates/README.md.tpl"),
        crlf: false,
        raw: false,
    },
    Template {
        target: "scripts/dump.cmd",
        source: "scripts/dump.cmd.tpl",
        content: include_bytes!("../assets/templates/scripts/dump.cmd.tpl"),
        crlf: true,
        raw: false,
    },
    Template {
        target: "scripts/doctor.cmd",
        source: "scripts/doctor.cmd.tpl",
        content: include_bytes!("../assets/templates/scripts/doctor.cmd.tpl"),
        crlf: true,
        raw: false,
    },
    Template {
        target: "scripts/tee.mjs",
        source: "scripts/tee.mjs",
        content: include_bytes!("../assets/templates/scripts/tee.mjs"),
        crlf: false,
        raw: true,
    },
];

/// 渲染全部模板到 `<capsule>` 根；返回写出的相对路径（顺序同 [`TEMPLATES`]）。
pub fn render(
    request: &BuildRequest,
    runtime_version: &str,
    capsule_root: &Path,
    host: &dyn Host,
) -> Result<Vec<String>, BuildError> {
    let values = placeholders(request, runtime_version);
    let mut written = Vec::new();
    for template in TEMPLATES {
        if request.dry_run {
            host.log(&format!(
                "[dry] 渲染 {} → {}",
                template.source, template.target
            ));
            written.push(template.target.to_string());
            continue;
        }
        let body = if template.raw {
            template.content.to_vec()
        } else {
            let text = std::str::from_utf8(template.content).map_err(|err| {
                BuildError::step(
                    Step::Templates,
                    format!("模板不是合法 UTF-8（{}）：{err}", template.source),
                )
            })?;
            let mut rendered = render_text(template.source, text, &values)?;
            if template.crlf {
                rendered = to_crlf(&rendered);
            }
            rendered.into_bytes()
        };
        let target = capsule_root.join(template.target);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                BuildError::step(
                    Step::Templates,
                    format!("建目录失败（{}）：{err}", parent.display()),
                )
            })?;
        }
        fs::write(&target, &body).map_err(|err| {
            BuildError::step(
                Step::Templates,
                format!("写 {} 失败：{err}", target.display()),
            )
        })?;
        written.push(template.target.to_string());
    }
    host.log(&format!("模板渲染完成（{} 个文件）", written.len()));
    Ok(written)
}

/// 只渲染一段文本（给单测用；真跑走 [`render`]）。
pub fn render_text(
    source: &str,
    text: &str,
    values: &[(&str, String)],
) -> Result<String, BuildError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(BuildError::step(
                Step::Templates,
                format!("模板里有没闭合的占位符（{source}）：{}", &rest[start..]),
            ));
        };
        let key = &after[..end];
        let value = values
            .iter()
            .find(|(name, _)| *name == key)
            .ok_or_else(|| {
                BuildError::step(
                    Step::Templates,
                    format!("模板占位符没有取值：{{{{{key}}}}}（{source}）"),
                )
            })?;
        out.push_str(&value.1);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// 占位符表（与上游 `build-capsule.mjs` 的 `PLACEHOLDERS()` 同集合）。
pub fn placeholders(request: &BuildRequest, runtime_version: &str) -> Vec<(&'static str, String)> {
    vec![
        ("CAPSULE_NAME", request.name.clone()),
        ("PROFILE", request.profile.clone()),
        ("PORT", request.port.to_string()),
        ("DSH_VERSION", request.dsh_version.clone()),
        ("NODE_VERSION", runtime_version.to_string()),
        ("BUILT_AT", request.built_at.clone()),
    ]
}

/// 统一成 CRLF（`cmd` 对 LF 的容错不可依赖）。
pub fn to_crlf(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    normalized.replace('\n', "\r\n")
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
    fn render_text_substitutes_all_known_placeholders() {
        let values = placeholders(&request(), "v24.22.0");
        let text = "name={{CAPSULE_NAME}} profile={{PROFILE}} port={{PORT}} dsh={{DSH_VERSION}} node={{NODE_VERSION}} at={{BUILT_AT}}";
        let out = render_text("t", text, &values).expect("应能渲染");
        assert!(out.contains("name=0.2.1-rc.3-demo"));
        assert!(out.contains("port=39414"));
        assert!(out.contains("dsh=0.2.1-rc.3"));
        assert!(out.contains("node=v24.22.0"));
        assert!(!out.contains("{{"));
    }

    #[test]
    fn unknown_placeholder_fails_loudly() {
        let values = placeholders(&request(), "v24.22.0");
        let err = render_text("t", "hello {{NOPE}}", &values).unwrap_err();
        assert!(err.to_string().contains("NOPE"), "{err}");
    }

    #[test]
    fn unclosed_placeholder_fails_loudly() {
        let values = placeholders(&request(), "v24.22.0");
        assert!(render_text("t", "hello {{PORT", &values).is_err());
    }

    #[test]
    fn crlf_conversion_is_idempotent() {
        assert_eq!(to_crlf("a\nb"), "a\r\nb");
        assert_eq!(to_crlf("a\r\nb"), "a\r\nb");
    }

    #[test]
    fn every_shipped_template_renders_with_real_values() {
        // 用真实模板跑一遍占位符替换：保证「模板里出现的占位符都有取值」这条不变量成立。
        let values = placeholders(&request(), "v24.22.0");
        for template in TEMPLATES {
            if template.raw {
                continue;
            }
            let text = std::str::from_utf8(template.content).expect("模板应是 UTF-8");
            let rendered = render_text(template.source, text, &values)
                .unwrap_or_else(|err| panic!("{} 渲染失败：{err}", template.source));
            assert!(
                !rendered.contains("{{"),
                "{} 还有未替换占位符",
                template.source
            );
        }
    }

    #[test]
    fn templates_cover_the_six_upstream_files() {
        let targets: Vec<&str> = TEMPLATES.iter().map(|t| t.target).collect();
        assert_eq!(
            targets,
            vec![
                "start.cmd",
                "activate-env.cmd",
                "README.md",
                "scripts/dump.cmd",
                "scripts/doctor.cmd",
                "scripts/tee.mjs"
            ]
        );
    }
}
