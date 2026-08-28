//! 玩家近几场胜负查询: 与段位查询相同的单 worker 串行 + 缓存模式, 避免请求风暴。
//!
//! 数据来自完美平台 `match/list` 接口(需 token), 缓存 TTL 300s;
//! 查询失败按"暂无数据"处理并在 TTL 后重试, 不阻塞计分板其余数据。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

use crate::wmpvp;

/// 对局期间每名玩家只拉一次(一局通常 <1h)
const CACHE_TTL: Duration = Duration::from_secs(1800);
/// 最多展示的场次
pub const FORM_GAMES: usize = 5;

#[derive(Debug, Clone, Default)]
pub struct FormEntry {
    /// 最近几场胜负(最新在前, true=胜); 空表示暂无数据
    pub results: Vec<bool>,
    /// S 段星级(0-100, 来自 detailStats; 决定钻石图标档位), None=未取到
    pub stars: Option<u32>,
}

pub struct FormService {
    tx: mpsc::UnboundedSender<String>,
    /// 与 worker 共享的 token(配置更新时改这里, worker 每次查询前读取)
    token: Arc<Mutex<String>>,
    cache: Arc<Mutex<SharedCache>>,
}

struct SharedCache {
    map: HashMap<String, (FormEntry, Instant)>,
    pending: HashSet<String>,
}

impl FormService {
    pub fn new(client: reqwest::Client, token: String) -> Arc<Self> {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let cache = Arc::new(Mutex::new(SharedCache {
            map: HashMap::new(),
            pending: HashSet::new(),
        }));
        let worker_cache = Arc::clone(&cache);
        let worker_client = client.clone();
        let worker_token = Arc::new(Mutex::new(token));
        let ctor_token = Arc::clone(&worker_token);
        tokio::spawn(async move {
            form_worker(worker_client, worker_token, rx, worker_cache).await;
        });
        Arc::new(Self {
            tx,
            token: ctor_token,
            cache,
        })
    }

    /// 配置更新时同步 token(下一次查询即生效)。
    pub async fn set_token(&self, token: String) {
        *self.token.lock().await = token;
        // token 变了, 旧缓存作废
        let mut cache = self.cache.lock().await;
        cache.map.clear();
        cache.pending.clear();
    }

    /// 把已知的近几场胜负直接写入缓存(mock/调试用, 不发起请求)。
    pub async fn seed(&self, steam_id: &str, results: Vec<bool>) {
        let sid = steam_id.trim().to_string();
        if sid.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().await;
        cache.pending.remove(&sid);
        cache.map.insert(
            sid,
            (FormEntry { results, stars: None }, Instant::now()),
        );
    }

    /// 查询 steamId 的近几场胜负; 命中缓存立即返回, 否则入队(同 id 只排一次)。
    pub async fn ensure(&self, steam_id: &str) -> FormEntry {
        let sid = steam_id.trim().to_string();
        if sid.is_empty() {
            return FormEntry::default();
        }
        let mut cache = self.cache.lock().await;
        if let Some((entry, at)) = cache.map.get(&sid) {
            if at.elapsed() < CACHE_TTL {
                return entry.clone();
            }
        }
        if !cache.pending.contains(&sid) {
            cache.pending.insert(sid.clone());
            cache
                .map
                .entry(sid.clone())
                .or_insert_with(|| (FormEntry::default(), Instant::now()));
            drop(cache);
            let _ = self.tx.send(sid.clone());
            let cache = self.cache.lock().await;
            return cache
                .map
                .get(&sid)
                .map(|(e, _)| e.clone())
                .unwrap_or_default();
        }
        cache
            .map
            .get(&sid)
            .map(|(e, _)| e.clone())
            .unwrap_or_default()
    }
}

async fn form_worker(
    client: reqwest::Client,
    token: Arc<Mutex<String>>,
    mut rx: mpsc::UnboundedReceiver<String>,
    shared: Arc<Mutex<SharedCache>>,
) {
    while let Some(sid) = rx.recv().await {
        let token = token.lock().await.clone();
        let results = wmpvp::fetch_recent_results(&client, &token, &sid, FORM_GAMES)
            .await
            .unwrap_or_default();
        // 顺带取 S 段星级(detailStats 的 stars 字段, 可查任意玩家)
        let stars = match wmpvp::fetch_detail_stats(&client, &token, &sid).await {
            Ok(Some(stats)) => stats.stars,
            _ => None,
        };
        let mut cache = shared.lock().await;
        cache.pending.remove(&sid);
        cache.map.insert(
            sid,
            (FormEntry { results, stars }, Instant::now()),
        );
    }
}
