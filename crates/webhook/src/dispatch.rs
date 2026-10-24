//! 投递（docs/structure.md §3.21）：有界队列 + 一个后台投递线程。
//!
//! 为什么要有队列而不是就地发：事件是从 core 的 supervisor（250 ms 一轮）与停止线程里
//! 抛出来的，**任何持锁路径都不等待进程，更不能等待网络**（§2.2.1 的锁纪律）。所以
//! `submit` 只做一次 `try_send`，真正的 HTTP 在投递线程里串行发生。
//!
//! 串行、单次超时 5 s、传输层失败重试 1 次：投递慢只会让后面的通知晚一点，不会拖住 core；
//! 端点长期不可达时队列最多堆 [`QUEUE_CAPACITY`] 条，满了丢新事件并记一行告警。

use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::{endpoint_label, Webhook};
use crate::http::{self, Timeouts};
use crate::template::Event;

/// 队列上限（每条事件最多 16 个请求，所以这里的数字是「事件」不是「请求」）。
pub const QUEUE_CAPACITY: usize = 256;

/// 传输层失败（连不上 / 超时 / 连接被断）时的重试次数（不含首次）。
const RETRIES: usize = 1;

/// 重试前的等待。
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// 投递线程的日志出口。crate 不依赖 core 的 `diag`，由调用方注入实现。
pub trait Sink: Send + Sync {
    fn info(&self, message: &str);
    fn warn(&self, message: &str);
}

enum Job {
    Event(Event),
    /// 屏障：投递线程处理到它时说明前面的事件都处理完了（core 退出前用）。
    Flush(SyncSender<()>),
}

/// 投递器：配置可以热换（`apply`），事件进来只排队。
pub struct Dispatcher {
    queue: SyncSender<Job>,
    sink: Arc<dyn Sink>,
    /// 当前配置；`Arc` 快照让投递线程不必长时间持锁。
    config: Arc<Mutex<Arc<Vec<Webhook>>>>,
    /// 投递线程句柄（起不来时为 `None`，此时事件会被丢弃并记告警）。
    worker: Mutex<Option<thread::JoinHandle<()>>>,
}

impl Dispatcher {
    /// 起投递线程。
    pub fn start(sink: Arc<dyn Sink>, webhooks: Vec<Webhook>) -> Arc<Self> {
        let (queue, receiver) = sync_channel(QUEUE_CAPACITY);
        let config = Arc::new(Mutex::new(Arc::new(webhooks)));
        let sink_for_worker = sink.clone();
        let config_for_worker = config.clone();
        let worker = thread::Builder::new()
            .name("webhook-delivery".to_string())
            .spawn(move || {
                worker_loop(receiver, config_for_worker, sink_for_worker);
            });
        let worker = match worker {
            Ok(handle) => Some(handle),
            Err(err) => {
                sink.warn(&format!(
                    "webhook：投递线程起不来（{err}）；之后的事件不会发送"
                ));
                None
            }
        };
        Arc::new(Self {
            queue,
            sink,
            config,
            worker: Mutex::new(worker),
        })
    }

    /// 热换配置（下一次事件就用新的；正在发的那条不受影响）。
    pub fn apply(&self, webhooks: Vec<Webhook>) {
        let mut current = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *current = Arc::new(webhooks);
    }

    /// 排一条事件（不阻塞、不等网络）。
    pub fn submit(&self, event: Event) {
        let label = event.label();
        match self.queue.try_send(Job::Event(event)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => self.sink.warn(&format!(
                "webhook：队列已满（{QUEUE_CAPACITY} 条），丢弃 {label} 事件（端点可能长期不可达）"
            )),
            Err(TrySendError::Disconnected(_)) => self
                .sink
                .warn(&format!("webhook：投递线程已退出，丢弃 {label} 事件")),
        }
    }

    /// 等队列排空（core 退出前用；有界，不无限等）。
    ///
    /// 返回 `true` = 该时刻之前排进去的事件都处理完了；`false` = 预算用完（剩余事件丢弃，
    /// 进程马上要退出，不再等）。
    pub fn drain(&self, budget: Duration) -> bool {
        let deadline = Instant::now() + budget;
        let (ack_tx, ack_rx) = sync_channel(1);
        loop {
            match self.queue.try_send(Job::Flush(ack_tx.clone())) {
                Ok(()) => break,
                Err(TrySendError::Full(_)) => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(TrySendError::Disconnected(_)) => return true,
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ack_rx.recv_timeout(remaining).is_ok()
    }

    /// 投递线程是否还活着（验收脚本 / 诊断用）。
    pub fn is_running(&self) -> bool {
        let mut worker = self
            .worker
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match worker.as_mut() {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }
}

fn worker_loop(
    receiver: Receiver<Job>,
    config: Arc<Mutex<Arc<Vec<Webhook>>>>,
    sink: Arc<dyn Sink>,
) {
    while let Ok(job) = receiver.recv() {
        match job {
            Job::Flush(ack) => {
                let _ = ack.send(());
            }
            Job::Event(event) => deliver(&event, &config, &sink),
        }
    }
}

/// 一条事件 → 按当前配置逐条 webhook 投递（串行）。
fn deliver(event: &Event, config: &Mutex<Arc<Vec<Webhook>>>, sink: &Arc<dyn Sink>) {
    let hooks = config
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if hooks.is_empty() {
        return;
    }
    for hook in hooks.iter() {
        let Some(template) = hook.template_for(event.trigger) else {
            continue;
        };
        let body = event.render(template);
        let label = endpoint_label(&hook.url);
        let mut attempt = 0;
        loop {
            match http::post_text(&hook.url, &body, Timeouts::default()) {
                Ok(outcome) if outcome.is_success() => {
                    sink.info(&format!(
                        "webhook：{} → {label} 已发送（HTTP {}，{} 字节）",
                        event.label(),
                        outcome.status,
                        body.len()
                    ));
                    break;
                }
                Ok(outcome) => {
                    // HTTP 层通了但端点不接受：重试通常没用，记一行就够了。
                    sink.warn(&format!(
                        "webhook：{} → {label} 返回 HTTP {}（非 2xx，不重试）",
                        event.label(),
                        outcome.status
                    ));
                    break;
                }
                Err(err) => {
                    if attempt < RETRIES {
                        attempt += 1;
                        sink.warn(&format!(
                            "webhook：{} → {label} 投递失败（{err}），{} ms 后重试",
                            event.label(),
                            RETRY_BACKOFF.as_millis()
                        ));
                        thread::sleep(RETRY_BACKOFF);
                        continue;
                    }
                    sink.warn(&format!(
                        "webhook：{} → {label} 投递失败（{err}），已放弃",
                        event.label()
                    ));
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use crate::config::{Trigger, Webhook};

    /// 记录日志的假 Sink（断言「失败被记下来」用）。
    #[derive(Default)]
    struct RecordingSink {
        info: Mutex<Vec<String>>,
        warn: Mutex<Vec<String>>,
    }

    impl Sink for RecordingSink {
        fn info(&self, message: &str) {
            self.info.lock().unwrap().push(message.to_string());
        }
        fn warn(&self, message: &str) {
            self.warn.lock().unwrap().push(message.to_string());
        }
    }

    /// 本地假 webhook 端点：循环接受连接，把每个请求的原文收集起来。
    ///
    /// `close_first` = 第一个连接读完请求后直接断开（用来测传输层重试）。
    struct FakeEndpoint {
        port: u16,
        requests: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        handle: Option<thread::JoinHandle<()>>,
    }

    impl FakeEndpoint {
        fn start(close_first: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("绑定假端点");
            listener.set_nonblocking(true).expect("非阻塞");
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let seen = requests.clone();
            let stop = Arc::new(AtomicBool::new(false));
            let stopping = stop.clone();
            let accepted = Arc::new(AtomicUsize::new(0));
            let counter = accepted.clone();
            let handle = thread::spawn(move || {
                while !stopping.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let index = counter.fetch_add(1, Ordering::Relaxed);
                            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();
                            if let Some(text) = read_request(&mut stream) {
                                seen.lock().unwrap().push(text);
                            }
                            if close_first && index == 0 {
                                let _ = stream.shutdown(Shutdown::Both);
                                continue;
                            }
                            let _ = stream.write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
                            );
                            let _ = stream.flush();
                            let _ = stream.shutdown(Shutdown::Both);
                        }
                        Err(ref err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => break,
                    }
                }
            });
            Self {
                port,
                requests,
                stop,
                handle: Some(handle),
            }
        }

        fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }

        fn request_count(&self) -> usize {
            self.requests.lock().unwrap().len()
        }

        fn last_request(&self) -> String {
            self.requests
                .lock()
                .unwrap()
                .last()
                .cloned()
                .unwrap_or_default()
        }
    }

    impl Drop for FakeEndpoint {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn read_request(stream: &mut std::net::TcpStream) -> Option<String> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
            if let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if buffer.len() >= end + 4 + length {
                    break;
                }
            }
        }
        (!buffer.is_empty()).then(|| String::from_utf8_lossy(&buffer).into_owned())
    }

    fn event(trigger: Trigger) -> Event {
        Event {
            trigger,
            time: "2026-10-20 16:40:00".to_string(),
            alias: "Deepseek Harness 0.2.0-rc.2 web".to_string(),
            dsh_version: "0.2.0-rc.2".to_string(),
            profile: "web".to_string(),
            local_url: Some("http://127.0.0.1:39412/?token=<token>".to_string()),
            lan_url: None,
        }
    }

    #[test]
    fn event_reaches_every_enabled_hook_once() {
        let first = FakeEndpoint::start(false);
        let second = FakeEndpoint::start(false);
        let sink = Arc::new(RecordingSink::default());
        let dispatcher = Dispatcher::start(
            sink.clone(),
            vec![
                Webhook {
                    url: first.url("/hook-a"),
                    ..Webhook::default()
                },
                Webhook {
                    url: second.url("/hook-b"),
                    ..Webhook::default()
                },
            ],
        );

        dispatcher.submit(event(Trigger::Start));
        assert!(dispatcher.drain(Duration::from_secs(10)), "队列应当排空");

        assert_eq!(first.request_count(), 1);
        assert_eq!(second.request_count(), 1);
        let body = first.last_request();
        assert!(body.contains("Service Start"), "{body}");
        assert!(
            body.contains("Name: Deepseek Harness 0.2.0-rc.2 web"),
            "{body}"
        );
        assert!(
            body.contains("http://127.0.0.1:39412/?token=<token>"),
            "{body}"
        );
        assert!(
            sink.info
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("已发送")),
            "{:?}",
            sink.info.lock().unwrap()
        );
    }

    #[test]
    fn disabled_hook_and_disabled_trigger_send_nothing() {
        let never = FakeEndpoint::start(false);
        let only_start = FakeEndpoint::start(false);
        let sink = Arc::new(RecordingSink::default());
        let mut quiet = Webhook {
            url: never.url("/quiet"),
            ..Webhook::default()
        };
        quiet.enabled = false;
        let mut start_only = Webhook {
            url: only_start.url("/start-only"),
            ..Webhook::default()
        };
        start_only.stop.enabled = false;
        start_only.crashed.enabled = false;
        let dispatcher = Dispatcher::start(sink, vec![quiet, start_only]);

        dispatcher.submit(event(Trigger::Stop));
        assert!(dispatcher.drain(Duration::from_secs(10)));
        assert_eq!(never.request_count(), 0, "整条关掉就不该发");
        assert_eq!(only_start.request_count(), 0, "条件没勾就不该发");

        dispatcher.submit(event(Trigger::Start));
        assert!(dispatcher.drain(Duration::from_secs(10)));
        assert_eq!(only_start.request_count(), 1);
    }

    #[test]
    fn apply_swaps_the_config_for_the_next_event() {
        let endpoint = FakeEndpoint::start(false);
        let sink = Arc::new(RecordingSink::default());
        let dispatcher = Dispatcher::start(sink, Vec::new());

        dispatcher.submit(event(Trigger::Start));
        assert!(dispatcher.drain(Duration::from_secs(10)));
        assert_eq!(endpoint.request_count(), 0, "没有配置时什么都不发");

        dispatcher.apply(vec![Webhook {
            url: endpoint.url("/after-apply"),
            ..Webhook::default()
        }]);
        dispatcher.submit(event(Trigger::Start));
        assert!(dispatcher.drain(Duration::from_secs(10)));
        assert_eq!(endpoint.request_count(), 1, "热换配置后立刻生效");
    }

    #[test]
    fn transport_failure_is_retried_once() {
        // 第一个连接读完请求就断（模拟端点抖动），第二个连接正常回 200。
        let endpoint = FakeEndpoint::start(true);
        let sink = Arc::new(RecordingSink::default());
        let dispatcher = Dispatcher::start(
            sink.clone(),
            vec![Webhook {
                url: endpoint.url("/flaky"),
                ..Webhook::default()
            }],
        );

        dispatcher.submit(event(Trigger::Crashed));
        assert!(dispatcher.drain(Duration::from_secs(20)), "重试后应当排空");
        assert_eq!(endpoint.request_count(), 2, "应当重试一次");
        assert!(
            sink.warn
                .lock()
                .unwrap()
                .iter()
                .any(|line| line.contains("重试")),
            "{:?}",
            sink.warn.lock().unwrap()
        );
    }
}
