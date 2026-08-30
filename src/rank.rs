//! 天梯段位分 -> 段位标签, 以及段位查询的串行队列与缓存。
//!
//! 段位分来自 `search/user` 接口(按 steamId 反查), 无需登录。
//! 为避免请求风暴, 所有段位查询经过单 worker 串行执行, 结果按 steamId 缓存。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::wmpvp;

/// S 段接口统一返回 2401, 不显示具体分数(避免多名玩家全是 2401 的困惑)。
const S_SCORE_HIDDEN: f64 = 2401.0;

/// 分数 -> 段位标签(完美平台官方分段, 抓自天梯页前端, 2024-05 赛季调整后):
/// D <1000 < C <1150 < C+ <1300 < 精英C+ <1450 < B <1600 < B+ <1750 < 精英B+ <1900
/// < A <2050 < A+ <2200 < 精英A+ <2400 < S。
pub fn rank_label_from_score(score: f64) -> &'static str {
    if !score.is_finite() {
        return "";
    }
    if score <= 0.0 {
        return "未定";
    }
    if score < 1000.0 {
        return "D";
    }
    if score <= 1150.0 {
        return "C";
    }
    if score <= 1300.0 {
        return "C+";
    }
    if score <= 1450.0 {
        return "精英C+";
    }
    if score <= 1600.0 {
        return "B";
    }
    if score <= 1750.0 {
        return "B+";
    }
    if score <= 1900.0 {
        return "精英B+";
    }
    if score <= 2050.0 {
        return "A";
    }
    if score <= 2200.0 {
        return "A+";
    }
    if score <= 2400.0 {
        return "精英A+";
    }
    "S 段"
}

/// 段位在当前档位内的进度(0~1, 官方天梯徽章边框进度环的填充比例; S 段固定 1)。
pub fn tier_progress(score: f64) -> f64 {
    if !score.is_finite() || score <= 0.0 {
        return 0.0;
    }
    if score < 1000.0 {
        return score / 1000.0;
    }
    const TIERS: &[(f64, f64)] = &[
        (1150.0, 1000.0), // C
        (1300.0, 1150.0), // C+
        (1450.0, 1300.0), // 精英C+
        (1600.0, 1450.0), // B
        (1750.0, 1600.0), // B+
        (1900.0, 1750.0), // 精英B+
        (2050.0, 1900.0), // A
        (2200.0, 2050.0), // A+
        (2400.0, 2200.0), // 精英A+
    ];
    for (max, min) in TIERS {
        if score <= *max {
            return ((score - min) / (max - min)).clamp(0.0, 1.0);
        }
    }
    1.0 // S
}

/// 段位标签 -> 徽章配色等级(前端用)。
pub fn rank_badge_class(label: &str) -> &'static str {
    let t = label.trim().to_uppercase();
    let Some(letter) = t.chars().find(|c| c.is_ascii_alphabetic()) else {
        return "U";
    };
    match letter {
        'S' => "S",
        'A' => "A",
        'B' => "B",
        'C' => "C",
        'D' => "D",
        _ => "U",
    }
}

/// 段位徽章主图标(本地嵌入提供, 素材抓自完美官方天梯页):
/// - S 段: 官方钻石图标(APNG 动画, 星级细分由前端按 stars 选择);
/// - 精英段(精英X+): 官方精英字母图标(X11.svg, 双+映射);
/// - 其他: 官方字母图标, "+" 映射为 "1"(如 "A+" -> /assets/A1.svg);
/// - 空标签返回空串。
pub fn rank_icon_url(label: &str) -> String {
    let t = label.trim().to_uppercase();
    let Some(letter) = t.chars().find(|c| c.is_ascii_alphabetic()) else {
        return String::new();
    };
    if letter == 'S' {
        return "/assets/s-diamond-1.png".to_string();
    }
    let file = if t.contains("精英") {
        format!("{}11", letter)
    } else if t.contains('+') {
        format!("{}1", letter)
    } else {
        letter.to_string()
    };
    format!("/assets/{}.svg", file)
}

/// 玩家段位查询结果。
#[derive(Debug, Clone, Default)]
pub struct RankMeta {
    pub status: RankStatus,
    pub label: String,
    /// 非 S 段的具体分数; S 段为 None(2401 不展示)。
    pub score: Option<u32>,
    /// 段内进度(0~1), 官方徽章边框进度环用
    pub progress: f64,
    /// S 段星级(仅自己账号或 mock 注入时有; 决定钻石图标变体)
    pub stars: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RankStatus {
    Loading,
    Ok,
    Unavailable,
}

impl Default for RankStatus {
    fn default() -> Self {
        RankStatus::Loading
    }
}

/// 玩家资料缓存条目: 昵称与段位来自同一次 `search/user` 请求。
#[derive(Debug, Clone)]
struct CacheEntry {
    meta: RankMeta,
    name: String,
}

/// 段位查询服务: 暴露 `ensure(steam_id)`, 内部单 worker 串行执行网络请求。
pub struct RankService {
    _client: reqwest::Client,
    tx: mpsc::UnboundedSender<String>,
    cache: Arc<Mutex<SharedCache>>,
}

/// 缓存 + 待处理集合(同一个锁保护, 避免同 steamId 重复排队)。
struct SharedCache {
    map: HashMap<String, CacheEntry>,
    pending: std::collections::HashSet<String>,
}

impl RankService {
    pub fn new(client: reqwest::Client) -> Self {
        let (tx, rx) = mpsc::unbounded_channel::<String>();
        let cache = Arc::new(Mutex::new(SharedCache {
            map: HashMap::new(),
            pending: std::collections::HashSet::new(),
        }));
        let worker_cache = Arc::clone(&cache);
        let worker_client = client.clone();
        tokio::spawn(async move {
            rank_worker(worker_client, rx, worker_cache).await;
        });
        Self {
            _client: client,
            tx,
            cache,
        }
    }

    /// 查询 steamId 的段位; 缓存命中则立即返回, 否则进入串行队列(同 id 只排队一次)。
    /// 返回的 RankMeta 状态可能是 Loading(尚未查到)或 Ok/Unavailable。
    pub async fn ensure(&self, steam_id: &str) -> RankMeta {
        let sid = steam_id.trim().to_string();
        if sid.is_empty() {
            return RankMeta {
                status: RankStatus::Unavailable,
                ..Default::default()
            };
        }
        let mut cache = self.cache.lock().await;
        // 每局只取一次: 缓存命中直接返回, 新对局由 reset_for_match 清空后重新拉取
        if let Some(entry) = cache.map.get(&sid) {
            return entry.meta.clone();
        }
        if !cache.pending.contains(&sid) {
            cache.pending.insert(sid.clone());
            cache.map.entry(sid.clone()).or_insert_with(|| CacheEntry {
                meta: RankMeta {
                    status: RankStatus::Loading,
                    ..Default::default()
                },
                name: String::new(),
            });
            drop(cache); // 发送前释放锁, worker 回调不需要本锁
            let _ = self.tx.send(sid.clone());
            let cache = self.cache.lock().await;
            return cache
                .map
                .get(&sid)
                .map(|e| e.meta.clone())
                .unwrap_or_default();
        }
        cache
            .map
            .get(&sid)
            .map(|e| e.meta.clone())
            .unwrap_or_default()
    }

    /// 返回缓存中的段位(不发起请求)。
    pub async fn get(&self, steam_id: &str) -> Option<RankMeta> {
        let cache = self.cache.lock().await;
        cache.map.get(steam_id.trim()).map(|e| e.meta.clone())
    }

    /// 返回缓存中的昵称(不发起请求; 未查到返回 None)。
    pub async fn name(&self, steam_id: &str) -> Option<String> {
        let cache = self.cache.lock().await;
        cache
            .map
            .get(steam_id.trim())
            .map(|e| e.name.clone())
            .filter(|n| !n.is_empty())
    }

    /// 把已获取的用户资料直接写入缓存(避免重复请求; 通常用于自己的账号信息)。
    pub async fn seed_user(&self, steam_id: &str, user: wmpvp::User) -> bool {
        let sid = steam_id.trim().to_string();
        if sid.is_empty() {
            return false;
        }
        let (meta, name) = meta_from_user(&user);
        let mut cache = self.cache.lock().await;
        cache.pending.remove(&sid);
        cache.map.insert(sid, CacheEntry { meta, name });
        true
    }

    /// 新对局开始: 清空段位缓存(自己的账号随后由 ensure/seed 重新填充), 保证每局各取一次。
    pub async fn reset_for_match(&self) {
        let mut cache = self.cache.lock().await;
        cache.map.clear();
        cache.pending.clear();
    }

    /// 补充 S 段星级(更新已有缓存条目; mock/自账号 detailStats 用)。
    pub async fn seed_stars(&self, steam_id: &str, stars: u32) {
        let sid = steam_id.trim().to_string();
        if sid.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().await;
        if let Some(entry) = cache.map.get_mut(&sid) {
            entry.meta.stars = Some(stars.min(100));
        }
    }
}

async fn rank_worker(
    client: reqwest::Client,
    mut rx: mpsc::UnboundedReceiver<String>,
    shared: Arc<Mutex<SharedCache>>,
) {
    while let Some(sid) = rx.recv().await {
        let (meta, name) = fetch_rank_once(&client, &sid).await;
        let mut cache = shared.lock().await;
        cache.pending.remove(&sid);
        cache.map.insert(sid.clone(), CacheEntry { meta, name });
        tracing::info!(steam_id = %sid, "段位已更新");
    }
}

async fn fetch_rank_once(client: &reqwest::Client, steam_id: &str) -> (RankMeta, String) {
    match wmpvp::search_by_steam_id(client, steam_id).await {
        Ok(Some(user)) => meta_from_user(&user),
        Ok(None) | Err(_) => (
            RankMeta {
                status: RankStatus::Unavailable,
                ..Default::default()
            },
            String::new(),
        ),
    }
}

/// 由 `search/user` 返回的用户条目构造段位元信息 + 昵称。
fn meta_from_user(user: &wmpvp::User) -> (RankMeta, String) {
    let name = user.nickname.clone().unwrap_or_default();
    let n = user.score_num().unwrap_or(0.0);
    if n > 0.0 {
        let rounded = n.round() as u32;
        let label = rank_label_from_score(n).to_string();
        let is_s = label.starts_with('S');
        let score = if is_s && (n - S_SCORE_HIDDEN).abs() < 0.5 {
            None // S 段 2401 不展示
        } else {
            Some(rounded)
        };
        (
            RankMeta {
                status: RankStatus::Ok,
                label,
                score,
                progress: tier_progress(n),
                stars: None,
            },
            name,
        )
    } else {
        (
            RankMeta {
                status: RankStatus::Unavailable,
                ..Default::default()
            },
            name,
        )
    }
}

/// 计算一队人的段位均分(只统计已查到的非 S 段有效分数)。
pub fn side_avg_score(meta_list: &[RankMeta]) -> Option<u32> {
    let mut sum: u64 = 0;
    let mut cnt = 0usize;
    for m in meta_list {
        if m.status == RankStatus::Ok && !m.label.starts_with('S') {
            if let Some(s) = m.score {
                sum += s as u64;
                cnt += 1;
            }
        }
    }
    if cnt == 0 {
        None
    } else {
        Some((sum as f64 / cnt as f64).round() as u32)
    }
}
