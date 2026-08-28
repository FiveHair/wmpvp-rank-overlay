//! 共享应用状态: 自己的账号卡片 + 当前对局聚合状态 + 面向前端页面的 JSON 结构。

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::Mutex;

use crate::form::FormService;
use crate::rank::{self, RankService};

/// 对局内一名玩家(来自 10002 消息的 playerList)。
#[derive(Debug, Clone)]
pub struct Player {
    pub steam_id: String,
    /// "CT" / "T" / 其他
    pub side: String,
    /// payload 里若带昵称则直接用, 否则依赖反查缓存
    pub name: Option<String>,
    pub kill: u32,
    pub death: u32,
    pub assist: u32,
    pub adr: Option<f64>,
    pub alive: bool,
    pub rating: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct MatchState {
    pub match_id: String,
    pub map: String,
    pub ct_score: u32,
    pub t_score: u32,
    pub ct_half: Option<u32>,
    pub t_half: Option<u32>,
    pub players: Vec<Player>,
}

/// 自己账号的赛季统计(来自 detailStats, 需 token)。
#[derive(Debug, Clone, Default)]
pub struct PlayerStats {
    pub avg_we: Option<f64>,
    pub rating_pro: Option<f64>,
    pub season_cnt: Option<u32>,
    pub adr: Option<f64>,
    pub win_rate: Option<f64>,
    pub kda: Option<f64>,
    pub headshot_ratio: Option<f64>,
    pub rws: Option<f64>,
    /// 当前段位星级(0-7)
    pub stars: Option<u32>,
    pub season_id: String,
    pub summary: String,
}

/// 自己账号的信息卡片。
#[derive(Debug, Clone, Default)]
pub struct MyCard {
    pub nickname: String,
    pub steam_id: String,
    pub avatar: Option<String>,
    pub rank_label: String,
    pub rank_score: Option<u32>,
    /// resolving / not-found / connecting / idle / in-match / error
    pub ws_status: String,
    pub last_error: String,
    /// 赛季统计(detailStats); 无 token 或未查到为 None
    pub stats: Option<PlayerStats>,
}

/// 本地缓存的自带头像(原始 URL + 下载的图片字节), 供前端本地加载。
#[derive(Debug, Clone)]
pub struct AvatarCache {
    pub url: String,
    pub bytes: Vec<u8>,
}

pub struct AppState {
    pub my: Mutex<MyCard>,
    pub match_state: Mutex<Option<MatchState>>,
    /// 最近一次对局数据到达时间(Unix 毫秒)
    pub updated_at: Mutex<u64>,
    pub avatar: Mutex<Option<AvatarCache>>,
    /// 数据插件产出: 插件名 -> (变量名 -> 值), 前端以 {{PLUGIN.<名>.<变量>}} 引用
    pub plugins: Mutex<HashMap<String, HashMap<String, String>>>,
    /// 数据日志(data.log): WS 流/击杀/账号原始数据输出, 供插件读取
    pub datalog: std::sync::Mutex<crate::datalog::DataLog>,
    /// 对局板是否正在展示(match 页报告)
    board_showing: AtomicBool,
    /// 配置代号: 每次保存并应用时递增, 前端据此重置展示状态
    cfg_epoch: AtomicU64,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            my: Mutex::new(MyCard::default()),
            match_state: Mutex::new(None),
            updated_at: Mutex::new(0),
            avatar: Mutex::new(None),
            plugins: Mutex::new(HashMap::new()),
            datalog: std::sync::Mutex::new(crate::datalog::DataLog::create()),
            board_showing: AtomicBool::new(false),
            cfg_epoch: AtomicU64::new(1),
        })
    }

    pub fn set_board_showing(&self, on: bool) {
        self.board_showing.store(on, Ordering::SeqCst);
    }

    pub fn board_showing(&self) -> bool {
        self.board_showing.load(Ordering::SeqCst)
    }

    pub fn bump_cfg_epoch(&self) {
        self.cfg_epoch.fetch_add(1, Ordering::SeqCst);
    }

    /// 清空展示数据(数据源 mock/真实切换时使用)
    pub async fn reset_data(&self) {
        *self.my.lock().await = MyCard::default();
        *self.match_state.lock().await = None;
        *self.avatar.lock().await = None;
    }

    pub fn cfg_epoch(&self) -> u64 {
        self.cfg_epoch.load(Ordering::SeqCst)
    }
}

// ================= 前端 JSON 结构 =================

#[derive(Serialize)]
pub struct ApiState {
    pub in_match: bool,
    pub my: MyJson,
    pub match_info: Option<MatchJson>,
    pub updated_at: u64,
    /// 配置代号(保存并应用时递增), 前端据此重置展示状态
    pub cfg_epoch: u64,
    /// 对局板动画退出(入场播完展示5秒后倒序退场); false = 一直展示
    pub anim_exit: bool,
    /// 对局板当前是否正在展示(match 页报告; 账号卡据此隐藏自己, OBS 中等同隐藏源)
    pub board_showing: bool,
    /// 数据插件产出: 插件名 -> (变量名 -> 值)
    pub plugins: HashMap<String, HashMap<String, String>>,
}

#[derive(Serialize)]
pub struct MyJson {
    pub nickname: String,
    pub steam_id: String,
    pub avatar: Option<String>,
    pub rank_label: String,
    pub rank_score: Option<u32>,
    pub rank_class: String,
    /// 完美官方段位字母图标 URL(空表示未定/未查到)
    pub rank_icon: String,
    /// 段内进度(0~1), 官方徽章边框进度环用
    pub rank_progress: f64,
    pub ws_status: String,
    pub last_error: String,
    pub stats: Option<StatsJson>,
}

#[derive(Serialize)]
pub struct StatsJson {
    pub avg_we: Option<f64>,
    pub rating_pro: Option<f64>,
    pub season_cnt: Option<u32>,
    pub adr: Option<f64>,
    pub win_rate: Option<f64>,
    pub kda: Option<f64>,
    pub headshot_ratio: Option<f64>,
    pub rws: Option<f64>,
    pub stars: Option<u32>,
    pub season_id: String,
    pub summary: String,
}

#[derive(Serialize)]
pub struct MatchJson {
    pub match_id: String,
    pub map: String,
    pub ct_score: u32,
    pub t_score: u32,
    pub ct_half: Option<u32>,
    pub t_half: Option<u32>,
    /// "CT" 或 "T": 监控玩家所在阵营(我方)
    pub my_side: String,
    pub teams: Vec<TeamJson>,
}

#[derive(Serialize)]
pub struct TeamJson {
    pub side: String,
    pub name: String,
    pub avg_score: Option<u32>,
    pub players: Vec<PlayerJson>,
}

#[derive(Serialize)]
pub struct PlayerJson {
    pub steam_id: String,
    pub name: String,
    pub rank_label: String,
    pub rank_score: Option<u32>,
    pub rank_class: String,
    pub rank_loading: bool,
    /// 完美官方段位图标 URL(S 段为钻石图标, 其余为盾牌字母)
    pub rank_icon: String,
    /// 段内进度(0~1), 官方徽章边框进度环用
    pub rank_progress: f64,
    /// S 段星级(仅自账号/mock 注入时有; 决定钻石图标变体)
    pub stars: Option<u32>,
    pub side: String,
    pub kill: u32,
    pub death: u32,
    pub assist: u32,
    pub adr: Option<f64>,
    pub alive: bool,
    pub is_me: bool,
    /// 近几场胜负(最新在前, true=胜); 空表示暂无数据
    pub form: Vec<bool>,
}

fn short_id(sid: &str) -> String {
    if sid.len() > 8 {
        sid[sid.len() - 8..].to_string()
    } else {
        sid.to_string()
    }
}

/// 依据共享状态 + 段位/昵称/战绩缓存, 生成前端快照。
pub async fn build_api_state(
    state: &AppState,
    ranks: &RankService,
    forms: &FormService,
    anim_exit: bool,
) -> ApiState {
    let my = state.my.lock().await.clone();
    let ms = state.match_state.lock().await.clone();
    let updated_at = *state.updated_at.lock().await;

    let my_json = MyJson {
        nickname: my.nickname.clone(),
        steam_id: my.steam_id.clone(),
        avatar: my.avatar.clone(),
        rank_label: my.rank_label.clone(),
        rank_score: my.rank_score,
        rank_class: rank::rank_badge_class(&my.rank_label).to_string(),
        rank_icon: rank::rank_icon_url(&my.rank_label),
        rank_progress: my
            .rank_score
            .map(|s| rank::tier_progress(s as f64))
            .unwrap_or(0.0),
        ws_status: my.ws_status.clone(),
        last_error: my.last_error.clone(),
        stats: my.stats.as_ref().map(|s| StatsJson {
            avg_we: s.avg_we,
            rating_pro: s.rating_pro,
            season_cnt: s.season_cnt,
            adr: s.adr,
            win_rate: s.win_rate,
            kda: s.kda,
            headshot_ratio: s.headshot_ratio,
            rws: s.rws,
            stars: s.stars,
            season_id: s.season_id.clone(),
            summary: s.summary.clone(),
        }),
    };

    let match_info = match ms.as_ref() {
        Some(m) => {
            let my_side = my_side_for(m, &my.steam_id);
            // 我方在前
            let mut ordered: Vec<&str> = Vec::new();
            if !my_side.is_empty() {
                ordered.push(&my_side);
                ordered.push(if my_side == "CT" { "T" } else { "CT" });
            } else {
                ordered.push("CT");
                ordered.push("T");
            }
            let mut teams = Vec::new();
            for side in ordered {
                if let Some(t) = build_team_json(m, side, &my.steam_id, ranks, forms).await {
                    teams.push(t);
                }
            }
            Some(MatchJson {
                match_id: m.match_id.clone(),
                map: m.map.clone(),
                ct_score: m.ct_score,
                t_score: m.t_score,
                ct_half: m.ct_half,
                t_half: m.t_half,
                my_side: my_side.to_string(),
                teams,
            })
        }
        None => None,
    };

    ApiState {
        in_match: ms.is_some(),
        my: my_json,
        match_info,
        updated_at,
        anim_exit,
        board_showing: state.board_showing(),
        cfg_epoch: state.cfg_epoch(),
        plugins: state.plugins.lock().await.clone(),
    }
}

fn my_side_for(m: &MatchState, my_steam_id: &str) -> String {
    if my_steam_id.is_empty() {
        return String::new();
    }
    m.players
        .iter()
        .find(|p| p.steam_id == my_steam_id)
        .map(|p| p.side.clone())
        .unwrap_or_default()
}

async fn build_team_json(
    m: &MatchState,
    side: &str,
    my_steam_id: &str,
    ranks: &RankService,
    forms: &FormService,
) -> Option<TeamJson> {
    let mut players: Vec<&Player> = m
        .players
        .iter()
        .filter(|p| p.side == side)
        .collect();
    if players.is_empty() {
        return None;
    }
    // 排序: rating 降序 -> kill 降序 -> death 升序 -> steamId
    players.sort_by(|a, b| {
        let ra = a.rating.unwrap_or(-1.0);
        let rb = b.rating.unwrap_or(-1.0);
        rb.partial_cmp(&ra)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.kill.cmp(&a.kill))
            .then_with(|| a.death.cmp(&b.death))
            .then_with(|| a.steam_id.cmp(&b.steam_id))
    });

    let mut metas = Vec::with_capacity(players.len());
    let mut js = Vec::with_capacity(players.len());
    for p in players {
        let meta = ranks.get(&p.steam_id).await.unwrap_or_default();
        let mut name = p.name.clone().filter(|n| !n.trim().is_empty());
        if name.is_none() {
            // 昵称未在 payload 中时, 用反查缓存兜底; 未查到则用短 steamId
            name = ranks.name(&p.steam_id).await.filter(|n| !n.trim().is_empty());
        }
        let name = name.unwrap_or_else(|| short_id(&p.steam_id));
        let rank_loading = meta.status == rank::RankStatus::Loading;
        let (label, class, score) = if rank_loading {
            (String::new(), "U".to_string(), None)
        } else {
            (
                meta.label.clone(),
                rank::rank_badge_class(&meta.label).to_string(),
                meta.score,
            )
        };
        metas.push(meta.clone());
        let form = forms.ensure(&p.steam_id).await;
        js.push(PlayerJson {
            steam_id: p.steam_id.clone(),
            name,
            rank_label: label,
            rank_score: score,
            rank_class: class,
            rank_loading,
            rank_icon: rank::rank_icon_url(&meta.label),
            rank_progress: meta.progress,
            stars: form.stars.or(meta.stars),
            side: p.side.clone(),
            kill: p.kill,
            death: p.death,
            assist: p.assist,
            adr: p.adr,
            alive: p.alive,
            is_me: p.steam_id == my_steam_id,
            form: form.results,
        });
    }
    let avg = rank::side_avg_score(&metas);
    Some(TeamJson {
        side: side.to_string(),
        name: if side == "CT" { "CT" } else { "T" }.to_string(),
        avg_score: avg,
        players: js,
    })
}
