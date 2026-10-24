//! 构建脚本：把 `assets/favicon.ico` 编进 exe 资源（资源 ID = 1）。
//!
//! 一个资产同时供两处使用：**exe 图标**（资源管理器 / alt-tab）与**托盘图标**
//! （`crates/tray` 用 `MAKEINTRESOURCEW(1)` 取）。换图标只改 `assets/favicon.ico`。
//!
//! 资源只有 Windows 需要；其它平台跳过，保证 `cargo build` 在别处也能过
//! （本项目只发 Windows，这里只是不给跨平台构建添堵）。

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=../../assets/favicon.ico");

    #[cfg(windows)]
    embed_resource::compile("app.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("把 assets/favicon.ico 编进 exe 资源失败（需要 Windows SDK 的 rc.exe）");
}
