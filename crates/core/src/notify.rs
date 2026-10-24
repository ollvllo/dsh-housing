//! webhook 事件源（docs/structure.md §3.21 / §5.11 D21）。
//!
//! 内核这边只有一层**薄胶水**：什么时候算「起来了 / 停了 / 崩了」是 `instance.rs` /
//! `runtime.rs` 的状态机说了算，投递（配置 / 模板 / HTTP / 重试 / 队列）在 `crates/webhook`。
//! 这里只做三件事：
//!
//! 1. 把实例的事实（有效显示名 / dsh 版本 / profile / token URL）取成 [`Facts`]；
//! 2. `{lan_url}`：探测本机局域网地址是否真能连上该端口——**探到才给值，探不到留空**
//!    （web profile 只绑 `127.0.0.1` 时必然为空，这与 §3.2「不展示 host」同一口径）；
//! 3. 把 [`Event`] 交给投递线程（`submit` 不阻塞、不碰网络）。
//!
//! 局域网地址的解析（`COMPUTERNAME` → 本机 IPv4）放在后台线程里做并缓存 5 分钟，
//! supervisor 的 250 ms 轮询**不会**因为一次 DNS 解析卡住。

use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use dsh_housing_webhook::{template_uses, Dispatcher, Event, Sink, Trigger, Webhook};

use crate::diag;
use crate::net;

/// 本机局域网地址的缓存时间（DHCP 换了地址最多延迟这么久才反映到 `{lan_url}`）。
const LAN_CACHE_TTL: Duration = Duration::from_secs(300);
/// 单个候选地址的 TCP 连通性探测超时。回环/拒连通常立刻返回，只有不可达的地址才等满。
const LAN_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

/// 一次事件要带的实例事实（在事件发生那一刻取好，投递线程不再回读 core 状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    /// 有效显示名（用户别名，没设过时是派生的 `Deepseek Harness <version> <profile>`，§3.20）。
    pub alias: String,
    /// 胶囊登记里的 dsh 版本（`app.version`）；缺登记信息时退回 `capsuleId`。
    pub dsh_version: String,
    /// profile 名。
    pub profile: String,
    /// 实例端口（拿不到 token URL 里端口时的兜底）。
    pub port: u16,
    /// 该实例的 dsh Web UI URL（含 `dshToken`，**是有效凭据**）。
    pub local_url: Option<String>,
}

/// 本机局域网 IPv4 的缓存。
#[derive(Default)]
struct LanCache {
    addrs: Vec<IpAddr>,
    refreshed_at: Option<Instant>,
    /// 后台解析正在进行（避免每次事件都起一个线程）。
    refreshing: bool,
}

/// webhook 事件源。
pub struct Notifier {
    dispatcher: Arc<Dispatcher>,
    lan: Arc<Mutex<LanCache>>,
    /// 有没有哪条 start 模板真的用到 `{lan_url}`（没有就不做探测，省掉不必要的 TCP 连接）。
    wants_lan_url: AtomicBool,
}

impl Notifier {
    /// 用当前配置起一个通知器（投递线程随之启动）。
    pub fn start(webhooks: Vec<Webhook>) -> Arc<Self> {
        let notifier = Self {
            dispatcher: Dispatcher::start(Arc::new(DiagSink), webhooks.clone()),
            lan: Arc::new(Mutex::new(LanCache::default())),
            wants_lan_url: AtomicBool::new(wants_lan_url(&webhooks)),
        };
        Arc::new(notifier)
    }

    /// 热换配置（控制台改了 webhook 立刻生效）。
    pub fn apply(&self, webhooks: Vec<Webhook>) {
        self.wants_lan_url
            .store(wants_lan_url(&webhooks), Ordering::Relaxed);
        self.dispatcher.apply(webhooks);
    }

    /// 抛一个事件（不阻塞；真正发送在投递线程里）。
    pub fn event(&self, trigger: Trigger, facts: &Facts) {
        // Start / HousingStart 才可能有 `{lan_url}`；其余事件模板里没有它。
        let wants_lan = matches!(trigger, Trigger::Start | Trigger::HousingStart);
        let lan_url = if wants_lan && self.wants_lan_url.load(Ordering::Relaxed) {
            self.lan_url(facts)
        } else {
            None
        };
        self.dispatcher.submit(Event {
            trigger,
            time: diag::timestamp_text(),
            alias: facts.alias.clone(),
            dsh_version: facts.dsh_version.clone(),
            profile: facts.profile.clone(),
            local_url: facts.local_url.clone(),
            lan_url,
        });
    }

    /// Housing Start（v0.3 / #259）：core 启动、控制台就绪后发一次。
    ///
    /// `local_url` 是**控制台** URL（含 consoleToken）；`{lan_url}` 由同一条探测逻辑给出——
    /// 控制台只绑回环时为空，将来支持局域网绑定时自动有值。`{alias}` / `{dsh_version}` /
    /// `{dsh_profile}` 在这个事件里没有意义，给固定标签 / 空串。
    pub fn housing_start(&self, local_url: Option<String>, port: u16) {
        let facts = Facts {
            alias: "DSH Housing".to_string(),
            dsh_version: env!("CARGO_PKG_VERSION").to_string(),
            profile: String::new(),
            port,
            local_url,
        };
        self.event(Trigger::HousingStart, &facts);
    }

    /// core 退出前等队列排空（有界）。
    pub fn drain(&self, budget: Duration) -> bool {
        self.dispatcher.drain(budget)
    }

    /// 投递线程还活着吗（诊断用）。
    pub fn is_running(&self) -> bool {
        self.dispatcher.is_running()
    }

    /// `{lan_url}`：把 `local_url` 的 host 换成**探测可达**的本机局域网 IPv4。
    fn lan_url(&self, facts: &Facts) -> Option<String> {
        let local_url = facts.local_url.as_deref()?;
        let port = port_of(local_url).unwrap_or(facts.port);
        for address in self.lan_addresses() {
            if !probe(address, port) {
                continue;
            }
            let url = rewrite_host(local_url, address, port)?;
            diag::info(&format!(
                "webhook：局域网可达 {address}:{port}（{{lan_url}} 用它）"
            ));
            return Some(url);
        }
        None
    }

    /// 本机候选局域网地址（缓存；过期时后台刷新，**首次同步解析**）。
    ///
    /// 首次必须同步：`event()` 在 core 启动后的第一个 Start 事件里就要用地址，而「起一个
    /// 后台线程然后返回空表」会让这次事件永远带空 `{lan_url}`（2026-10-21 的真实通知就是
    /// 这个现象，§5.12 D30）。同步解析 = 一次 UDP 路由选择 + 本机名解析，毫秒级。
    fn lan_addresses(&self) -> Vec<IpAddr> {
        let mut cache = self
            .lan
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let stale = cache
            .refreshed_at
            .map(|at| at.elapsed() >= LAN_CACHE_TTL)
            .unwrap_or(true);
        if stale && !cache.refreshing {
            if cache.refreshed_at.is_none() {
                // 首次：同步解析，保证这次事件拿到真实地址。
                cache.addrs = net::lan_candidates();
                cache.refreshed_at = Some(Instant::now());
            } else {
                // 过期：保留旧值，后台刷新（不阻塞 supervisor）。
                cache.refreshing = true;
                let shared = self.lan.clone();
                let spawned = thread::Builder::new()
                    .name("lan-addresses".to_string())
                    .spawn(move || {
                        let addrs = net::lan_candidates();
                        let mut cache = shared
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        cache.addrs = addrs;
                        cache.refreshed_at = Some(Instant::now());
                        cache.refreshing = false;
                    });
                if let Err(err) = spawned {
                    cache.refreshing = false;
                    diag::warn(&format!("webhook：局域网地址解析线程起不来（{err}）"));
                }
            }
        }
        cache.addrs.clone()
    }
}

/// 有没有 enabled 的 Start / HousingStart 模板用到 `{lan_url}`。
fn wants_lan_url(webhooks: &[Webhook]) -> bool {
    webhooks.iter().any(|hook| {
        [Trigger::Start, Trigger::HousingStart]
            .iter()
            .any(|trigger| {
                hook.template_for(*trigger)
                    .map(|template| template_uses(template, "lan_url"))
                    .unwrap_or(false)
            })
    })
}

/// 从本机连一下自己的局域网地址：通了才说明 dsh 真的在这个地址上听（不是只绑了回环）。
fn probe(address: IpAddr, port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::new(address, port), LAN_PROBE_TIMEOUT).is_ok()
}

/// 取 URL 里的端口（`http://127.0.0.1:39412/?token=…` → 39412）。
fn port_of(url: &str) -> Option<u16> {
    let (_, rest) = url.trim().split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    port_of_authority(&rest[..authority_end])
}

fn port_of_authority(authority: &str) -> Option<u16> {
    if authority.is_empty() {
        return None;
    }
    let port = if let Some(inner) = authority.strip_prefix('[') {
        // IPv6：[::1]:8080
        inner.split_once(']')?.1.strip_prefix(':')?
    } else {
        match authority.rsplit_once(':') {
            Some((_, port)) => port,
            None => return None,
        }
    };
    port.parse::<u16>().ok()
}

/// 把 URL 的 host 换成给定地址（保留端口、路径与 token）。
fn rewrite_host(url: &str, address: IpAddr, default_port: u16) -> Option<String> {
    let (scheme, rest) = url.trim().split_once("://")?;
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() {
        return None;
    }
    let port = port_of_authority(authority).unwrap_or(default_port);
    let mut tail = rest[authority_end..].to_string();
    if tail.is_empty() {
        tail.push('/');
    }
    Some(format!("{scheme}://{address}:{port}{tail}"))
}

/// 日志出口：接到 core 的 `diag`（crate 不依赖 core）。
struct DiagSink;

impl Sink for DiagSink {
    fn info(&self, message: &str) {
        diag::info(message);
    }

    fn warn(&self, message: &str) {
        diag::warn(message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsh_housing_webhook::{TriggerConfig, DEFAULT_STOP_TEMPLATE};
    use std::net::TcpListener;

    fn facts(local_url: Option<&str>) -> Facts {
        Facts {
            alias: "Deepseek Harness 0.2.0-rc.2 web".to_string(),
            dsh_version: "0.2.0-rc.2".to_string(),
            profile: "web".to_string(),
            port: 39412,
            local_url: local_url.map(str::to_string),
        }
    }

    #[test]
    fn rewrite_keeps_path_and_token_but_swaps_host() {
        let url = "http://127.0.0.1:39412/?token=abc";
        let rewritten = rewrite_host(url, "192.168.3.100".parse().unwrap(), 39412).unwrap();
        assert_eq!(rewritten, "http://192.168.3.100:39412/?token=abc");
    }

    #[test]
    fn rewrite_uses_default_port_when_url_has_none() {
        let rewritten = rewrite_host(
            "http://localhost/?token=abc",
            "192.168.3.100".parse().unwrap(),
            39412,
        )
        .unwrap();
        assert_eq!(rewritten, "http://192.168.3.100:39412/?token=abc");
    }

    #[test]
    fn port_of_reads_the_authority_port_only() {
        assert_eq!(port_of("http://127.0.0.1:39412/?token=x"), Some(39412));
        assert_eq!(port_of("https://[::1]:9443/x"), Some(9443));
        assert_eq!(port_of("http://example.test/x"), None);
        assert_eq!(port_of("nonsense"), None);
    }

    #[test]
    fn probe_only_succeeds_when_something_listens() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().unwrap().port();
        assert!(probe("127.0.0.1".parse().unwrap(), port));
        let closed_port = {
            let probe_listener = TcpListener::bind("127.0.0.1:0").unwrap();
            probe_listener.local_addr().unwrap().port()
        };
        assert!(!probe("127.0.0.1".parse().unwrap(), closed_port));
    }

    #[test]
    fn first_lan_lookup_is_synchronous() {
        // #270 回归：第一次取地址不能只起后台线程就返回空表（否则首个 Start 事件必空）。
        let notifier = Notifier::start(Vec::new());
        let first = notifier.lan_addresses();
        let cache = notifier.lan.lock().unwrap();
        assert!(
            cache.refreshed_at.is_some(),
            "首次调用后缓存必须已经写入（同步解析）"
        );
        assert_eq!(first, cache.addrs);
    }

    #[test]
    fn wants_lan_url_when_either_start_or_housing_start_mentions_it() {
        let mut hook = dsh_housing_webhook::Webhook {
            url: "https://example.test/hook".to_string(),
            ..Default::default()
        };
        hook.start.template = DEFAULT_STOP_TEMPLATE.to_string();
        hook.housing_start.enabled = false;
        assert!(!wants_lan_url(&[hook.clone()]));

        hook.start.template = "start {lan_url}".to_string();
        assert!(wants_lan_url(&[hook.clone()]));

        // Start 不用、但 Housing Start 用，也要触发探测（v0.3 / #259）。
        hook.start.template = DEFAULT_STOP_TEMPLATE.to_string();
        hook.housing_start.enabled = true;
        hook.housing_start.template = "housing {lan_url}".to_string();
        assert!(wants_lan_url(&[hook.clone()]));

        hook.enabled = false;
        assert!(!wants_lan_url(&[hook.clone()]), "整条关掉就不该触发探测");

        hook.enabled = true;
        hook.start.enabled = false;
        hook.housing_start.enabled = false;
        assert!(!wants_lan_url(&[hook]));
    }

    #[test]
    fn notifier_without_config_still_accepts_events() {
        // 没配 webhook 时投递线程也不该崩：事件进队列、立刻被忽略。
        let notifier = Notifier::start(Vec::new());
        notifier.event(
            Trigger::Start,
            &facts(Some("http://127.0.0.1:39412/?token=x")),
        );
        notifier.housing_start(Some("http://127.0.0.1:30010/?token=x".to_string()), 30010);
        assert!(notifier.drain(Duration::from_secs(5)));
        assert!(notifier.is_running());
    }

    #[test]
    fn lan_url_replaces_the_host_of_a_reachable_wildcard_binding() {
        // 绑 0.0.0.0 的监听 = dsh 绑通配时的形态：用候选地址探测应当通过，
        // lan_url 把 host 换成它、保留端口与 token。
        if net::default_route_ipv4().is_none() {
            eprintln!("本机没有默认路由地址，跳过这条（不算失败）");
            return;
        }
        let listener = std::net::TcpListener::bind(("0.0.0.0", 0)).expect("绑定 0.0.0.0");
        let port = listener.local_addr().unwrap().port();
        let notifier = Notifier::start(Vec::new());
        // 首次同步解析：一次调用就该就绪（不再轮询后台线程）。
        let candidates = notifier.lan_addresses();
        assert!(!candidates.is_empty(), "首次同步解析应给出本机 LAN 候选");
        let first = candidates[0];

        let facts = Facts {
            local_url: Some(format!("http://127.0.0.1:{port}/?token=abc")),
            port,
            ..facts(None)
        };
        match notifier.lan_url(&facts) {
            Some(url) => assert_eq!(url, format!("http://{first}:{port}/?token=abc")),
            // 有些机器的防火墙会拦「本机连自己的局域网地址」；那不是代码问题，跳过。
            None => eprintln!("{first}:{port} 探测不可达（防火墙？），跳过地址断言"),
        }
    }

    #[test]
    fn start_event_without_local_url_has_no_lan_url() {
        // 拿不到 token URL 时就别猜一个局域网地址。
        let notifier = Notifier::start(vec![dsh_housing_webhook::Webhook {
            url: "https://example.test/hook".to_string(),
            start: TriggerConfig {
                enabled: true,
                template: "{lan_url}".to_string(),
            },
            ..Default::default()
        }]);
        assert!(notifier.lan_url(&facts(None)).is_none());
    }
}
