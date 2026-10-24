// dsh-housing 控制台前端（无构建 Vue 3，docs/structure.md §3.17 / §3.20）。
//
// - 模板写在 index.html 里（Vue 的浏览器构建在运行时编译它），这里只放数据与方法；
// - token 来自地址栏 `?token=`，读进 sessionStorage 后立刻从地址栏抹掉；
// - 所有 API 请求都带 `X-Console-Token`；轮询 /api/state，日志面板打开时轮询日志尾部；
// - 主界面只有实例管理器；「管理 dsh 版本」与「设置」是顶栏图标唤出的悬浮窗（`panel` 状态）。

import { createApp } from './vendor/vue.esm-browser.prod.js';

const TOKEN_KEY = 'dshHousingConsoleToken';
const urlToken = new URLSearchParams(window.location.search).get('token');
if (urlToken) {
  sessionStorage.setItem(TOKEN_KEY, urlToken);
  history.replaceState(null, '', window.location.pathname);
}
const token = sessionStorage.getItem(TOKEN_KEY) || '';

const STATE_INTERVAL_MS = 2000;
const LOG_INTERVAL_MS = 1000;
const LOG_TAIL_BYTES = 64 * 1024;

async function requestJson(path, options = {}) {
  const headers = { 'X-Console-Token': token };
  let body;
  if (options.body !== undefined) {
    headers['Content-Type'] = 'application/json';
    body = JSON.stringify(options.body);
  }
  const response = await fetch(path, { method: options.method || 'GET', headers, body });
  const text = await response.text();
  let data = null;
  if (text) {
    try {
      data = JSON.parse(text);
    } catch (_error) {
      data = null;
    }
  }
  if (!response.ok) {
    const message = (data && (data.error || data.detail)) || text || `HTTP ${response.status}`;
    throw new Error(message);
  }
  return data;
}

async function requestLogs(key) {
  const headers = { 'X-Console-Token': token };
  const response = await fetch(
    `/api/instances/logs?key=${encodeURIComponent(key)}&tail=${LOG_TAIL_BYTES}`,
    { headers },
  );
  const text = await response.text();
  if (!response.ok) {
    throw new Error(text || `HTTP ${response.status}`);
  }
  return { text, source: response.headers.get('x-log-source') || '' };
}

function envToRows(env) {
  return Object.entries(env || {}).map(([key, value]) => ({ key, value }));
}

function rowsToEnv(rows) {
  const env = {};
  const seen = new Set();
  for (const row of rows) {
    const key = (row.key || '').trim();
    if (!key) {
      continue;
    }
    if (seen.has(key)) {
      throw new Error(`变量名重复：${key}`);
    }
    seen.add(key);
    env[key] = row.value == null ? '' : String(row.value);
  }
  return env;
}

// M4（§3.19）：这些环境变量由启动器自己按登记项算出来，改了不会改变实际行为，必须就地说明。
const DERIVED_ENV = {
  DSH_WEB_PORT:
    'DSH_WEB_PORT 由实例的「端口」字段决定（命令行 --port），dsh 0.2.x 不读这个变量；改端口请用上面的「改端口」。',
};

// 草稿与 `state` 必须是两份独立对象：用户改草稿时轮询不能把它冲掉（纯数据，JSON 克隆足够）。
function cloneJson(value) {
  return value == null ? value : JSON.parse(JSON.stringify(value));
}

function settingsToDraft(settings) {
  return {
    gui_port: settings.gui_port,
    console_host: settings.console_host || '127.0.0.1',
    sample_interval_ms: settings.sample_interval_ms,
    auto_open_browser: settings.auto_open_browser,
    auto_open_console: settings.auto_open_console !== false,
    tray_restart_max: settings.tray_restart_max,
    tray_restart_backoff_text: (settings.tray_restart_backoff_ms || []).join(','),
    log_retention_days: settings.log_retention_days,
  };
}

function parseBackoff(text) {
  const items = String(text || '')
    .split(',')
    .map((item) => item.trim())
    .filter((item) => item.length > 0)
    .map((item) => Number(item));
  if (items.length === 0) {
    throw new Error('托盘重启退避至少要填一项（毫秒）');
  }
  for (const item of items) {
    if (!Number.isFinite(item) || item < 0) {
      throw new Error('托盘重启退避必须是毫秒数（逗号分隔）');
    }
  }
  return items;
}

createApp({
  data() {
    return {
      state: null,
      loading: false,
      error: '',
      notice: '',
      errorTimer: null,
      pollTimer: null,
      logTimer: null,
      logKey: '',
      logText: '',
      logSource: '',
      // v0.2（§3.20）：顶栏图标唤出的悬浮窗，同一时刻只开一个；'' = 全部关闭。
      panel: '',
      envOpen: {},
      envDrafts: {},
      aliasDrafts: {},
      // M4：端口编辑 / 壳设置 / 自启动
      portOpen: {},
      portDrafts: {},
      // v0.3 / #285：实例 host 编辑（空串 = 跟随胶囊配置）
      hostOpen: {},
      hostDrafts: {},
      settingsDraft: null,
      // v0.2 / #244：webhook 草稿（null = 还没从 /api/state 灌过）
      webhookDrafts: null,
      triggerMeta: [
        { key: 'start', label: '启动（实例就绪）' },
        { key: 'stop', label: '停止' },
        { key: 'crashed', label: '崩溃（自行退出）' },
        { key: 'housing_start', label: 'Housing 启动（控制台就绪）' },
      ],
      // key → 保存时的 pid：pid 变了（重启完成）就把「重启后生效」撤掉。
      pendingRestart: {},
      removeConfirm: {},
      registerPath: '',
      // v0.5 / #249：构建器（§3.24）——草稿、版本目录、日志
      builderDraft: null,
      builderVersions: { dsh_tags: {}, dsh_versions: [], node_versions: [], profiles: [] },
      builderLog: '',
      builderLogSource: '',
      builderLogTimer: null,
      busy: false,
    };
  },

  computed: {
    instances() {
      return (this.state && this.state.status && this.state.status.instances) || [];
    },
    capsules() {
      return (this.state && this.state.registry && this.state.registry.capsules) || [];
    },
    trayOnline() {
      return Boolean(this.state && this.state.status && this.state.status.tray_running);
    },
    lanHost() {
      return (this.state && this.state.settings && this.state.settings.lan_ip) || '';
    },
    // v0.4 / #286：草稿里选了非回环地址（提交后仍要重启 core 才生效）。
    consoleExposed() {
      const host = this.settingsDraft && this.settingsDraft.console_host;
      return Boolean(host) && host !== '127.0.0.1' && host !== 'localhost';
    },
    // 页面的 token 本来就在 sessionStorage 里，这里只是把它拼成局域网设备能直接打开的链接。
    consoleLanUrl() {
      if (!this.consoleExposed || !this.lanHost) {
        return '';
      }
      const port = this.state && this.state.settings && this.state.settings.console_port;
      if (!port) {
        return '';
      }
      return `http://${this.lanHost}:${port}/?token=${token}`;
    },
    autostartLabel() {
      const labels = {
        enabled: '已启用',
        disabled: '已停用',
        missing: '未创建',
        foreign: '指向别的程序',
        error: '查询失败',
      };
      const view = this.state && this.state.autostart;
      if (!view) {
        return '—';
      }
      return labels[view.state] || view.state;
    },
    builderState() {
      return (this.state && this.state.builder) || null;
    },
    builderRunning() {
      const phase = this.builderState && this.builderState.phase;
      return phase === 'running' || phase === 'registering';
    },
    builderResult() {
      return (this.builderState && this.builderState.result) || null;
    },
    builderBadge() {
      const labels = {
        idle: '未开始',
        running: '构建中',
        registering: '登记中',
        done: '已完成',
        failed: '失败',
        canceled: '已取消',
      };
      const phase = (this.builderState && this.builderState.phase) || 'idle';
      return labels[phase] || phase;
    },
    builderLogSourceLabel() {
      if (this.builderLogSource === 'ring') {
        return '（实时）';
      }
      if (this.builderLogSource === 'file') {
        return '（日志文件）';
      }
      return '';
    },
    profileOptions() {
      const profiles = (this.builderVersions && this.builderVersions.profiles) || [];
      return profiles.length ? profiles : ['web', 'headless', 'acp', 'sdk', 'sdk-minimal'];
    },
    versionsHint() {
      const versions = this.builderVersions || {};
      if (!versions.fetched_at) {
        return '还没抓到，可手填';
      }
      const when = new Date(versions.fetched_at * 1000).toLocaleString();
      return versions.stale ? `${when}（已过时，可点刷新）` : when;
    },
  },

  mounted() {
    this.refresh();
    this.pollTimer = window.setInterval(() => {
      if (!document.hidden) {
        this.refresh(true);
      }
    }, STATE_INTERVAL_MS);
    // 悬浮窗的键盘出口：Esc 关闭（点击遮罩 / 右上 ✕ 是另外两条路）。
    this.onKeydown = (event) => {
      if (event.key === 'Escape' && this.panel) {
        this.closePanel();
      }
    };
    window.addEventListener('keydown', this.onKeydown);
  },

  beforeUnmount() {
    window.clearInterval(this.pollTimer);
    window.clearInterval(this.logTimer);
    window.clearInterval(this.builderLogTimer);
    window.removeEventListener('keydown', this.onKeydown);
  },

  methods: {
    async refresh(silent = false) {
      if (!silent) {
        this.loading = true;
      }
      try {
        const state = await requestJson('/api/state');
        this.state = state;
        this.syncDrafts();
      } catch (error) {
        if (!silent) {
          this.showError(this.explain(error));
        }
      } finally {
        this.loading = false;
      }
    },

    syncDrafts() {
      // 壳设置只在第一次拿到 state 时灌进草稿：之后用户正在编辑，轮询不能覆盖。
      if (!this.settingsDraft && this.state && this.state.settings) {
        this.settingsDraft = settingsToDraft(this.state.settings);
      }
      // webhook 同理：只灌一次，用户改到一半时轮询不能把它冲掉。
      if (this.webhookDrafts === null && this.state && this.state.webhooks) {
        this.webhookDrafts = cloneJson(this.state.webhooks.items);
      }
      for (const instance of this.instances) {
        if (!(instance.key in this.aliasDrafts)) {
          this.aliasDrafts[instance.key] = instance.alias || '';
        }
        // 重启后生效的提示：pid 变了（或实例停了）就撤掉。
        if (this.pendingRestart[instance.key] !== undefined) {
          const savedPid = this.pendingRestart[instance.key];
          if (instance.pid !== savedPid) {
            delete this.pendingRestart[instance.key];
          }
        }
      }
      if (this.logKey && !this.instances.some((instance) => instance.key === this.logKey)) {
        this.closeLogs();
      }
    },

    explain(error) {
      const message = (error && error.message) || String(error);
      if (/consoleToken|401/i.test(message)) {
        return '令牌缺失或已失效：请从托盘菜单「打开控制台…」重新打开本页。';
      }
      return message;
    },

    showError(message) {
      this.error = message;
      window.clearTimeout(this.errorTimer);
      this.errorTimer = window.setTimeout(() => {
        this.error = '';
      }, 8000);
    },

    showNotice(message) {
      this.notice = message;
      window.setTimeout(() => {
        if (this.notice === message) {
          this.notice = '';
        }
      }, 4000);
    },

    // ── v0.2（§3.20）：顶栏悬浮窗 ──

    togglePanel(name) {
      this.panel = this.panel === name ? '' : name;
    },

    closePanel() {
      this.panel = '';
      this.stopBuilderLogPolling();
    },

    configFor(key) {
      for (const capsule of this.capsules) {
        for (const config of capsule.instances) {
          if (`${capsule.capsule_id}/${config.profile}` === key) {
            return config;
          }
        }
      }
      return null;
    },

    stateLabel(state) {
      const labels = {
        stopped: '已停止',
        starting: '启动中',
        running: '运行中',
        stopping: '停止中',
        failed: '失败',
      };
      return labels[state] || state;
    },

    canStart(instance) {
      return instance.state === 'stopped' || instance.state === 'failed';
    },

    canStop(instance) {
      return instance.state === 'starting' || instance.state === 'running';
    },

    formatBytes(bytes) {
      if (!bytes) {
        return '0';
      }
      const units = ['B', 'KB', 'MB', 'GB', 'TB'];
      let value = Number(bytes);
      let index = 0;
      while (value >= 1024 && index < units.length - 1) {
        value /= 1024;
        index += 1;
      }
      return `${value.toFixed(value >= 100 ? 0 : 1)} ${units[index]}`;
    },

    formatCpu(percent) {
      return `${Number(percent || 0).toFixed(1)}%`;
    },

    async perform(notice, work) {
      this.busy = true;
      try {
        const result = await work();
        if (result && result.notice) {
          this.showNotice(result.notice);
        } else if (notice) {
          this.showNotice(notice);
        }
        await this.refresh(true);
        return result;
      } catch (error) {
        this.showError(this.explain(error));
        return null;
      } finally {
        this.busy = false;
      }
    },

    async startInstance(instance) {
      await this.perform(`已请求启动 ${instance.key}`, () =>
        requestJson('/api/instances/start', { method: 'POST', body: { key: instance.key } }));
    },

    async stopInstance(instance) {
      await this.perform(`已请求停止 ${instance.key}`, () =>
        requestJson('/api/instances/stop', { method: 'POST', body: { key: instance.key } }));
    },

    async openDsh(instance) {
      await this.perform('已在默认浏览器打开 dsh 页', () =>
        requestJson('/api/instances/open', { method: 'POST', body: { key: instance.key } }));
    },

    async saveAlias(instance) {
      const alias = (this.aliasDrafts[instance.key] || '').trim();
      const result = await this.perform('已保存别名', () =>
        requestJson('/api/instances/alias', {
          method: 'PUT',
          body: { key: instance.key, alias },
        }));
      if (result) {
        // 服务端回的是「有效显示名」：清空别名 = 回到默认名，输入框要重新跟着服务端走
        // （否则输入框空白、卡片却是另一个名字）。
        delete this.aliasDrafts[instance.key];
        this.syncDrafts();
      }
    },

    toggleEnv(instance) {
      const open = !this.envOpen[instance.key];
      this.envOpen[instance.key] = open;
      if (open && !this.envDrafts[instance.key]) {
        const config = this.configFor(instance.key);
        this.envDrafts[instance.key] = envToRows(config ? config.env : {});
      }
    },

    addEnvRow(instance) {
      if (!this.envDrafts[instance.key]) {
        this.envDrafts[instance.key] = [];
      }
      this.envDrafts[instance.key].push({ key: '', value: '' });
    },

    removeEnvRow(instance, index) {
      this.envDrafts[instance.key].splice(index, 1);
    },

    async saveEnv(instance) {
      let env;
      try {
        env = rowsToEnv(this.envDrafts[instance.key] || []);
      } catch (error) {
        this.showError(this.explain(error));
        return;
      }
      const wasRunning = this.canStop(instance);
      const result = await this.perform('已保存 env', () =>
        requestJson('/api/instances/env', {
          method: 'PUT',
          body: { key: instance.key, env },
        }));
      if (result && wasRunning) {
        this.pendingRestart[instance.key] = instance.pid;
      }
    },

    // ── M4：端口 / 自启 / 壳设置 ──

    derivedEnvWarning(instance) {
      const rows = this.envDrafts[instance.key] || [];
      const hit = rows.find(
        (row) => DERIVED_ENV[String(row.key || '').trim().toUpperCase()],
      );
      return hit ? DERIVED_ENV[String(hit.key).trim().toUpperCase()] : '';
    },

    togglePort(instance) {
      const open = !this.portOpen[instance.key];
      this.portOpen[instance.key] = open;
      if (open) {
        const config = this.configFor(instance.key);
        this.portDrafts[instance.key] = config ? config.port : instance.port;
      }
    },

    async savePort(instance) {
      const port = Number(this.portDrafts[instance.key]);
      if (!Number.isInteger(port) || port < 1 || port > 65535) {
        this.showError('端口必须是 1–65535 的整数');
        return;
      }
      await this.perform('已保存端口', () =>
        requestJson('/api/instances/port', {
          method: 'PUT',
          body: { key: instance.key, port },
        }));
    },

    toggleHost(instance) {
      const open = !this.hostOpen[instance.key];
      this.hostOpen[instance.key] = open;
      if (open) {
        const config = this.configFor(instance.key);
        this.hostDrafts[instance.key] = (config && config.host) || '';
      }
    },

    useLoopbackHost(instance) {
      this.hostDrafts[instance.key] = '127.0.0.1';
    },

    useLanHost(instance) {
      if (this.lanHost) {
        this.hostDrafts[instance.key] = this.lanHost;
      }
    },

    async saveHost(instance) {
      const host = (this.hostDrafts[instance.key] || '').trim();
      await this.perform(host ? `host 将设为 ${host}` : '已清除 host', () =>
        requestJson('/api/instances/host', {
          method: 'PUT',
          body: { key: instance.key, host },
        }));
    },

    async toggleAutostart(instance, checked) {
      await this.perform(checked ? '已标记为登录自启' : '已取消登录自启', () =>
        requestJson('/api/instances/autostart', {
          method: 'PUT',
          body: { key: instance.key, autostart: Boolean(checked) },
        }));
    },

    async saveSettings() {
      const draft = this.settingsDraft;
      if (!draft) {
        return;
      }
      let backoff;
      try {
        backoff = parseBackoff(draft.tray_restart_backoff_text);
      } catch (error) {
        this.showError(this.explain(error));
        return;
      }
      const body = {
        gui_port: Number(draft.gui_port) || 0,
        console_host: String(draft.console_host || '').trim(),
        sample_interval_ms: Number(draft.sample_interval_ms),
        auto_open_browser: Boolean(draft.auto_open_browser),
        auto_open_console: Boolean(draft.auto_open_console),
        tray_restart_max: Number(draft.tray_restart_max),
        tray_restart_backoff_ms: backoff,
        log_retention_days: Number(draft.log_retention_days),
      };
      await this.perform('已保存设置', () =>
        requestJson('/api/settings', { method: 'PUT', body }));
    },

    // v0.4 / #286：把「局域网设备能直接打开」的链接交给用户（剪贴板不可用时退回 prompt）。
    async copyLanConsoleUrl() {
      const url = this.consoleLanUrl;
      if (!url) {
        return;
      }
      try {
        await navigator.clipboard.writeText(url);
        this.showNotice('局域网控制台链接已复制');
      } catch (_error) {
        window.prompt('复制这个链接（Ctrl+C）', url);
      }
    },

    // ── v0.5 / #249：构建可移植版本（§3.24） ──

    toggleBuilder() {
      const open = this.panel !== 'builder';
      this.panel = open ? 'builder' : '';
      if (open) {
        if (!this.builderDraft) {
          this.resetBuilderDraft();
        }
        this.loadVersions(false);
        this.startBuilderLogPolling();
      } else {
        this.stopBuilderLogPolling();
      }
    },

    resetBuilderDraft() {
      const versions = this.builderVersions || {};
      const tags = versions.dsh_tags || {};
      this.builderDraft = {
        name: '',
        dsh: tags.next || tags.latest || '',
        node: (versions.node_versions && versions.node_versions[0]) || '',
        profile: versions.default_profile || 'web',
        port: null,
        root: versions.default_root || '',
        overwrite: false,
        dry_run: false,
      };
    },

    async loadVersions(refresh) {
      try {
        const versions = await requestJson(
          `/api/builder/versions${refresh ? '?refresh=1' : ''}`,
        );
        this.builderVersions = versions;
        if (!this.builderDraft) {
          this.resetBuilderDraft();
          return;
        }
        // 只补空字段：用户已经填的内容不能被轮询 / 刷新冲掉。
        const tags = versions.dsh_tags || {};
        if (!this.builderDraft.dsh && (tags.next || tags.latest)) {
          this.builderDraft.dsh = tags.next || tags.latest;
        }
        if (!this.builderDraft.node && versions.node_versions && versions.node_versions.length) {
          this.builderDraft.node = versions.node_versions[0];
        }
        if (!this.builderDraft.root && versions.default_root) {
          this.builderDraft.root = versions.default_root;
        }
      } catch (error) {
        this.showError(this.explain(error));
      }
    },

    async startBuild() {
      const draft = this.builderDraft;
      if (!draft) {
        return;
      }
      if (!String(draft.name || '').trim() || !String(draft.dsh || '').trim() || !String(draft.node || '').trim()) {
        this.showError('目录名 / dsh 版本 / node 版本都要填');
        return;
      }
      const body = {
        name: String(draft.name).trim(),
        dsh: String(draft.dsh).trim(),
        node: String(draft.node).trim(),
        profile: draft.profile,
        overwrite: Boolean(draft.overwrite),
        dry_run: Boolean(draft.dry_run),
      };
      if (draft.port) {
        body.port = Number(draft.port);
      }
      if (draft.root && String(draft.root).trim()) {
        body.root = String(draft.root).trim();
      }
      this.builderLog = '';
      this.builderLogSource = '';
      const result = await this.perform('已开始构建', () =>
        requestJson('/api/builder/start', { method: 'POST', body }));
      if (result) {
        this.startBuilderLogPolling();
      }
    },

    async cancelBuild() {
      await this.perform('已请求取消构建', () =>
        requestJson('/api/builder/cancel', { method: 'POST' }));
    },

    startBuilderLogPolling() {
      window.clearInterval(this.builderLogTimer);
      this.fetchBuilderLog();
      this.builderLogTimer = window.setInterval(() => {
        if (!document.hidden && this.panel === 'builder') {
          this.fetchBuilderLog(true);
        }
      }, LOG_INTERVAL_MS);
    },

    stopBuilderLogPolling() {
      window.clearInterval(this.builderLogTimer);
      this.builderLogTimer = null;
    },

    async fetchBuilderLog(silent = false) {
      try {
        const headers = { 'X-Console-Token': token };
        const response = await fetch(`/api/builder/log?tail=${LOG_TAIL_BYTES}`, { headers });
        if (response.status === 404) {
          this.builderLog = '';
          this.builderLogSource = '';
          return;
        }
        const text = await response.text();
        if (!response.ok) {
          throw new Error(text || `HTTP ${response.status}`);
        }
        this.builderLog = text;
        this.builderLogSource = response.headers.get('x-log-source') || '';
        this.$nextTick(() => {
          const pane = document.querySelector('.builder-log');
          if (pane) {
            pane.scrollTop = pane.scrollHeight;
          }
        });
      } catch (error) {
        if (!silent) {
          this.showError(this.explain(error));
        }
      }
    },

    formatElapsed(ms) {
      const total = Math.max(0, Math.round(Number(ms || 0) / 1000));
      const minutes = Math.floor(total / 60);
      const seconds = total % 60;
      return minutes > 0 ? `${minutes} 分 ${seconds} 秒` : `${seconds} 秒`;
    },

    // ── v0.2 / #244：webhook ──

    hookUrlOk(hook) {
      const url = (hook.url || '').trim();
      if (!url) {
        return false;
      }
      return /^https?:\/\//i.test(url);
    },

    addWebhook() {
      // 新建时带上三个条件的默认模板（默认模板的唯一真源在 core / webhook crate，前端只取用）。
      const defaults = (this.state && this.state.webhooks && this.state.webhooks.defaults) || {};
      const trigger = (key) => ({
        enabled: true,
        template: defaults[key] || '',
      });
      if (!this.webhookDrafts) {
        this.webhookDrafts = [];
      }
      this.webhookDrafts.push({
        url: '',
        enabled: true,
        start: trigger('start'),
        stop: trigger('stop'),
        crashed: trigger('crashed'),
        housing_start: trigger('housing_start'),
      });
    },

    removeWebhook(index) {
      if (!this.webhookDrafts) {
        return;
      }
      this.webhookDrafts.splice(index, 1);
    },

    resetTemplate(hook, key) {
      const defaults = (this.state && this.state.webhooks && this.state.webhooks.defaults) || {};
      if (hook[key] && defaults[key] !== undefined) {
        hook[key].template = defaults[key];
      }
    },

    async saveWebhooks() {
      if (!this.webhookDrafts) {
        return;
      }
      const hooks = this.webhookDrafts.map((hook) => ({
        url: (hook.url || '').trim(),
        enabled: Boolean(hook.enabled),
        start: {
          enabled: Boolean(hook.start && hook.start.enabled),
          template: (hook.start && hook.start.template) || '',
        },
        stop: {
          enabled: Boolean(hook.stop && hook.stop.enabled),
          template: (hook.stop && hook.stop.template) || '',
        },
        crashed: {
          enabled: Boolean(hook.crashed && hook.crashed.enabled),
          template: (hook.crashed && hook.crashed.template) || '',
        },
        housing_start: {
          enabled: Boolean(hook.housing_start && hook.housing_start.enabled),
          template: (hook.housing_start && hook.housing_start.template) || '',
        },
      }));
      const bad = hooks.find((hook) => !/^https?:\/\//i.test(hook.url));
      if (bad) {
        this.showError(`URL 必须是 http:// 或 https:// 开头：${bad.url || '（空）'}`);
        return;
      }
      const result = await this.perform('已保存 webhook', () =>
        requestJson('/api/webhooks', { method: 'PUT', body: { webhooks: hooks } }));
      if (result && result.state && result.state.webhooks) {
        // 服务端去空白 / 补默认值之后是权威形状：保存成功就用它重新灌草稿。
        this.webhookDrafts = cloneJson(result.state.webhooks.items);
      }
    },

    async enableAutostart() {      await this.perform('已启用登录自启动', () =>
        requestJson('/api/autostart/enable', { method: 'POST' }));
    },

    async disableAutostart() {
      await this.perform('已停用登录自启动', () =>
        requestJson('/api/autostart/disable', { method: 'POST' }));
    },

    async toggleLogs(instance) {      if (this.logKey === instance.key) {
        this.closeLogs();
        return;
      }
      this.logKey = instance.key;
      this.logText = '';
      this.logSource = '';
      await this.fetchLogs();
      window.clearInterval(this.logTimer);
      this.logTimer = window.setInterval(() => {
        if (!document.hidden && this.logKey) {
          this.fetchLogs(true);
        }
      }, LOG_INTERVAL_MS);
    },

    closeLogs() {
      window.clearInterval(this.logTimer);
      this.logTimer = null;
      this.logKey = '';
      this.logText = '';
      this.logSource = '';
    },

    async fetchLogs(silent = false) {
      if (!this.logKey) {
        return;
      }
      try {
        const { text, source } = await requestLogs(this.logKey);
        this.logText = text;
        this.logSource = source;
      } catch (error) {
        if (!silent) {
          this.showError(this.explain(error));
        }
      }
    },

    async registerCapsule() {
      const root = this.registerPath.trim();
      if (!root) {
        return;
      }
      const result = await this.perform('已登记', () =>
        requestJson('/api/capsules/register', { method: 'POST', body: { root } }));
      if (result && result.ok) {
        this.registerPath = '';
      }
    },

    async removeCapsule(capsule) {
      const confirm = this.removeConfirm[capsule.capsule_id] || '';
      if (confirm !== capsule.capsule_id) {
        this.showError('确认文本与胶囊标识不一致');
        return;
      }
      const result = await this.perform(`已移除 ${capsule.capsule_id}`, () =>
        requestJson('/api/capsules/remove', {
          method: 'POST',
          body: { capsule_id: capsule.capsule_id, confirm },
        }));
      if (result && result.ok) {
        delete this.removeConfirm[capsule.capsule_id];
      }
    },
  },
}).mount('#app');
