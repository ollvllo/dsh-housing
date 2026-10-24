//! 流水线单测：用假 host（记录命令、回放输出）验证六步的顺序、干跑与拒绝面。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::host::{Command, Host, Outcome};
use crate::request::{BuildRequest, Step};
use crate::BuildError;

/// 记录命令 / 日志 / 步骤的假宿主。
#[derive(Default)]
struct FakeHost {
    commands: Mutex<Vec<Command>>,
    logs: Mutex<Vec<String>>,
    steps: Mutex<Vec<Step>>,
    cancelled: AtomicBool,
    /// 命中这些子串的命令返回非 0（模拟失败）。
    failing: Mutex<Vec<String>>,
}

impl FakeHost {
    fn commands(&self) -> Vec<Command> {
        self.commands.lock().unwrap().clone()
    }

    fn logs(&self) -> Vec<String> {
        self.logs.lock().unwrap().clone()
    }

    fn steps(&self) -> Vec<Step> {
        self.steps.lock().unwrap().clone()
    }

    fn fail_on(&self, needle: &str) {
        self.failing.lock().unwrap().push(needle.to_string());
    }
}

impl Host for FakeHost {
    fn run(&self, command: &Command) -> Result<Outcome, BuildError> {
        let matched = self
            .failing
            .lock()
            .unwrap()
            .iter()
            .any(|needle| command.display().contains(needle.as_str()));
        self.commands.lock().unwrap().push(command.clone());
        if matched {
            return Ok(Outcome {
                code: 1,
                stdout: String::new(),
                stderr_tail: "boom".to_string(),
            });
        }
        Ok(Outcome::default())
    }

    fn log(&self, message: &str) {
        self.logs.lock().unwrap().push(message.to_string());
    }

    fn step(&self, step: Step) {
        self.steps.lock().unwrap().push(step);
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("dsh-housing-builder-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建测试目录");
    dir
}

fn request(root: &Path) -> BuildRequest {
    BuildRequest {
        name: "0.2.1-rc.3-demo".to_string(),
        dsh_version: "0.2.1-rc.3".to_string(),
        node_version: "v24.22.0".to_string(),
        profile: "web".to_string(),
        port: 39414,
        root: root.to_path_buf(),
        registry: None,
        overwrite: false,
        force_app: false,
        dry_run: true,
        built_at: "2026-10-24 14:05:00 +08:00".to_string(),
    }
}

#[test]
fn dry_run_walks_all_six_steps_and_writes_nothing() {
    let root = temp_root("dry");
    let host = FakeHost::default();
    let report = crate::run(&request(&root), &host).expect("干跑应成功");

    assert!(report.dry_run);
    assert_eq!(report.capsule_id, "0.2.1-rc.3-demo");
    assert_eq!(report.templates.len(), 6);
    assert_eq!(
        host.steps(),
        vec![
            Step::Skeleton,
            Step::Runtime,
            Step::App,
            Step::Templates,
            Step::Manifest,
            Step::Verify
        ]
    );
    assert!(!report.capsule_root.exists(), "干跑不建目录");
    let logs = host.logs();
    assert!(logs.iter().any(|line| line.contains("[dry] 只打印计划")));
    assert!(logs.iter().any(|line| line.contains("跳过 3 条版本断言")));
    assert!(host.commands().is_empty(), "干跑不跑任何命令");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn non_empty_target_requires_overwrite() {
    let root = temp_root("occupied");
    let capsule = root.join("0.2.1-rc.3-demo");
    fs::create_dir_all(&capsule).expect("建目标");
    fs::write(capsule.join("keep.txt"), "x").expect("写文件");

    let host = FakeHost::default();
    let err = crate::run(&request(&root), &host).unwrap_err();
    assert_eq!(err.step_name(), Some(Step::Skeleton));
    assert!(format!("{err}").contains("覆盖"), "{err}");

    // 勾了覆盖就能过（干跑不写盘）。
    let mut allowed = request(&root);
    allowed.overwrite = true;
    assert!(crate::run(&allowed, &host).is_ok());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn empty_target_directory_is_fine() {
    let root = temp_root("empty");
    fs::create_dir_all(root.join("0.2.1-rc.3-demo")).expect("建空目录");
    let host = FakeHost::default();
    assert!(crate::run(&request(&root), &host).is_ok());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn cancellation_is_reported_as_cancelled_not_failure() {
    let root = temp_root("cancel");
    let host = FakeHost::default();
    host.cancelled.store(true, Ordering::SeqCst);
    let err = crate::run(&request(&root), &host).unwrap_err();
    assert!(err.is_cancelled(), "{err}");
    assert_eq!(err.step_name(), Some(Step::Skeleton));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn invalid_request_fails_before_touching_disk() {
    let root = temp_root("invalid");
    let host = FakeHost::default();
    let mut bad = request(&root);
    bad.profile = "mine".to_string();
    let err = crate::run(&bad, &host).unwrap_err();
    assert_eq!(err.step_name(), None, "参数错误不属于任何一步");
    assert!(!request(&root).capsule_root().exists());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn naming_hint_is_logged_but_does_not_block() {
    let root = temp_root("naming");
    let host = FakeHost::default();
    let mut loose = request(&root);
    loose.name = "demo".to_string();
    assert!(crate::run(&loose, &host).is_ok());
    assert!(host.logs().iter().any(|line| line.contains("命名")));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn runtime_step_failure_stops_the_pipeline() {
    let root = temp_root("runtime-fail");
    let host = FakeHost::default();
    let mut real = request(&root);
    real.dry_run = false;
    // 让 curl 下载失败：node.zip 不存在且镜像不可达 → 我们只模拟命令失败。
    host.fail_on("curl.exe");
    let err = crate::run(&real, &host).unwrap_err();
    assert_eq!(err.step_name(), Some(Step::Runtime));
    // 骨架已经建出来了（真跑），失败不能把目录删掉（留给用户重试）。
    assert!(real.capsule_root().exists());
    let _ = fs::remove_dir_all(&root);
}
