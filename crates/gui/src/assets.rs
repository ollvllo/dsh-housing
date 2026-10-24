//! 内嵌前端资源（docs/structure.md §2.5 / §5.8 D2 / §3.23）。
//!
//! `crates/gui/web/` 是整个控制台（index.html / app.js / styles.css / vendor/），
//! 由 `include_dir!` 在**编译期**编进 exe；运行时不需要 node、不需要外部文件。
//!
//! 前端是**无构建 Vue 3**：`vendor/vue.esm-browser.prod.js` 是 Vue 3 的官方
//! 浏览器构建（MIT，许可证副本见 `web/vendor/LICENSE-vue.txt`），页面模板写在
//! `index.html` 里，`app.js` 只用 `createApp` 组装数据与方法。
//!
//! `favicon.ico` 不放在 `web/` 里：它就是仓库根 `assets/favicon.ico`（exe 资源 `1 ICON`
//! 与托盘图标用的同一份），这里用 `include_bytes!` 编进来，避免同一张图存在两份（#271）。

use include_dir::{include_dir, Dir};

/// 编译期嵌入的整个 `web/` 目录。
pub static WEB: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/web");

/// 控制台 favicon：与 exe / 托盘图标**同一个文件**（v0.4 / #271）。
pub static FAVICON: &[u8] = include_bytes!("../../../assets/favicon.ico");

/// 按字面路径取文件（`""` / `"/"` 不算文件，由调用方映射到 `index.html`）。
pub fn get(path: &str) -> Option<&'static [u8]> {
    WEB.get_file(path).map(|file| file.contents())
}

/// 静态资源的 MIME（未知扩展名按二进制流处理，不让浏览器猜）。
pub fn content_type(path: &str) -> &'static str {
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_the_console_entry_points() {
        for name in ["index.html", "app.js", "styles.css"] {
            assert!(get(name).is_some(), "web/{name} 应被编进 exe");
        }
        assert!(
            get("vendor/vue.esm-browser.prod.js").is_some(),
            "Vue 的浏览器构建应随仓库提交"
        );
        assert!(get("nope.html").is_none());
    }

    #[test]
    fn content_types_follow_extension() {
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(
            content_type("vendor/vue.esm-browser.prod.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("styles.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("LICENSE-vue.txt"), "text/plain; charset=utf-8");
        assert_eq!(content_type("favicon.ico"), "image/x-icon");
        assert_eq!(content_type("no-extension"), "application/octet-stream");
    }

    #[test]
    fn favicon_is_the_shared_assets_icon() {
        // v0.4 / #271：控制台图标必须就是 exe / 托盘用的那一份，不是复制品。
        let on_disk = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/favicon.ico"
        ))
        .expect("assets/favicon.ico 应随仓库存在");
        assert_eq!(
            FAVICON,
            on_disk.as_slice(),
            "编进 exe 的 favicon 与 assets/favicon.ico 必须逐字节一致"
        );
        // ICO 头：reserved=0 / type=1（icon）——顺带防「其实是个 png 改了后缀」。
        assert_eq!(&FAVICON[..4], &[0, 0, 1, 0], "favicon.ico 的 ICO 魔数");
    }
}
