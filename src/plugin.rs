//! 数据插件: 以 plugins/*.json 声明式定义外部数据源, 周期抓取并把结果
//! 以 `{{PLUGIN.<插件名>.<变量>}}` 暴露给 OBS 页面(见 /api/state 的 plugins 字段)。
//! 用法与完整示例见 docs/plugins.md。
//!
//! 插件目录查找: exe 同级 plugins/ 优先, 再逐级向上(运行仓库内构建产物时
//! 命中源码 plugins/); 同名插件先找到的优先。
//!
//! 数据源: url 以 `file:` 开头读本地文件(相对 exe, 周期最小 1 秒), 否则按 HTTP 抓取。
//! 提取类型: `regex` 取第一个匹配的捕获组; `count` 统计全部匹配条数。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::{sleep, Duration};

use crate::monitor::MonitorConfig;
use crate::state::AppState;

fn default_ua() -> String {
    "curl/8.5.0".to_string()
}
fn default_true() -> bool {
    true
}
fn default_interval() -> u64 {
    1800
}
fn default_retry() -> u64 {
    300
}

#[derive(Debug, Deserialize, Clone)]
pub struct PluginDef {
    pub name: String,
    /// HTTP 抓取地址; 以 `file:` 开头则读本地文件(相对 exe, 如 file:data.log)
    pub url: String,
    #[serde(default = "default_ua")]
    pub user_agent: String,
    /// 直连不走系统代理(部分源在系统代理下反而连不上)
    #[serde(default = "default_true")]
    pub no_proxy: bool,
    #[serde(default = "default_interval")]
    pub interval_sec: u64,
    /// 429 限流后的重试间隔(秒)
    #[serde(default = "default_retry")]
    pub retry_sec: u64,
    #[serde(default)]
    pub extract: Vec<Extractor>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct Extractor {
    /// 变量名, 页面以 {{PLUGIN.<插件名>.<变量>}} 引用
    pub var: String,
    /// regex | count
    #[serde(rename = "type")]
    pub kind: String,
    /// 正则
    #[serde(default)]
    pub pattern: String,
}

/// 候选插件目录: exe 同级 plugins/ 起逐级向上(先找到的优先)
fn plugin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    let mut dir = std::env::current_exe().unwrap_or_default();
    dir.pop();
    loop {
        let cand = dir.join("plugins");
        if cand.is_dir() {
            dirs.push(cand);
        }
        if !dir.pop() {
            return dirs;
        }
    }
}

/// 载入全部插件定义(仅启动时读取一次, 增删改插件文件需重启程序);
/// 同名插件以先找到的目录为准
fn load_defs() -> Vec<PluginDef> {
    let mut defs: Vec<PluginDef> = Vec::new();
    for dir in plugin_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read_to_string(&p) {
                Ok(body) => match serde_json::from_str::<PluginDef>(&body) {
                    Ok(d) if !d.name.is_empty() && !d.extract.is_empty() => {
                        if defs.iter().any(|x| x.name == d.name) {
                            continue;
                        }
                        tracing::info!(plugin = %d.name, path = %p.display(), "插件已加载");
                        defs.push(d);
                    }
                    Ok(_) => tracing::warn!(path = %p.display(), "插件缺少 name/extract, 忽略"),
                    Err(err) => tracing::warn!(path = %p.display(), error = %err, "插件定义解析失败"),
                },
                Err(err) => tracing::warn!(path = %p.display(), error = %err, "插件文件读取失败"),
            }
        }
    }
    defs
}

/// 启动全部插件任务
pub fn spawn_all(state: Arc<AppState>, cfg_rx: watch::Receiver<Arc<MonitorConfig>>) {
    for def in load_defs() {
        tokio::spawn(run_plugin(def, Arc::clone(&state), cfg_rx.clone()));
    }
}

/// file: 源路径(相对 exe); 非 file: url 返回 None
fn file_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file:")?;
    let mut p = std::env::current_exe().unwrap_or_default();
    p.pop();
    Some(p.join(rest))
}

async fn run_plugin(
    def: PluginDef,
    state: Arc<AppState>,
    mut cfg_rx: watch::Receiver<Arc<MonitorConfig>>,
) {
    let is_file = def.url.starts_with("file:");
    let client = if is_file {
        None
    } else {
        let mut builder = reqwest::Client::builder()
            .user_agent(&def.user_agent)
            .danger_accept_invalid_certs(true);
        if def.no_proxy {
            builder = builder.no_proxy();
        }
        match builder.build() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(plugin = %def.name, error = %e, "插件 client 构建失败");
                return;
            }
        }
    };
    let mut sim: HashMap<String, i64> = HashMap::new();
    loop {
        let (mock_out, sid) = {
            let c = cfg_rx.borrow();
            (c.mock && c.mock_plugins, c.steam_id.trim().to_string())
        };
        // 模拟输出: 不真实抓取, 各变量输出随机递增的模拟值(便于调试前端展示/动画)
        if mock_out {
            let ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let mut vals: HashMap<String, String> = HashMap::new();
            for ex in &def.extract {
                let v = sim
                    .entry(ex.var.clone())
                    .or_insert(((ns % 300 + ex.var.len() as u32 * 37) % 400 + 100) as i64);
                // 每周期约 40% 概率 +1, 各变量按名哈希去相关
                let h = ex
                    .var
                    .bytes()
                    .fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
                if (ns ^ h.wrapping_mul(2654435761)) % 5 < 2 {
                    *v += 1;
                }
                vals.insert(ex.var.clone(), v.to_string());
            }
            state
                .plugins
                .lock()
                .await
                .insert(def.name.clone(), vals);
            tokio::select! {
                _ = sleep(Duration::from_secs(5)) => {}
                _ = cfg_rx.changed() => {}
            }
            continue;
        }
        if !is_file && sid.is_empty() {
            tokio::select! {
                _ = sleep(Duration::from_secs(60)) => {}
                _ = cfg_rx.changed() => {}
            }
            continue;
        }
        let url = def.url.replace("{steam_id}", &sid);
        // 本地文件读取廉价, 周期最小 1 秒; HTTP 保持最小 60 秒
        let mut wait = if is_file {
            def.interval_sec.max(1)
        } else {
            def.interval_sec.max(60)
        };
        let fetched: Option<String> = if let Some(path) = file_path(&url) {
            match std::fs::read_to_string(&path) {
                Ok(body) => Some(body),
                Err(e) => {
                    tracing::warn!(plugin = %def.name, path = %path.display(), error = %e, "插件文件读取失败");
                    None
                }
            }
        } else if let Some(client) = &client {
            match client.get(&url).send().await {
                Ok(resp) if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS => {
                    tracing::warn!(plugin = %def.name, "插件接口限流(429), 稍后重试");
                    wait = def.retry_sec.max(30);
                    None
                }
                Ok(resp) if resp.status().is_success() => match resp.text().await {
                    Ok(body) => Some(body),
                    Err(e) => {
                        tracing::warn!(plugin = %def.name, error = %e, "插件响应读取失败");
                        None
                    }
                },
                Ok(resp) => {
                    tracing::warn!(plugin = %def.name, status = %resp.status(), "插件接口异常");
                    None
                }
                Err(e) => {
                    tracing::warn!(plugin = %def.name, error = %e, "插件请求失败");
                    None
                }
            }
        } else {
            None
        };
        if let Some(body) = fetched {
            let mut vals: HashMap<String, String> = HashMap::new();
            for ex in &def.extract {
                match run_extractor(ex, &body) {
                    Ok(Some(v)) => {
                        vals.insert(ex.var.clone(), v);
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!(
                        plugin = %def.name,
                        var = %ex.var,
                        error = %e,
                        "插件变量提取失败"
                    ),
                }
            }
            if !vals.is_empty() {
                state
                    .plugins
                    .lock()
                    .await
                    .insert(def.name.clone(), vals.clone());
                tracing::debug!(plugin = %def.name, ?vals, "插件数据已更新");
            }
        }
        // 等待下一周期; 配置变化(如 steamId 变更)立即重查
        tokio::select! {
            _ = sleep(Duration::from_secs(wait)) => {}
            _ = cfg_rx.changed() => {}
        }
    }
}

fn run_extractor(ex: &Extractor, body: &str) -> Result<Option<String>> {
    match ex.kind.as_str() {
        "regex" => {
            let re = regex::Regex::new(&ex.pattern)?;
            Ok(re
                .captures(body)
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string()))
        }
        "count" => {
            let re = regex::Regex::new(&ex.pattern)?;
            Ok(Some(re.find_iter(body).count().to_string()))
        }
        other => Err(anyhow!("未知提取类型: {other}")),
    }
}
