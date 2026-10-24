//! 本地 HTTP 服务：路由、鉴权、静态资源、统一错误（docs/structure.md §3.17 / §3.23）。
//!
//! # 鉴权三件套
//!
//! 1. 默认只绑 `127.0.0.1`（由调用方给的 [`ConsoleConfig::bind`] 决定；v0.4 / #286 起 core
//!    也可以传 `0.0.0.0` 或本机具体 IPv4，用户显式开启「局域网可访问」时才会这样）；
//! 2. 每个 `/api/*` 请求必须带 `consoleToken`（`X-Console-Token` 头，或 `?token=` 给脚本用）；
//! 3. 变更请求（POST / PUT / DELETE）若带 `Origin`，必须命中白名单：`127.0.0.1:<port>` /
//!    `localhost:<port>`，绑定非回环时再加 [`ConsoleConfig::origin_hosts`] 里的每个本机地址。
//!
//! # 线程模型
//!
//! [`serve`] 在**专用线程**里跑 current-thread tokio runtime；core 是同步的，
//! handler 调 [`ConsoleBackend`] 的同步方法（都是读快照 / 写小 json，毫秒级）。

use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::JoinHandle;

use axum::body::{Body, Bytes};
use axum::extract::{Query, Request, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde_json::json;

use crate::api::{
    ActionResult, AliasRequest, AutostartRequest, BuilderLogQuery, BuilderStartRequest,
    BuilderVersionsQuery, ConsoleBackend, EnvRequest, HostRequest, KeyRequest, LogPayload,
    LogsQuery, PortRequest, RegisterRequest, RemoveRequest, SettingsRequest, WebhooksRequest,
    DEFAULT_LOG_TAIL, MAX_LOG_TAIL,
};
use crate::assets;

/// 提前拒绝（401 / 403）前最多读掉多少请求体，见 [`guard`]。
const MAX_DRAIN_BYTES: usize = 256 * 1024;

/// 控制台启动参数。
#[derive(Debug, Clone)]
pub struct ConsoleConfig {
    /// 绑定地址；默认 `127.0.0.1:<gui_port>`（`0` = 随机端口）；
    /// v0.4 / #286 起也可能是 `0.0.0.0` 或本机具体 IPv4（用户显式开启局域网访问）。
    pub bind: SocketAddr,
    /// 每次启动新生成的 consoleToken（64 个 hex 字符）。
    pub token: String,
    /// 允许的 `Origin` 列表；空 = 按实际绑定端口推导（`http://127.0.0.1:<p>` / `http://localhost:<p>`）。
    pub allowed_origins: Vec<String>,
    /// 额外的「本机地址」：绑定非回环时浏览器会从 `http://<本机 IP>:<port>` 打开页面，
    /// 那些 `Origin` 也要进白名单（`allowed_origins` 为空时才用）。
    pub origin_hosts: Vec<IpAddr>,
}

impl ConsoleConfig {
    /// core 用的构造：只绑回环、Origin 由端口推导。
    pub fn loopback(port: u16, token: impl Into<String>) -> Self {
        Self::on(SocketAddr::from(([127, 0, 0, 1], port)), token)
    }

    /// 指定绑定地址（v0.4 / #286）；Origin 仍按实际绑定端口推导。
    pub fn on(bind: SocketAddr, token: impl Into<String>) -> Self {
        Self {
            bind,
            token: token.into(),
            allowed_origins: Vec::new(),
            origin_hosts: Vec::new(),
        }
    }
}

/// 控制台启动失败的原因（core 只告警，不阻断）。
#[derive(Debug)]
pub struct ConsoleError {
    detail: String,
}

impl ConsoleError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ConsoleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for ConsoleError {}

/// 已启动的控制台：实际地址 + 关闭通道 + 线程句柄。
///
/// core 退出时进程整体消失，句柄随之失效；`stop()` 只给单测 / 将来的「换端口重启」用。
pub struct ConsoleHandle {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl ConsoleHandle {
    /// 实际监听地址（`bind` 传 0 时这里才是真实端口）。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 请求 graceful shutdown 并等线程退出（有界：axum 的 graceful 会等现有连接结束）。
    pub fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ConsoleHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

/// 启动 HTTP 服务（绑定失败立即返回 `Err`，core 据此决定「控制台不可用」）。
pub fn serve(
    config: ConsoleConfig,
    backend: Arc<dyn ConsoleBackend>,
) -> Result<ConsoleHandle, ConsoleError> {
    let listener = TcpListener::bind(config.bind)
        .map_err(|err| ConsoleError::new(format!("绑定 {} 失败：{err}", config.bind)))?;
    let addr = listener
        .local_addr()
        .map_err(|err| ConsoleError::new(format!("读取监听地址失败：{err}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|err| ConsoleError::new(format!("设置非阻塞监听失败：{err}")))?;

    // 绑完之后才知道真实端口（bind 传 0 时），Origin 与响应里的端口都用它。
    let app = router(
        ConsoleConfig {
            bind: addr,
            ..config
        },
        backend,
    );
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

    let join = std::thread::Builder::new()
        .name("console-http".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    // GUI 子系统没有控制台可打；core 启动日志里已经记了端口，这里只能静默退出。
                    eprintln!("dsh-housing 控制台：创建 tokio runtime 失败：{err}");
                    return;
                }
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(err) => {
                        eprintln!("dsh-housing 控制台：接管监听 socket 失败：{err}");
                        return;
                    }
                };
                let _ = axum::serve(listener, app)
                    .with_graceful_shutdown(async move {
                        let _ = shutdown_rx.await;
                    })
                    .await;
            });
        })
        .map_err(|err| ConsoleError::new(format!("启动 HTTP 线程失败：{err}")))?;

    Ok(ConsoleHandle {
        addr,
        shutdown: Some(shutdown_tx),
        join: Some(join),
    })
}

/// 组装路由（`serve` 与单测共用；单测用 tower 的 `oneshot` 直接打它，不真起 TCP）。
pub fn router(config: ConsoleConfig, backend: Arc<dyn ConsoleBackend>) -> Router {
    let origins = if config.allowed_origins.is_empty() {
        let port = config.bind.port();
        let mut origins = vec![
            format!("http://127.0.0.1:{port}"),
            format!("http://localhost:{port}"),
        ];
        // v0.4 / #286：绑非回环时，页面来源会是本机某个局域网地址。
        for host in &config.origin_hosts {
            let origin = format!("http://{host}:{port}");
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        origins
    } else {
        config.allowed_origins.clone()
    };
    let context = Context {
        backend,
        token: config.token,
        origins,
    };

    Router::new()
        .route("/", get(static_file))
        .route("/favicon.ico", get(favicon))
        .route("/api/state", get(api_state))
        .route("/api/instances/start", post(api_start))
        .route("/api/instances/stop", post(api_stop))
        .route("/api/instances/open", post(api_open))
        .route("/api/instances/logs", get(api_logs))
        .route("/api/instances/env", put(api_env))
        .route("/api/instances/alias", put(api_alias))
        .route("/api/instances/port", put(api_port))
        .route("/api/instances/host", put(api_host))
        .route("/api/instances/autostart", put(api_autostart))
        .route("/api/settings", put(api_settings))
        .route("/api/webhooks", put(api_webhooks))
        .route("/api/autostart/enable", post(api_autostart_enable))
        .route("/api/autostart/disable", post(api_autostart_disable))
        .route("/api/capsules/register", post(api_register))
        .route("/api/capsules/remove", post(api_remove))
        // v0.5 / #249：可移植版本构建器（§3.24）
        .route("/api/builder/start", post(api_builder_start))
        .route("/api/builder/cancel", post(api_builder_cancel))
        .route("/api/builder/log", get(api_builder_log))
        .route("/api/builder/versions", get(api_builder_versions))
        .fallback(static_file)
        .with_state(context.clone())
        .layer(middleware::from_fn(security_headers))
        .layer(middleware::from_fn_with_state(context, guard))
}

/// 路由共享的上下文（token 与允许的 Origin 只在内存里）。
#[derive(Clone)]
struct Context {
    backend: Arc<dyn ConsoleBackend>,
    token: String,
    origins: Vec<String>,
}

// ── 中间件 ──

/// 鉴权：`/api/*` 要 token；变更请求要 Origin。
///
/// 提前拒绝之前会**先把请求体读掉**（上限 [`MAX_DRAIN_BYTES`]）：客户端还在写、服务端直接
/// 关连接，Windows 会给未读数据回 RST，浏览器于是只看到「网络错误」而读不到我们的
/// `401` / `403` JSON。读掉之后连接是干净关的，前端横幅能显示真正的原因。
async fn guard(State(context): State<Context>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if path.starts_with("/api/") {
        let mutation = matches!(
            *request.method(),
            Method::POST | Method::PUT | Method::DELETE
        );
        let rejection = if !token_matches(&context, request.headers(), request.uri()) {
            Some(api_error(
                StatusCode::UNAUTHORIZED,
                "缺少或错误的 consoleToken",
            ))
        } else if mutation
            && request
                .headers()
                .get(header::ORIGIN)
                .map(|origin| {
                    let origin = origin.to_str().unwrap_or_default();
                    !context.origins.iter().any(|allowed| allowed == origin)
                })
                .unwrap_or(false)
        {
            Some(api_error(StatusCode::FORBIDDEN, "Origin 不被信任"))
        } else {
            None
        };

        if let Some(response) = rejection {
            let (_, body) = request.into_parts();
            let _ = axum::body::to_bytes(body, MAX_DRAIN_BYTES).await;
            return response;
        }
    }
    next.run(request).await
}

/// 统一安全响应头：token / 页面内容都不该被缓存或经 Referer 外泄。
async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    // Vue 的浏览器构建在运行时编译 index.html 里的模板，需要 'unsafe-eval'；
    // 页面没有内联脚本 / 外链资源，数据只经 Vue 的文本插值（默认转义）渲染。
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-eval'; style-src 'self'; \
             img-src 'self' data:; connect-src 'self'",
        ),
    );
    response
}

fn token_matches(context: &Context, headers: &HeaderMap, uri: &Uri) -> bool {
    if let Some(value) = headers.get("x-console-token") {
        if let Ok(text) = value.to_str() {
            if constant_time_eq(text, &context.token) {
                return true;
            }
        }
    }
    if let Some(query) = uri.query() {
        for pair in query.split('&') {
            if let Some((name, value)) = pair.split_once('=') {
                if name == "token" && constant_time_eq(value, &context.token) {
                    return true;
                }
            }
        }
    }
    false
}

/// 长度 + 内容都相等的比较（避免把 token 比较做成明显的早退分支）。
fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

// ── API handlers ──

async fn api_state(State(context): State<Context>) -> Response {
    Json(context.backend.state()).into_response()
}

async fn api_start(State(context): State<Context>, Json(request): Json<KeyRequest>) -> Response {
    action(context.backend.start(&request.key))
}

async fn api_stop(State(context): State<Context>, Json(request): Json<KeyRequest>) -> Response {
    action(context.backend.stop(&request.key))
}

async fn api_open(State(context): State<Context>, Json(request): Json<KeyRequest>) -> Response {
    action(context.backend.open(&request.key))
}

async fn api_env(State(context): State<Context>, Json(request): Json<EnvRequest>) -> Response {
    action(context.backend.set_env(&request.key, request.env))
}

async fn api_alias(State(context): State<Context>, Json(request): Json<AliasRequest>) -> Response {
    let alias = {
        let text = request.alias.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    };
    action(context.backend.set_alias(&request.key, alias))
}

async fn api_register(
    State(context): State<Context>,
    Json(request): Json<RegisterRequest>,
) -> Response {
    action(context.backend.register(&request.root))
}

async fn api_remove(
    State(context): State<Context>,
    Json(request): Json<RemoveRequest>,
) -> Response {
    action(
        context
            .backend
            .remove_capsule(&request.capsule_id, &request.confirm),
    )
}

async fn api_port(State(context): State<Context>, Json(request): Json<PortRequest>) -> Response {
    action(
        context
            .backend
            .set_instance_port(&request.key, request.port),
    )
}

async fn api_host(State(context): State<Context>, Json(request): Json<HostRequest>) -> Response {
    action(
        context
            .backend
            .set_instance_host(&request.key, &request.host),
    )
}

async fn api_autostart(
    State(context): State<Context>,
    Json(request): Json<AutostartRequest>,
) -> Response {
    action(
        context
            .backend
            .set_instance_autostart(&request.key, request.autostart),
    )
}

async fn api_settings(
    State(context): State<Context>,
    Json(request): Json<SettingsRequest>,
) -> Response {
    action(context.backend.set_settings(request))
}

async fn api_webhooks(
    State(context): State<Context>,
    Json(request): Json<WebhooksRequest>,
) -> Response {
    action(context.backend.set_webhooks(request.webhooks))
}

async fn api_autostart_enable(State(context): State<Context>) -> Response {
    action(context.backend.enable_autostart())
}

async fn api_autostart_disable(State(context): State<Context>) -> Response {
    action(context.backend.disable_autostart())
}

// ── v0.5 / #249：可移植版本构建器（§3.24） ──

async fn api_builder_start(
    State(context): State<Context>,
    Json(request): Json<BuilderStartRequest>,
) -> Response {
    action(context.backend.builder_start(request))
}

async fn api_builder_cancel(State(context): State<Context>) -> Response {
    action(context.backend.builder_cancel())
}

/// 构建日志：与实例日志同一套语义（`text/plain` + `x-log-source`）。
async fn api_builder_log(
    State(context): State<Context>,
    Query(query): Query<BuilderLogQuery>,
) -> Response {
    let tail = query
        .tail
        .unwrap_or(DEFAULT_LOG_TAIL)
        .clamp(1024, MAX_LOG_TAIL);
    match context.backend.builder_log(tail) {
        Ok(payload) => log_response(payload),
        Err(err) => api_error(
            StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            err.message,
        ),
    }
}

async fn api_builder_versions(
    State(context): State<Context>,
    Query(query): Query<BuilderVersionsQuery>,
) -> Response {
    let refresh = query.refresh.unwrap_or(0) != 0;
    Json(context.backend.builder_versions(refresh)).into_response()
}

async fn api_logs(State(context): State<Context>, Query(query): Query<LogsQuery>) -> Response {
    let tail = query
        .tail
        .unwrap_or(DEFAULT_LOG_TAIL)
        .clamp(1024, MAX_LOG_TAIL);
    match context.backend.logs(&query.key, tail) {
        Ok(payload) => log_response(payload),
        Err(err) => api_error(
            StatusCode::from_u16(err.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            err.message,
        ),
    }
}

// ── 静态资源 ──

/// v0.4 / #271：控制台 favicon——仓库根 `assets/favicon.ico` 的原始字节。
///
/// 不放进 `web/` 是因为它跟 exe 资源 / 托盘图标是同一份文件（`crates/app/app.rc` 的 `1 ICON`），
/// 复制一份到前端目录只会让「换图标」变成改两处。
async fn favicon() -> Response {
    let mut response = Response::new(Body::from(Bytes::from_static(assets::FAVICON)));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("image/x-icon"),
    );
    response
}

async fn static_file(uri: Uri) -> Response {
    let raw = uri.path().trim_start_matches('/');
    // 没有前端路由：`/` 之外只认 literal 文件路径。
    let path = if raw.is_empty() { "index.html" } else { raw };

    if path == "api" || path.starts_with("api/") {
        return api_error(StatusCode::NOT_FOUND, format!("没有这个接口：/{path}"));
    }
    if path
        .split('/')
        .any(|segment| segment == ".." || segment.contains('\\'))
    {
        return api_error(StatusCode::NOT_FOUND, "找不到资源");
    }

    match assets::get(path) {
        Some(bytes) => {
            let mut response = Response::new(Body::from(Bytes::from_static(bytes)));
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static(assets::content_type(path)),
            );
            response
        }
        None => api_error(StatusCode::NOT_FOUND, format!("找不到资源：/{path}")),
    }
}

// ── 响应构造 ──

fn action(result: ActionResult) -> Response {
    let status = StatusCode::from_u16(result.status).unwrap_or(StatusCode::OK);
    (status, Json(result)).into_response()
}

fn api_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "ok": false, "error": message.into() })),
    )
        .into_response()
}

fn log_response(payload: LogPayload) -> Response {
    let mut response = (StatusCode::OK, payload.text).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("x-log-source"),
        HeaderValue::from_str(&payload.source).unwrap_or(HeaderValue::from_static("none")),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{
        AppView, AutostartView, BackendError, BuilderVersionsView, CapsuleView, ConsoleInfo,
        ConsoleState, InstanceConfigView, RegistryView, RuntimeView, SettingsView, WebhookView,
    };
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tower::ServiceExt;

    /// 记录收到的动作，便于断言「handler 真的调到了 backend」。
    struct FakeBackend {
        calls: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: Mutex::new(Vec::new()),
            })
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().unwrap().push(call.into());
        }
    }

    fn sample_state() -> ConsoleState {
        ConsoleState {
            status: serde_json::json!({
                "instances": [{
                    "key": "cap/web", "capsule_id": "cap", "profile": "web",
                    "port": 39412, "state": "running", "pid": 4242
                }],
                "shutting_down": false,
                "tray_running": true
            }),
            registry: RegistryView {
                data_dir: r"D:\Tools\dsh-housing\.dsh-housing".to_string(),
                capsules: vec![CapsuleView {
                    capsule_id: "cap".to_string(),
                    root: r"D:\capsules\cap".to_string(),
                    alias: None,
                    schema: 1,
                    registered_at: 1,
                    checked_at: 2,
                    runtime: Some(RuntimeView {
                        kind: "node-portable-zip".to_string(),
                        version: "24.21.0".to_string(),
                    }),
                    app: Some(AppView {
                        package: "@deepseek-ai/dsh".to_string(),
                        version: "0.2.0-rc.2".to_string(),
                        bin: "lib/bin.js".to_string(),
                    }),
                    warnings: Vec::new(),
                    instances: vec![InstanceConfigView {
                        profile: "web".to_string(),
                        port: 39412,
                        host: None,
                        workspace: r"D:\capsules\cap\workspace-web".to_string(),
                        alias: None,
                        autostart: false,
                        env: BTreeMap::new(),
                    }],
                }],
            },
            console: ConsoleInfo { port: 39420 },
            settings: SettingsView {
                version: 1,
                gui_port: 0,
                console_host: "127.0.0.1".to_string(),
                sample_interval_ms: 3_000,
                auto_open_browser: false,
                auto_open_console: true,
                tray_restart_max: 3,
                tray_restart_backoff_ms: vec![1_000, 5_000, 15_000],
                log_retention_days: 14,
                lan_ip: Some("192.168.3.100".to_string()),
                settings_path: r"D:\Tools\dsh-housing\.dsh-housing\settings.json".to_string(),
                exe_path: r"D:\Tools\dsh-housing\dsh-housing.exe".to_string(),
                exe_integrity: "high".to_string(),
                console_port: 39420,
                core_pid: 4242,
            },
            autostart: AutostartView {
                task_name: "dsh-housing".to_string(),
                state: "missing".to_string(),
                detail: None,
                legacy: None,
                can_enable: true,
            },
            webhooks: crate::api::WebhookSection {
                file_path: r"D:\Tools\dsh-housing\.dsh-housing\webhooks.json".to_string(),
                items: Vec::new(),
                defaults: crate::api::TriggerTemplates {
                    start: "Service Start".to_string(),
                    stop: "Service Stop".to_string(),
                    crashed: "Service Crashed".to_string(),
                    housing_start: "Housing Start".to_string(),
                },
                placeholders: vec!["{time}".to_string()],
            },
            builder: crate::api::BuilderView {
                phase: "idle".to_string(),
                step: None,
                step_title: String::new(),
                step_index: 0,
                step_total: 6,
                request: None,
                started_at: 0,
                elapsed_ms: 0,
                pid: None,
                error: None,
                result: None,
                log_path: None,
                log_source: "none".to_string(),
                last: None,
            },
        }
    }

    impl ConsoleBackend for FakeBackend {
        fn state(&self) -> ConsoleState {
            sample_state()
        }

        // v0.5 / #249：构建器（§3.24）——假后端只记一笔，够验路由 / 鉴权 / 形状。
        fn builder_start(&self, request: BuilderStartRequest) -> ActionResult {
            self.record(format!("builder-start:{}", request.name));
            ActionResult::ok(sample_state(), "已开始构建")
        }

        fn builder_cancel(&self) -> ActionResult {
            self.record("builder-cancel".to_string());
            ActionResult::ok(sample_state(), "已请求取消构建")
        }

        fn builder_log(&self, _tail: usize) -> Result<LogPayload, BackendError> {
            Ok(LogPayload {
                text: "[builder] fake line\n".to_string(),
                source: "ring".to_string(),
            })
        }

        fn builder_versions(&self, refresh: bool) -> BuilderVersionsView {
            BuilderVersionsView {
                fetched_at: if refresh { 1_700_000_000 } else { 0 },
                stale: false,
                dsh_tags: BTreeMap::from([("next".to_string(), "0.2.1-rc.3".to_string())]),
                dsh_versions: vec!["0.2.1-rc.3".to_string(), "0.2.1-rc.2".to_string()],
                node_versions: vec!["v24.22.0".to_string()],
                default_root: r"D:\Images\DeepseekHarness".to_string(),
                default_profile: "web".to_string(),
                profiles: vec!["web".to_string(), "headless".to_string()],
                registry: "https://registry.npmmirror.com/".to_string(),
            }
        }

        fn start(&self, key: &str) -> ActionResult {
            self.record(format!("start:{key}"));
            ActionResult::ok(sample_state(), format!("已启动 {key}"))
        }

        fn stop(&self, key: &str) -> ActionResult {
            self.record(format!("stop:{key}"));
            ActionResult::ok(sample_state(), format!("已停止 {key}"))
        }

        fn open(&self, key: &str) -> ActionResult {
            self.record(format!("open:{key}"));
            ActionResult::ok(sample_state(), "实例还没就绪（没有 token URL）")
        }

        fn logs(&self, key: &str, tail: usize) -> Result<LogPayload, BackendError> {
            self.record(format!("logs:{key}:{tail}"));
            if key == "missing/web" {
                return Err(BackendError::not_found(format!("没有这个实例：{key}")));
            }
            if key == "broken/web" {
                return Err(BackendError::internal(
                    "读日志失败：拒绝访问。 (os error 5)",
                ));
            }
            Ok(LogPayload {
                text: format!("log tail for {key}"),
                source: "ring".to_string(),
            })
        }

        fn set_env(&self, key: &str, env: BTreeMap<String, String>) -> ActionResult {
            self.record(format!("env:{key}:{}", env.len()));
            ActionResult::ok(sample_state(), "已保存，重启实例后生效")
        }

        fn set_alias(&self, key: &str, alias: Option<String>) -> ActionResult {
            self.record(format!("alias:{key}:{}", alias.unwrap_or_default()));
            ActionResult::ok(sample_state(), "已保存")
        }

        fn register(&self, root: &str) -> ActionResult {
            self.record(format!("register:{root}"));
            ActionResult::ok(sample_state(), format!("已登记 {root}"))
        }

        fn remove_capsule(&self, capsule_id: &str, confirm: &str) -> ActionResult {
            self.record(format!("remove:{capsule_id}:{confirm}"));
            if confirm != capsule_id {
                return ActionResult::fail(400, "确认文本与胶囊标识不一致");
            }
            ActionResult::ok(sample_state(), format!("已移除 {capsule_id}"))
        }

        fn set_settings(&self, settings: SettingsRequest) -> ActionResult {
            if settings.sample_interval_ms < 500 {
                return ActionResult::fail(400, "采样间隔必须在 500–60000 毫秒之间");
            }
            self.record(format!(
                "settings:{}:{}",
                settings.gui_port, settings.sample_interval_ms
            ));
            ActionResult::ok(sample_state(), "已保存设置")
        }

        fn set_instance_port(&self, key: &str, port: u16) -> ActionResult {
            if port == 0 {
                return ActionResult::fail(400, "端口必须在 1..=65535");
            }
            self.record(format!("port:{key}:{port}"));
            ActionResult::ok(sample_state(), format!("端口已改为 {port}，下次启动生效"))
        }

        fn set_instance_host(&self, key: &str, host: &str) -> ActionResult {
            let host = host.trim();
            if host == "0.0.0.0" {
                return ActionResult::fail(400, "host 不能是通配地址 0.0.0.0");
            }
            self.record(format!("host:{key}:{host}"));
            ActionResult::ok(sample_state(), format!("host 已改为 {host}，下次启动生效"))
        }

        fn set_instance_autostart(&self, key: &str, autostart: bool) -> ActionResult {
            self.record(format!("autostart:{key}:{autostart}"));
            ActionResult::ok(
                sample_state(),
                if autostart {
                    "已标记为登录自启"
                } else {
                    "已取消登录自启"
                },
            )
        }

        fn enable_autostart(&self) -> ActionResult {
            self.record("autostart-enable");
            ActionResult::ok(sample_state(), "已创建任务 dsh-housing")
        }

        fn disable_autostart(&self) -> ActionResult {
            self.record("autostart-disable");
            ActionResult::ok(sample_state(), "已删除任务 dsh-housing")
        }

        fn set_webhooks(&self, webhooks: Vec<WebhookView>) -> ActionResult {
            self.record(format!("webhooks:{}", webhooks.len()));
            // 只做一条与 core 同口径的前置校验：scheme 不合法 → 400（真实校验在 core）。
            if let Some(bad) = webhooks
                .iter()
                .find(|hook| !hook.url.starts_with("http://") && !hook.url.starts_with("https://"))
            {
                return ActionResult::fail(
                    400,
                    format!("URL 必须是 http:// 或 https://：{}", bad.url),
                );
            }
            ActionResult::ok(sample_state(), "已保存 webhook（即时生效）")
        }
    }

    const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn app(backend: Arc<FakeBackend>) -> Router {
        router(ConsoleConfig::loopback(39420, TOKEN), backend)
    }

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("测试 runtime")
            .block_on(future)
    }

    fn request(
        method: Method,
        uri: &str,
        token: Option<&str>,
        origin: Option<&str>,
        body: Option<&str>,
    ) -> Request {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header("x-console-token", token);
        }
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                builder.body(Body::from(body.to_string())).unwrap()
            }
            None => builder.body(Body::empty()).unwrap(),
        }
    }

    async fn body_text(response: Response) -> String {
        use http_body_util::BodyExt;
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    fn api_requires_the_token() {
        let backend = FakeBackend::new();
        let app = app(backend);

        let response =
            block_on(
                app.clone()
                    .oneshot(request(Method::GET, "/api/state", None, None, None)),
            )
            .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response = block_on(app.clone().oneshot(request(
            Method::GET,
            "/api/state",
            Some("wrong-token"),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let response =
            block_on(app.oneshot(request(Method::GET, "/api/state", Some(TOKEN), None, None)))
                .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        let text = block_on(body_text(response));
        assert!(text.contains("\"port\":39420"), "{text}");
    }

    #[test]
    fn query_token_is_accepted_for_scripts() {
        let backend = FakeBackend::new();
        let app = app(backend);
        let uri = format!("/api/state?token={TOKEN}");
        let response = block_on(app.oneshot(request(Method::GET, &uri, None, None, None)))
            .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[test]
    fn mutations_check_origin_but_reads_do_not() {
        let backend = FakeBackend::new();
        let app = app(backend.clone());
        let body = r#"{"key":"cap/web"}"#;

        // 带一个不受信任的 Origin → 403，且根本没调 backend。
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/instances/start",
            Some(TOKEN),
            Some("http://evil.example"),
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(backend.calls.lock().unwrap().is_empty());

        // 同源 Origin → 放行。
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/instances/start",
            Some(TOKEN),
            Some("http://127.0.0.1:39420"),
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.calls.lock().unwrap().as_slice(), ["start:cap/web"]);

        // 没有 Origin（脚本 / 验收）→ 只认 token。
        let response = block_on(app.oneshot(request(
            Method::POST,
            "/api/instances/stop",
            Some(TOKEN),
            None,
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["start:cap/web", "stop:cap/web"]
        );
    }

    #[test]
    fn conflict_and_validation_statuses_pass_through() {
        let backend = FakeBackend::new();
        let app = app(backend);
        let response = block_on(app.oneshot(request(
            Method::POST,
            "/api/capsules/remove",
            Some(TOKEN),
            None,
            Some(r#"{"capsule_id":"cap","confirm":"wrong"}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = block_on(body_text(response));
        assert!(text.contains("确认文本"), "{text}");
    }

    #[test]
    fn m4_routes_reach_the_backend() {
        let backend = FakeBackend::new();
        let app = app(backend.clone());

        // 设置：合法 → 200；非法（采样间隔太小）→ 400。
        let ok_body = r#"{"gui_port":0,"sample_interval_ms":2000,"auto_open_browser":true,
            "tray_restart_max":3,"tray_restart_backoff_ms":[1000,5000],"log_retention_days":7}"#;
        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/settings",
            Some(TOKEN),
            None,
            Some(ok_body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        let text = block_on(body_text(response));
        assert!(text.contains("已保存设置"), "{text}");

        let bad_body = ok_body.replace("\"sample_interval_ms\":2000", "\"sample_interval_ms\":10");
        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/settings",
            Some(TOKEN),
            None,
            Some(&bad_body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        // 端口：0 → 400；正常值 → 200 且把 key/port 传给 backend。
        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/instances/port",
            Some(TOKEN),
            None,
            Some(r#"{"key":"cap/web","port":0}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/instances/port",
            Some(TOKEN),
            None,
            Some(r#"{"key":"cap/web","port":39416}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);

        // 实例 host（v0.3 / §3.22）：0.0.0.0 → 400；具体 IP → 200；空串 = 清除 → 200。
        for (body, status) in [
            (
                r#"{"key":"cap/web","host":"0.0.0.0"}"#,
                StatusCode::BAD_REQUEST,
            ),
            (
                r#"{"key":"cap/web","host":"192.168.3.192"}"#,
                StatusCode::OK,
            ),
            (r#"{"key":"cap/web","host":""}"#, StatusCode::OK),
        ] {
            let response = block_on(app.clone().oneshot(request(
                Method::PUT,
                "/api/instances/host",
                Some(TOKEN),
                None,
                Some(body),
            )))
            .expect("路由应返回响应");
            assert_eq!(response.status(), status, "{body}");
        }

        // 实例自启开关。
        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/instances/autostart",
            Some(TOKEN),
            None,
            Some(r#"{"key":"cap/web","autostart":true}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);

        // 自启动启用 / 停用（无请求体）。
        for path in ["/api/autostart/enable", "/api/autostart/disable"] {
            let response =
                block_on(
                    app.clone()
                        .oneshot(request(Method::POST, path, Some(TOKEN), None, None)),
                )
                .expect("路由应返回响应");
            assert_eq!(response.status(), StatusCode::OK, "{path}");
        }

        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            [
                "settings:0:2000",
                "port:cap/web:39416",
                "host:cap/web:192.168.3.192",
                "host:cap/web:",
                "autostart:cap/web:true",
                "autostart-enable",
                "autostart-disable",
            ]
        );
    }

    #[test]
    fn webhook_route_replaces_the_list_and_keeps_error_statuses() {
        let backend = FakeBackend::new();
        let app = app(backend.clone());

        let body = r#"{"webhooks":[{"url":"https://ntfy.example.test/_dsh","enabled":true,
            "start":{"enabled":true,"template":"start {alias}"},
            "stop":{"enabled":false,"template":""},
            "crashed":{"enabled":true,"template":"crash {alias}"}}]}"#;
        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/webhooks",
            Some(TOKEN),
            None,
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        let text = block_on(body_text(response));
        assert!(text.contains("已保存 webhook"), "{text}");

        // 空列表 = 全部关掉，合法。
        let response = block_on(app.clone().oneshot(request(
            Method::PUT,
            "/api/webhooks",
            Some(TOKEN),
            None,
            Some(r#"{"webhooks":[]}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);

        // 不合法的 URL → 400（校验在 core，这里验证状态码透传）。
        let bad = body.replace("https://ntfy.example.test/_dsh", "ftp://example.test/x");
        let response = block_on(app.oneshot(request(
            Method::PUT,
            "/api/webhooks",
            Some(TOKEN),
            None,
            Some(&bad),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let text = block_on(body_text(response));
        assert!(text.contains("http"), "{text}");

        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["webhooks:1", "webhooks:0", "webhooks:1"]
        );
    }

    #[test]
    fn state_exposes_settings_and_autostart_views() {
        let backend = FakeBackend::new();
        let app = app(backend);
        let response =
            block_on(app.oneshot(request(Method::GET, "/api/state", Some(TOKEN), None, None)))
                .expect("路由应返回响应");
        let text = block_on(body_text(response));
        assert!(text.contains("\"exe_integrity\":\"high\""), "{text}");
        assert!(text.contains("\"task_name\":\"dsh-housing\""), "{text}");
        assert!(text.contains("\"can_enable\":true"), "{text}");
    }

    #[test]
    fn logs_return_text_and_source_header() {
        let backend = FakeBackend::new();
        let app = app(backend.clone());
        let response = block_on(app.clone().oneshot(request(
            Method::GET,
            "/api/instances/logs?key=cap%2Fweb&tail=4096",
            Some(TOKEN),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers().get("x-log-source").unwrap(), "ring");
        let text = block_on(body_text(response));
        assert!(text.contains("log tail for cap/web"), "{text}");

        let response = block_on(app.clone().oneshot(request(
            Method::GET,
            "/api/instances/logs?key=missing%2Fweb",
            Some(TOKEN),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        // 读盘失败是 500：找不到实例（404）与内部错误（500）必须分得开（§3.17 错误码口径）。
        let response = block_on(app.oneshot(request(
            Method::GET,
            "/api/instances/logs?key=broken%2Fweb",
            Some(TOKEN),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let text = block_on(body_text(response));
        assert!(text.contains("读日志失败"), "{text}");
    }

    #[test]
    fn static_assets_are_served_and_traversal_is_rejected() {
        let backend = FakeBackend::new();
        let app = app(backend);

        let response = block_on(
            app.clone()
                .oneshot(request(Method::GET, "/", None, None, None)),
        )
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            response.headers().get(header::REFERRER_POLICY).unwrap(),
            "no-referrer"
        );

        let response =
            block_on(
                app.clone()
                    .oneshot(request(Method::GET, "/app.js", None, None, None)),
            )
            .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);

        for path in ["/../Cargo.toml", "/vendor/../../Cargo.toml", "/nope.html"] {
            let response =
                block_on(
                    app.clone()
                        .oneshot(request(Method::GET, path, None, None, None)),
                )
                .expect("路由应返回响应");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "path={path}");
        }

        // 未知 `/api/*`：没有 token 先吃 401（不泄漏「这个接口是否存在」），带了 token 才是 404。
        let response =
            block_on(
                app.clone()
                    .oneshot(request(Method::GET, "/api/nope", None, None, None)),
            )
            .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response =
            block_on(app.oneshot(request(Method::GET, "/api/nope", Some(TOKEN), None, None)))
                .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn favicon_is_served_from_the_shared_icon() {
        // v0.4 / #271：/favicon.ico 回的必须就是仓库根 assets/favicon.ico 那一份字节。
        let app = app(FakeBackend::new());
        let response =
            block_on(app.oneshot(request(Method::GET, "/favicon.ico", None, None, None)))
                .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            "image/x-icon"
        );
        let bytes = block_on(async {
            use http_body_util::BodyExt;
            response.into_body().collect().await.unwrap().to_bytes()
        });
        assert_eq!(bytes.as_ref(), crate::assets::FAVICON);
    }

    #[test]
    fn lan_origins_are_trusted_only_when_the_console_binds_off_loopback() {
        // v0.4 / #286：绑定 0.0.0.0 时，页面是从 LAN 地址打开的，它的 Origin 必须在白名单里。
        let backend = FakeBackend::new();
        let mut config = ConsoleConfig::on("0.0.0.0:39420".parse().unwrap(), TOKEN);
        config.origin_hosts = vec!["192.168.3.192".parse().unwrap()];
        let lan_app = router(config, backend.clone());

        let body = r#"{"key":"cap/web"}"#;
        let response = block_on(lan_app.clone().oneshot(request(
            Method::POST,
            "/api/instances/start",
            Some(TOKEN),
            Some("http://192.168.3.192:39420"),
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.calls.lock().unwrap().as_slice(), ["start:cap/web"]);

        // 陌生 Origin 仍然 403。
        let response = block_on(lan_app.oneshot(request(
            Method::POST,
            "/api/instances/stop",
            Some(TOKEN),
            Some("http://evil.example"),
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        // 回环档（默认）不信任任何 LAN Origin：白名单跟着绑定地址走。
        let loopback_backend = FakeBackend::new();
        let loopback_app = router(
            ConsoleConfig::loopback(39420, TOKEN),
            loopback_backend.clone(),
        );
        let response = block_on(loopback_app.oneshot(request(
            Method::POST,
            "/api/instances/start",
            Some(TOKEN),
            Some("http://192.168.3.192:39420"),
            Some(body),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(loopback_backend.calls.lock().unwrap().is_empty());
    }

    // ── v0.5 / #249：构建器路由（§3.24） ──

    #[test]
    fn builder_routes_require_token_and_forward_to_backend() {
        let backend = FakeBackend::new();
        let app = app(backend.clone());

        // 不带 token → 401（且后端一次都没被调到）。
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/builder/start",
            None,
            None,
            Some(r#"{"name":"0.2.1-rc.3-demo","dsh":"0.2.1-rc.3","node":"v24.22.0"}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(backend.calls.lock().unwrap().is_empty());

        // 带 token → 200，后端记录一次 start。
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/builder/start",
            Some(TOKEN),
            None,
            Some(
                r#"{"name":"0.2.1-rc.3-demo","dsh":"0.2.1-rc.3","node":"v24.22.0","dry_run":true}"#,
            ),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        let text = block_on(body_text(response));
        assert!(text.contains("已开始构建"), "{text}");
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["builder-start:0.2.1-rc.3-demo".to_string()]
        );

        // 缺必填字段 → 422（axum 的 Json 提取失败）。
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/builder/start",
            Some(TOKEN),
            None,
            Some(r#"{"name":"x"}"#),
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        // cancel。
        let response = block_on(app.clone().oneshot(request(
            Method::POST,
            "/api/builder/cancel",
            Some(TOKEN),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(backend
            .calls
            .lock()
            .unwrap()
            .contains(&"builder-cancel".to_string()));
    }

    #[test]
    fn builder_log_returns_plain_text_with_source_header() {
        let app = app(FakeBackend::new());
        let response = block_on(app.clone().oneshot(request(
            Method::GET,
            "/api/builder/log?tail=4096",
            Some(TOKEN),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-log-source").unwrap(),
            "ring",
            "构建日志与实例日志共用一套来源头"
        );
        let text = block_on(body_text(response));
        assert!(text.contains("[builder]"), "{text}");
    }

    #[test]
    fn builder_versions_returns_catalog_and_passes_refresh() {
        let backend = FakeBackend::new();
        let app = app(backend.clone());
        let response = block_on(app.clone().oneshot(request(
            Method::GET,
            "/api/builder/versions?refresh=1",
            Some(TOKEN),
            None,
            None,
        )))
        .expect("路由应返回响应");
        assert_eq!(response.status(), StatusCode::OK);
        let text = block_on(body_text(response));
        assert!(text.contains("\"dsh_tags\""), "{text}");
        assert!(text.contains("\"node_versions\""), "{text}");
        assert!(text.contains("0.2.1-rc.3"), "{text}");
        assert!(
            text.contains("\"fetched_at\":1700000000"),
            "refresh 应传给后端：{text}"
        );
        assert!(text.contains("\"default_profile\":\"web\""), "{text}");
    }
}
