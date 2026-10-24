//! `.dsh-housing\` 数据目录的定位与初始化（docs/structure.md §2.4）。
//!
//! 三条硬性前提（写进代码，不只是文档）：
//!
//! 1. 数据目录相对 **exe 自身路径** 定位（`GetModuleFileNameW` 的 Rust 封装
//!    `std::env::current_exe`），**不依赖 cwd** —— 任务计划拉起时 cwd 可能是 `System32`；
//! 2. 启动时做写探针，不可写就明确报错退出，**不静默回退**；
//! 3. 两份 json 只在这里创建：`capsules.json`、`settings.json`；实例级 `env` 随前者走。
//!
//! M2 才会往里写真实内容（胶囊登记与实例记录）；M0 只保证「目录在、文件在、格式对」，
//! 且**已存在的文件一律不覆盖**。

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use dsh_housing_webhook::WebhookFile;

use crate::capsule::CapsulesFile;
use crate::model::{Settings, SCHEMA_VERSION};

/// 数据目录名（exe 同级）。
pub const DATA_DIR_NAME: &str = ".dsh-housing";
/// 日志子目录名。
pub const LOGS_DIR_NAME: &str = "logs";
/// 胶囊清单文件名。
pub const CAPSULES_FILE_NAME: &str = "capsules.json";
/// 壳设置文件名。
pub const SETTINGS_FILE_NAME: &str = "settings.json";
/// webhook 配置文件名（v0.2 / #244；与 `settings.json` 分开，各自只有一个写者，§5.11 D23）。
pub const WEBHOOKS_FILE_NAME: &str = "webhooks.json";
/// 写探针文件名（建完即删）。
const WRITE_PROBE_FILE_NAME: &str = ".write-probe";
/// 原子替换用的临时后缀。
const TEMP_SUFFIX: &str = ".tmp";

/// 初始化结果，供 `diag` 记录与测试断言。
#[derive(Debug, Clone)]
pub struct BootstrapReport {
    /// 启动器自己的 exe 路径。
    pub exe_path: PathBuf,
    /// `.dsh-housing\` 的绝对路径。
    pub data_dir: PathBuf,
    /// 本次真正新建的文件 / 目录（已存在的不列出，重复启动时为空）。
    pub created: Vec<String>,
}

/// 数据目录相关的失败。`context` 说明卡在哪一步，`detail` / `source` 是原始错误。
#[derive(Debug)]
pub struct BootstrapError {
    context: &'static str,
    detail: String,
    source: Option<io::Error>,
}

impl BootstrapError {
    fn new(context: &'static str, source: io::Error) -> Self {
        Self {
            context,
            detail: source.to_string(),
            source: Some(source),
        }
    }

    fn message(context: &'static str, detail: impl Into<String>) -> Self {
        Self {
            context,
            detail: detail.into(),
            source: None,
        }
    }

    /// 卡住的那一步，供调用方按需展示。
    pub fn context(&self) -> &'static str {
        self.context
    }
}

impl fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.context, self.detail)
    }
}

impl std::error::Error for BootstrapError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_ref().map(|err| err as _)
    }
}

/// 启动器自己的 exe 路径。
pub fn exe_path() -> Result<PathBuf, BootstrapError> {
    std::env::current_exe().map_err(|err| BootstrapError::new("解析 exe 自身路径", err))
}

/// 数据目录 = `<exe 所在目录>\.dsh-housing\`。
pub fn resolve_data_dir() -> Result<PathBuf, BootstrapError> {
    data_dir_for_exe(&exe_path()?)
}

/// 数据目录的纯计算部分（不做任何 IO，便于测试）。
pub fn data_dir_for_exe(exe: &Path) -> Result<PathBuf, BootstrapError> {
    match exe.parent() {
        Some(dir) => Ok(dir.join(DATA_DIR_NAME)),
        None => Err(BootstrapError::message(
            "解析 exe 自身路径",
            format!("{} 没有父目录", exe.display()),
        )),
    }
}

/// 生产入口：定位 exe → 自建并校验数据目录。
pub fn bootstrap() -> Result<BootstrapReport, BootstrapError> {
    let exe_path = exe_path()?;
    let data_dir = data_dir_for_exe(&exe_path)?;
    let created = prepare_data_dir(&data_dir)?;
    Ok(BootstrapReport {
        exe_path,
        data_dir,
        created,
    })
}

/// 读 `settings.json`。
///
/// 文件不存在 → 默认值（`bootstrap` 已经会建）；JSON 坏了 → 返回错误，由调用方
/// **告警后按默认值继续**——一个可手改的设置文件不该让启动器起不来。
pub fn load_settings(data_dir: &Path) -> Result<Settings, BootstrapError> {
    let path = data_dir.join(SETTINGS_FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Settings::default()),
        Err(err) => {
            return Err(BootstrapError::message(
                "读取 settings.json",
                format!("{}：{}", path.display(), err),
            ))
        }
    };
    serde_json::from_str(&text).map_err(|err| {
        BootstrapError::message("解析 settings.json", format!("{}：{}", path.display(), err))
    })
}

/// 在指定目录上完成 M0 的全部落盘动作，返回本次新建的条目。
///
/// 幂等：目录与 json 已存在时原样保留（不覆盖用户数据），`created` 为空。
/// 生产路径请走 `bootstrap`；本函数给测试与将来的「换数据目录」入口用。
pub fn prepare_data_dir(data_dir: &Path) -> Result<Vec<String>, BootstrapError> {
    let mut created = Vec::new();

    if !data_dir.exists() {
        fs::create_dir_all(data_dir).map_err(|err| BootstrapError::new("创建数据目录", err))?;
        created.push(DATA_DIR_NAME.to_string());
    }

    let logs_dir = data_dir.join(LOGS_DIR_NAME);
    if !logs_dir.exists() {
        fs::create_dir_all(&logs_dir).map_err(|err| BootstrapError::new("创建日志目录", err))?;
        created.push(format!("{DATA_DIR_NAME}\\{LOGS_DIR_NAME}"));
    }

    probe_writable(data_dir)?;

    let capsules = default_capsules_json();
    let capsules_text = to_pretty_json(&capsules, "capsules.json 默认内容")?;
    ensure_file(
        &data_dir.join(CAPSULES_FILE_NAME),
        &capsules_text,
        CAPSULES_FILE_NAME,
        &mut created,
    )?;

    let settings_text = to_pretty_json(&Settings::default(), "settings.json 默认内容")?;
    ensure_file(
        &data_dir.join(SETTINGS_FILE_NAME),
        &settings_text,
        SETTINGS_FILE_NAME,
        &mut created,
    )?;

    let webhooks_text = to_pretty_json(&WebhookFile::default(), "webhooks.json 默认内容")?;
    ensure_file(
        &data_dir.join(WEBHOOKS_FILE_NAME),
        &webhooks_text,
        WEBHOOKS_FILE_NAME,
        &mut created,
    )?;

    Ok(created)
}

/// `capsules.json` 的初始内容。
///
/// M0 只盖版本号与空表；条目 schema 由 `capsule.rs` 定型（M2，见 docs/structure.md §2.4）。
fn default_capsules_json() -> serde_json::Value {
    serde_json::json!({
        "version": SCHEMA_VERSION,
        "capsules": [],
    })
}

/// 读 `capsules.json`。文件不存在 → 空表；解析失败 → 返回错误（由调用方决定
/// 「告警 + 空表继续，首次写盘前备份坏文件」——见 `docs/structure.md` §2.4 硬规则 2）。
pub fn load_capsules(data_dir: &Path) -> Result<CapsulesFile, BootstrapError> {
    let path = data_dir.join(CAPSULES_FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(CapsulesFile::default()),
        Err(err) => {
            return Err(BootstrapError::message(
                "读取 capsules.json",
                format!("{}：{}", path.display(), err),
            ))
        }
    };
    serde_json::from_str(&text).map_err(|err| {
        BootstrapError::message("解析 capsules.json", format!("{}：{}", path.display(), err))
    })
}

/// 写 `capsules.json`（临时文件 + 改名，原子替换）。
pub fn save_capsules(data_dir: &Path, file: &CapsulesFile) -> Result<(), BootstrapError> {
    let path = data_dir.join(CAPSULES_FILE_NAME);
    let text = to_pretty_json(file, "capsules.json 内容")?;
    write_atomic(&path, &text).map_err(|err| {
        BootstrapError::message("写入 capsules.json", format!("{}：{}", path.display(), err))
    })
}

/// 写 `settings.json`（M4 控制台 `PUT /api/settings` 的唯一落盘入口）。
///
/// 与 `capsules.json` 同一套「临时文件 + 改名」：写一半断电也不会留下半份 json。
pub fn save_settings(data_dir: &Path, settings: &Settings) -> Result<(), BootstrapError> {
    let path = data_dir.join(SETTINGS_FILE_NAME);
    let text = to_pretty_json(settings, "settings.json 内容")?;
    write_atomic(&path, &text).map_err(|err| {
        BootstrapError::message("写入 settings.json", format!("{}：{}", path.display(), err))
    })
}

/// 任务计划 XML 备份目录（`autostart.rs` 把旧链路任务的定义存这里）。
pub const BACKUP_DIR_NAME: &str = "backup";

/// 读 `webhooks.json`。文件不存在 → 空表；解析失败 → 返回错误（由调用方
/// 「告警 + 空表继续，首次写盘前备份坏文件」，与 `capsules.json` 同一套，§2.4）。
pub fn load_webhooks(data_dir: &Path) -> Result<WebhookFile, BootstrapError> {
    let path = data_dir.join(WEBHOOKS_FILE_NAME);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(WebhookFile::default()),
        Err(err) => {
            return Err(BootstrapError::message(
                "读取 webhooks.json",
                format!("{}：{}", path.display(), err),
            ))
        }
    };
    serde_json::from_str(&text).map_err(|err| {
        BootstrapError::message("解析 webhooks.json", format!("{}：{}", path.display(), err))
    })
}

/// 写 `webhooks.json`（`PUT /api/webhooks` 的唯一落盘入口，原子替换）。
pub fn save_webhooks(data_dir: &Path, file: &WebhookFile) -> Result<(), BootstrapError> {
    let path = data_dir.join(WEBHOOKS_FILE_NAME);
    let text = to_pretty_json(file, "webhooks.json 内容")?;
    write_atomic(&path, &text).map_err(|err| {
        BootstrapError::message("写入 webhooks.json", format!("{}：{}", path.display(), err))
    })
}

/// 把坏掉的 `webhooks.json` 改名备份成 `webhooks.json.broken-<unix秒>`。
pub fn backup_broken_webhooks(data_dir: &Path) -> Result<Option<PathBuf>, BootstrapError> {
    backup_broken_file(data_dir, WEBHOOKS_FILE_NAME)
}

/// 把坏掉的 `capsules.json` 改名备份成 `capsules.json.broken-<unix秒>`。
///
/// 只应该在「解析失败 + 即将用新内容覆盖」时调用；源文件不存在时返回 `Ok(None)`（
/// 此时没什么可备份的）。改名失败返回错误，调用方应放弃写盘（宁可这次登记失败，
/// 也不覆盖用户手改坏的数据）。
pub fn backup_broken_capsules(data_dir: &Path) -> Result<Option<PathBuf>, BootstrapError> {
    backup_broken_file(data_dir, CAPSULES_FILE_NAME)
}

/// 「坏文件先改名备份」的通用实现（`capsules.json` / `webhooks.json` 共用）。
pub fn backup_broken_file(
    data_dir: &Path,
    file_name: &str,
) -> Result<Option<PathBuf>, BootstrapError> {
    let path = data_dir.join(file_name);
    if !path.exists() {
        return Ok(None);
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let backup = data_dir.join(format!("{file_name}.broken-{stamp}"));
    fs::rename(&path, &backup).map_err(|err| {
        BootstrapError::message(
            "备份损坏的配置文件",
            format!("{} → {}：{}", path.display(), backup.display(), err),
        )
    })?;
    Ok(Some(backup))
}

fn to_pretty_json<T: serde::Serialize>(
    value: &T,
    context: &'static str,
) -> Result<String, BootstrapError> {
    let mut text = serde_json::to_string_pretty(value)
        .map_err(|err| BootstrapError::message(context, err.to_string()))?;
    text.push('\n');
    Ok(text)
}

/// 文件不存在才写；写入走「临时文件 + 改名为正式文件」。
fn ensure_file(
    path: &Path,
    contents: &str,
    label: &str,
    created: &mut Vec<String>,
) -> Result<(), BootstrapError> {
    if path.exists() {
        return Ok(());
    }
    write_atomic(path, contents).map_err(|err| {
        BootstrapError::message("写入默认文件", format!("{}：{}", path.display(), err))
    })?;
    created.push(label.to_string());
    Ok(())
}

fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径没有文件名"))?;
    let temp_path = path.with_file_name(format!("{}{}", file_name.to_string_lossy(), TEMP_SUFFIX));

    let result = (|| -> io::Result<()> {
        let mut file = fs::File::create(&temp_path)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // Windows 上 fs::rename 走 MoveFileEx(MOVEFILE_REPLACE_EXISTING)，可覆盖同名文件。
        fs::rename(&temp_path, path)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

/// 写探针：建一个临时文件再删掉，证明「这个目录真的能写」。
///
/// 删不掉不算致命（只留一个可忽略的小文件），所以删除失败被忽略。
fn probe_writable(data_dir: &Path) -> Result<(), BootstrapError> {
    let probe_path = data_dir.join(WRITE_PROBE_FILE_NAME);
    fs::File::create(&probe_path)
        .and_then(|mut file| file.write_all(b"dsh-housing write probe\n"))
        .map_err(|err| BootstrapError::new("数据目录写探针", err))?;
    let _ = fs::remove_file(&probe_path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// 测试目录放在工作区的 `target/test-tmp/` 下，**不用** `std::env::temp_dir()`：
    /// DSH 沙箱会给每个会话一个私有 TEMP（如 `…\cache\tmp\dsh-XXXXXX`），其 ACL 只授权
    /// 受限令牌的 SID，去权运行（升权后）反而写不进去；而工作区在任何模式下都可写。
    fn test_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp")
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let root = test_root();
        fs::create_dir_all(&root).expect("创建测试根目录");
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("系统时间早于 UNIX 纪元")
            .as_nanos();
        root.join(format!("{label}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn data_dir_sits_next_to_exe() {
        let dir = data_dir_for_exe(Path::new(r"D:\some\where\dsh-housing.exe")).unwrap();
        assert_eq!(dir, Path::new(r"D:\some\where").join(DATA_DIR_NAME));
    }

    #[test]
    fn prepare_creates_defaults_once() {
        let dir = unique_temp_dir("store");
        let created = prepare_data_dir(&dir).expect("首次初始化应当成功");
        assert!(created.iter().any(|item| item == CAPSULES_FILE_NAME));
        assert!(created.iter().any(|item| item == SETTINGS_FILE_NAME));
        assert!(created.iter().any(|item| item == WEBHOOKS_FILE_NAME));
        assert!(dir.join(LOGS_DIR_NAME).is_dir());

        let capsules: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(CAPSULES_FILE_NAME)).unwrap())
                .expect("capsules.json 应是合法 JSON");
        assert_eq!(capsules["version"], SCHEMA_VERSION);

        let settings: Settings =
            serde_json::from_str(&fs::read_to_string(dir.join(SETTINGS_FILE_NAME)).unwrap())
                .expect("settings.json 应是合法 JSON");
        assert_eq!(settings, Settings::default());

        let webhooks: WebhookFile =
            serde_json::from_str(&fs::read_to_string(dir.join(WEBHOOKS_FILE_NAME)).unwrap())
                .expect("webhooks.json 应是合法 JSON");
        assert_eq!(webhooks, WebhookFile::default());

        // 幂等 + 不覆盖：第二次不再新建任何条目，内容保持原样。
        fs::write(dir.join(SETTINGS_FILE_NAME), "{\"version\": 1}").unwrap();
        let created_again = prepare_data_dir(&dir).expect("重复初始化应当成功");
        assert!(created_again.is_empty(), "重复初始化不应再新建条目");
        assert_eq!(
            fs::read_to_string(dir.join(SETTINGS_FILE_NAME)).unwrap(),
            "{\"version\": 1}"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unwritable_parent_is_reported_not_swallowed() {
        // 用一个「父路径是文件」的目录来制造必然的创建失败。
        let blocker = unique_temp_dir("blocker");
        fs::write(&blocker, b"not a directory").unwrap();
        let err = prepare_data_dir(&blocker.join("child")).expect_err("必须报错而不是静默回退");
        assert_eq!(err.context(), "创建数据目录");
        fs::remove_file(&blocker).unwrap();
    }

    #[test]
    fn save_settings_round_trips_and_replaces_atomically() {
        let dir = unique_temp_dir("settings");
        prepare_data_dir(&dir).expect("初始化数据目录");

        let settings = Settings {
            sample_interval_ms: 2_000,
            auto_open_browser: true,
            log_retention_days: 3,
            ..Settings::default()
        };
        save_settings(&dir, &settings).expect("写 settings.json");

        let loaded = load_settings(&dir).expect("读回 settings.json");
        assert_eq!(loaded, settings, "写进去什么就该读回什么");

        // 原子替换：不留临时文件。
        let leftovers = fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "临时文件必须被改名，不该留在数据目录里");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn webhooks_round_trip_and_broken_file_gets_backed_up() {
        let dir = unique_temp_dir("webhooks");
        prepare_data_dir(&dir).expect("初始化数据目录");

        let mut file = WebhookFile::default();
        file.webhooks.push(dsh_housing_webhook::Webhook {
            url: "https://ntfy.example.test/_dsh".to_string(),
            ..Default::default()
        });
        save_webhooks(&dir, &file).expect("写 webhooks.json");
        assert_eq!(load_webhooks(&dir).expect("读回 webhooks.json"), file);

        // 手改坏的文件：解析失败要能被调用方发现，且备份不会被静默覆盖。
        fs::write(dir.join(WEBHOOKS_FILE_NAME), "{ not json").unwrap();
        let error = load_webhooks(&dir).expect_err("坏文件必须报错而不是当空表");
        assert_eq!(error.context(), "解析 webhooks.json");

        let backup = backup_broken_webhooks(&dir)
            .expect("备份坏文件")
            .expect("源文件存在时应当有备份路径");
        assert!(backup.exists());
        assert!(backup
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("webhooks.json.broken-"));
        assert!(!dir.join(WEBHOOKS_FILE_NAME).exists(), "坏文件已被改名");
        assert_eq!(
            backup_broken_webhooks(&dir).expect("没有源文件时不算错"),
            None
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
