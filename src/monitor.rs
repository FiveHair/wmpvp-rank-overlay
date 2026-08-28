//! WebSocket 对局监控: 订阅完美平台推送。
//!
//! 协议(与油猴脚本一致):
//! - 连接后发送 `ping`, 服务端回 `pong`, 随后每 `poll_ms` 发送
//!   `{"messageType":10001,"messageData":{"steam_id":...}}` 轮询。
//! - `10002` = 对局数据(playerList/killCount/比分), `10003` = 空闲。

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::form::FormService;
use crate::rank::RankService;
use crate::state::{AppState, MatchState, MyCard, Player, PlayerStats};
use crate::wmpvp;

pub struct MonitorConfig {
    /// 被监控账号的 steamId(自己)
    pub steam_id: String,
    /// 完美电竞登录 token(可选); 提供后启用赛季统计 detailStats
    pub token: String,
    pub poll_ms: u64,
    /// 本地面板端口
    pub port: u16,
    /// 对局板动画退出(入场播完展示5秒后倒序退场); false = 一直展示
    pub anim_exit: bool,
    /// 占位数据模式(保存后立即切换, 不需重启); 插件同步输出模拟值
    pub mock: bool,
}

const STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// 赛季统计刷新间隔
const STATS_REFRESH: Duration = Duration::from_secs(600);

/// 主监控循环(永不返回): 加载账号信息 -> 订阅 WS -> 断线退避重连。
/// 配置通过 watch 通道热更新: token/steamId 变化时自动重载账号信息并重连。
pub async fn run(
    client: reqwest::Client,
    state: Arc<AppState>,
    ranks: Arc<RankService>,
    forms: Arc<FormService>,
    mut cfg_rx: watch::Receiver<Arc<MonitorConfig>>,
) {
    // 提供 token 时, 后台定期刷新自己账号的赛季统计(低频, 配置变化即时刷新)
    {
        let st_state = Arc::clone(&state);
        let st_client = client.clone();
        tokio::spawn(stats_loop(st_client, st_state, cfg_rx.clone()));
    }

    let mut retry: u32 = 0;
    let mut ws_url: Option<String> = None;
    // 已加载账号信息对应的 steamId(配置变化后清空, 触发重新加载)
    let mut loaded_for: Option<String> = None;
    // 已生效配置指纹 (steam_id, token, poll_ms)
    let mut applied: Option<(String, String, u64)> = None;

    loop {
        let cfg = cfg_rx.borrow_and_update().clone();
        let key = (
            cfg.steam_id.trim().to_string(),
            cfg.token.trim().to_string(),
            cfg.poll_ms.max(1000),
        );
        if applied.as_ref() != Some(&key) {
            tracing::info!(steam_id = %key.0, has_token = !key.1.is_empty(), "已应用新配置");
            applied = Some(key.clone());
            ws_url = None;
            retry = 0;
            loaded_for = None;
            forms.set_token(cfg.token.trim().to_string()).await;
            {
                let mut my = state.my.lock().await;
                *my = MyCard::default();
                my.steam_id = cfg.steam_id.trim().to_string();
            }
            *state.match_state.lock().await = None;
        }
        let sid_input = cfg.steam_id.trim().to_string();
        if sid_input.is_empty() {
            set_status(&state, "idle", "未配置 steamId, 请在配置窗口填写后保存").await;
            if cfg_rx.changed().await.is_err() {
                return;
            }
            continue;
        }

        // 1) 加载自己的账号信息(昵称/头像, 顺带把段位种进缓存), 只在配置变化后加载一次
        if loaded_for.is_none() {
            set_status(&state, "resolving", "").await;
            match wmpvp::search_by_steam_id(&client, &sid_input).await {
                Ok(Some(user)) => {
                    let formal = user.steam_id().trim().to_string();
                    let sid_for_card = if formal.is_empty() { sid_input.clone() } else { formal };
                    // 以接口返回的正式 steamId 为准(可能补零/格式化)
                    let _ = ranks.seed_user(&sid_for_card, user.clone()).await;
                    {
                        let nickname = user
                            .nickname
                            .clone()
                            .filter(|n| !n.trim().is_empty())
                            .unwrap_or_else(|| short_id(&sid_for_card));
                        let mut my = state.my.lock().await;
                        my.steam_id = sid_for_card.clone();
                        my.nickname = nickname.clone();
                        my.avatar = user.avatar.clone();
                        state
                            .datalog
                            .lock()
                            .unwrap()
                            .raw(&format!("[ACCOUNT] steam_id={} nickname={}", sid_for_card, nickname));
                    }
                    tracing::info!(steam_id = %sid_for_card, "已加载账号信息");
                    // 配置切换时 stats_loop 可能已先刷新过又被上面的状态重置清掉,
                    // 账号加载完成后立即补一次, 保证信息卡统计/星级就位
                    let cfg_token = cfg.token.trim().to_string();
                    if !cfg_token.is_empty() {
                        let _ = refresh_stats(&client, &state, &sid_for_card, &cfg_token).await;
                    }
                    // 下载头像到本地缓存, 供前端稳定加载(不依赖外网 CDN)
                    if let Some(av) = user.avatar.clone().filter(|a| !a.trim().is_empty()) {
                        let st_av = Arc::clone(&state);
                        let cl_av = client.clone();
                        tokio::spawn(async move {
                            // 同一 URL 已缓存则跳过
                            let cached_same = st_av
                                .avatar
                                .lock()
                                .await
                                .as_ref()
                                .map(|a| a.url == av)
                                .unwrap_or(false);
                            if cached_same {
                                return;
                            }
                            match cl_av.get(&av).send().await {
                                Ok(r) => {
                                    if let Ok(bytes) = r.bytes().await {
                                        if !bytes.is_empty() {
                                            let mut c = st_av.avatar.lock().await;
                                            *c = Some(crate::state::AvatarCache {
                                                url: av.clone(),
                                                bytes: bytes.to_vec(),
                                            });
                                            tracing::info!(url = %av, "头像已缓存到本地");
                                        }
                                    }
                                }
                                Err(e) => tracing::warn!(error = %e, "头像下载失败"),
                            }
                        });
                    }
                    loaded_for = Some(sid_for_card);
                }
                Ok(None) => {
                    set_status(&state, "error", "未在完美平台查到该 steamId").await;
                    tracing::warn!(steam_id = %sid_input, "search/user 未匹配到该 steamId");
                    // 未匹配到正式 steamId 时也先把用户给的 id 记下, 避免反复查询
                    state.my.lock().await.steam_id = sid_input.clone();
                    loaded_for = Some(sid_input.clone());
                }
                Err(e) => {
                    set_status(&state, "error", &format!("查询账号信息失败: {}", e)).await;
                    tracing::warn!(error = %e, "查询账号信息失败");
                    sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }
        }

        let sid = state.my.lock().await.steam_id.clone();

        // 2) 把自己的段位同步进信息卡片(worker 完成后自动生效)
        sync_own_rank(&state, &ranks, &sid).await;

        // 3) 获取/复用 WS 地址
        if ws_url.is_none() {
            match wmpvp::get_websocket_url(&client, &sid).await {
                Ok(url) => ws_url = Some(url),
                Err(e) => {
                    set_status(&state, "error", &format!("获取 WS 地址失败: {}", e)).await;
                    retry += 1;
                    sleep(backoff(retry)).await;
                    continue;
                }
            }
        }

        // 4) 连接并订阅(配置变化时立即中断会话, 避免旧账号数据滞留)
        let url = ws_url.clone().expect("ws_url 已赋值");
        set_status(&state, "connecting", "").await;
        let session = run_ws_session(
            &state,
            &ranks,
            &forms,
            &sid,
            &url,
            cfg.poll_ms,
            cfg_rx.clone(),
            key.clone(),
        )
        .await;
        match session {
            Err(e) if e.to_string().contains("配置已变化") => {
                tracing::info!("监控配置已变化, 立即重建会话");
                continue; // 不退避, 直接按新配置重载
            }
            Err(e) => {
                // 断线属正常事件(网络切换/服务器空闲回收), 静默退避重连即可, 不刷 WARN
                tracing::info!(error = %e, "WS 连接断开, 准备重连");
            }
            Ok(()) => {}
        }

        retry += 1;
        if retry > 5 {
            // 连续失败次数过多时刷新 ws 地址(可能 token/地址失效)
            ws_url = None;
            retry = 0;
        }
        set_status(&state, "error", "连接断开, 正在重连").await;
        sleep(backoff(retry)).await;
    }
}

fn backoff(retry: u32) -> Duration {
    Duration::from_millis(2000 * 2u64.pow(retry.min(5)))
}

/// 后台赛季统计(detailStats)刷新: 600s 一次; token/steamId 配置变化时立即重新查询;
/// token 失效时零请求等待, 直到配置更新。
/// 拉取一次 detailStats 写入 my.stats(赛季统计/星级)。返回 false 表示 token 失效。
async fn refresh_stats(
    client: &reqwest::Client,
    state: &AppState,
    steam_id: &str,
    token: &str,
) -> bool {
    match wmpvp::fetch_detail_stats(client, token, steam_id).await {
        Ok(Some(ds)) => {
            let mut my = state.my.lock().await;
            my.stats = Some(PlayerStats {
                avg_we: ds.avg_we,
                rating_pro: ds.pw_rating,
                season_cnt: ds.cnt,
                adr: ds.adr,
                win_rate: ds.win_rate,
                kda: ds.kd,
                headshot_ratio: ds.head_shot_ratio,
                rws: ds.rws,
                stars: ds.stars,
                season_id: ds.season_id,
                summary: ds.summary.clone().unwrap_or_default(),
            });
            drop(my);
            tracing::info!(steam_id = %steam_id, "赛季统计已刷新");
            true
        }
        Ok(None) => {
            tracing::debug!("detailStats 无数据");
            true
        }
        Err(wmpvp::StatsError::LoginExpired) => {
            let mut my = state.my.lock().await;
            my.ws_status = "token-invalid".to_string();
            my.last_error = "token 已失效, 请重新抓取".to_string();
            drop(my);
            tracing::error!("detailStats token 已失效, 等待新配置");
            false
        }
        Err(wmpvp::StatsError::Other(e)) => {
            tracing::warn!(error = %e, "detailStats 查询失败, 稍后重试");
            true
        }
    }
}

async fn stats_loop(
    client: reqwest::Client,
    state: Arc<AppState>,
    mut cfg_rx: watch::Receiver<Arc<MonitorConfig>>,
) {
    let mut last: Option<(String, String)> = None;
    loop {
        let cfg = cfg_rx.borrow_and_update().clone();
        let token = cfg.token.trim().to_string();
        let target = cfg.steam_id.trim().to_string();
        if token.is_empty() || target.is_empty() {
            last = None;
            if cfg_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
        let key = (token.clone(), target.clone());
        let should_query = last.as_ref() != Some(&key);
        if !should_query {
            tokio::select! {
                _ = sleep(STATS_REFRESH) => {}
                _ = cfg_rx.changed() => continue,
            }
        }
        last = Some(key.clone());
        if !refresh_stats(&client, &state, &target, &token).await {
            // token 失效, 等待新配置
            last = None;
            if cfg_rx.changed().await.is_err() {
                return;
            }
            continue;
        }
    }
}

async fn set_status(state: &AppState, status: &str, err: &str) {
    let mut my = state.my.lock().await;
    my.ws_status = status.to_string();
    if !err.is_empty() {
        my.last_error = err.to_string();
    }
}

async fn sync_own_rank(state: &AppState, ranks: &RankService, sid: &str) {
    if let Some(meta) = ranks.get(sid).await {
        if meta.status == crate::rank::RankStatus::Ok {
            let mut my = state.my.lock().await;
            my.rank_label = meta.label.clone();
            my.rank_score = meta.score;
        }
    }
}

/// 单次 WS 会话: 建立连接, 持续轮询, 直至断开/超时/出错。
async fn run_ws_session(
    state: &Arc<AppState>,
    ranks: &RankService,
    forms: &FormService,
    steam_id: &str,
    ws_url: &str,
    poll_ms: u64,
    mut cfg_rx: watch::Receiver<Arc<MonitorConfig>>,
    cfg_key: (String, String, u64),
) -> Result<()> {
    let mut request = ws_url.into_client_request()?;
    // 模拟浏览器来源头, 与 Web 端握手保持一致
    if let Ok(origin) = tokio_tungstenite::tungstenite::http::HeaderValue::from_str(
        "https://client.wmpvp.com",
    ) {
        request
            .headers_mut()
            .insert(tokio_tungstenite::tungstenite::http::header::ORIGIN, origin);
    }
    let (ws, _resp) = tokio_tungstenite::connect_async(request).await?;
    tracing::info!("WS 已连接");
    state.datalog.lock().unwrap().raw("[WS] connected");
    let (mut write, mut read) = ws.split();
    write
        .send(tokio_tungstenite::tungstenite::Message::Text("ping".into()))
        .await?;

    let mut notified_status = false;
    // 击杀流差分基线(断线重连/新对局时重置)
    let mut kill_tracker = KillTracker::default();
    // 对齐原脚本的全双工节奏: read 永远在 select 中被 poll(及时处理 WS Ping/Pong),
    // 轮询用独立定时器 —— 收到响应后等 poll_ms 再发下一条, 而不是内联 sleep 阻塞读取。
    let poll_dur = Duration::from_millis(poll_ms);
    let mut next_poll: Option<Pin<Box<tokio::time::Sleep>>> = None;
    loop {
        tokio::select! {
            msg = read.next() => {
                match msg {
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Text(t))) => {
                        let t = t.to_string();
                        tracing::debug!(recv = %truncate(&t, 240), "WS 收到文本消息");
                        if t == "pong" {
                            tracing::info!("收到 pong, 发送首次轮询");
                            write.send(poll_message(steam_id)).await?;
                            continue;
                        }
                        let env: WsEnvelope = match serde_json::from_str(&t) {
                            Ok(e) => e,
                            Err(_) => continue,
                        };
                        // 数据日志: WS 流消息原文(同类型内容变化才写, 供插件读取筛选)
                        if (10001..=10003).contains(&env.message_type) {
                            state.datalog.lock().unwrap().line(
                                &format!("ws{}", env.message_type),
                                &format!("[WS] type={} data={}", env.message_type, t),
                            );
                        }
                        match env.message_type {
                            10002 => {
                                if !notified_status { set_status(state, "in-match", "").await; notified_status = true; }
                                handle_match_payload(state, ranks, forms, &mut kill_tracker, steam_id, &env.message_data).await;
                            }
                            10003 => {
                                if !notified_status { set_status(state, "idle", "").await; notified_status = true; }
                                // 空闲: 清空对局
                                if state.match_state.lock().await.take().is_some() {
                                    tracing::info!("对局结束, 回到空闲");
                                }
                                kill_tracker.reset();
                                *state.updated_at.lock().await = now_millis();
                            }
                            _ => {}
                        }
                        if env.message_type == 10002 || env.message_type == 10003 {
                            // 响应到达后安排下一次轮询(期间读取保持活跃)
                            next_poll = Some(Box::pin(tokio::time::sleep(poll_dur)));
                        }
                    }
                    Some(Ok(tokio_tungstenite::tungstenite::Message::Ping(p))) => {
                        // 立即回 Pong 保活(服务器探测)
                        write.send(tokio_tungstenite::tungstenite::Message::Pong(p)).await?;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => return Err(anyhow!("WS 读取错误: {}", e)),
                    None => return Err(anyhow!("WS 连接关闭")),
                }
            }
            _ = async {
                match next_poll.as_mut() {
                    Some(s) => s.as_mut().await,
                    None => std::future::pending::<()>().await,
                }
            }, if next_poll.is_some() => {
                next_poll = None;
                write.send(poll_message(steam_id)).await?;
            }
            // 监控配置(steamId/token/轮询)变化 -> 立即中断会话, 由主循环按新配置重载
            _ = cfg_rx.changed() => {
                let c = cfg_rx.borrow().clone();
                let new_key = (
                    c.steam_id.trim().to_string(),
                    c.token.trim().to_string(),
                    c.poll_ms.max(1000),
                );
                if new_key != cfg_key {
                    return Err(anyhow!("监控配置已变化, 重建会话"));
                }
            }
            _ = sleep(STALL_TIMEOUT) => {
                return Err(anyhow!("WS 无响应超过 {}s", STALL_TIMEOUT.as_secs()));
            }
        }
    }
}

fn poll_message(steam_id: &str) -> tokio_tungstenite::tungstenite::Message {
    let msg = serde_json::json!({
        "messageType": 10001,
        "messageData": { "steam_id": steam_id }
    });
    tokio_tungstenite::tungstenite::Message::Text(msg.to_string().into())
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

fn short_id(sid: &str) -> String {
    if sid.len() > 8 {
        sid[sid.len() - 8..].to_string()
    } else {
        sid.to_string()
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ================= 10002 载荷解析 =================

#[derive(Debug, Deserialize)]
struct WsEnvelope {
    #[serde(alias = "messageType")]
    message_type: i64,
    #[serde(alias = "messageData", default)]
    message_data: serde_json::Value,
}

#[derive(Debug, Deserialize, Default)]
struct MatchPayload {
    #[serde(alias = "matchId", alias = "match_id", alias = "id", default)]
    match_id: Option<String>,
    #[serde(alias = "map", alias = "mapName", default)]
    map: Option<String>,
    #[serde(alias = "ctScore", alias = "score1", default)]
    ct_score: Option<u32>,
    #[serde(alias = "terroristScore", alias = "score2", default)]
    t_score: Option<u32>,
    #[serde(alias = "ctHalfScore", default)]
    ct_half: Option<u32>,
    #[serde(alias = "terroristHalfScore", default)]
    t_half: Option<u32>,
    #[serde(alias = "playerList", alias = "players", default)]
    players: Vec<PlayerPayload>,
    /// 击杀矩阵(累计只增): { killerId: { victimId: count } }
    #[serde(alias = "killCount", default)]
    kill_count: Option<serde_json::Map<String, serde_json::Value>>,
    /// 击杀流水(含武器), 元素含 KillerId/VictimId/Weapon
    #[serde(alias = "killHistory", default)]
    kill_history: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct PlayerPayload {
    #[serde(alias = "steamId", alias = "steam_id", alias = "playerId", alias = "uid", default)]
    steam_id: Option<String>,
    #[serde(alias = "side", alias = "team", alias = "camp", alias = "group", default)]
    side: Option<String>,
    #[serde(alias = "kill", alias = "kills", default)]
    kill: Option<u32>,
    #[serde(alias = "death", alias = "deaths", alias = "deathCount", default)]
    death: Option<u32>,
    #[serde(alias = "assist", alias = "assists", default)]
    assist: Option<u32>,
    #[serde(default)]
    adr: Option<f64>,
    #[serde(alias = "alive", default)]
    alive: Option<bool>,
    #[serde(alias = "rating", alias = "pwRating", default)]
    rating: Option<f64>,
    #[serde(
        alias = "pvpNickName",
        alias = "name",
        alias = "nickName",
        alias = "userName",
        alias = "nickname",
        default
    )]
    name: Option<String>,
}

fn normalize_side(s: &str) -> String {
    let up = s.trim().to_uppercase();
    if up == "CT" || up.contains("COUNTER") {
        "CT".to_string()
    } else if up == "T" || up.contains("TERROR") {
        "T".to_string()
    } else {
        up
    }
}

/// 击杀流追踪: 基于累计击杀矩阵做差分, 新增部分对照 killHistory 取武器, 输出日志。
#[derive(Default)]
struct KillTracker {
    prev: Option<serde_json::Map<String, serde_json::Value>>,
}

impl KillTracker {
    fn reset(&mut self) {
        self.prev = None;
    }
}

/// 武器代码规范化: 去掉 weapon_ 前缀, 统一小写(如 "weapon_AK47_VIP" -> "ak47_vip")
fn weapon_code(raw: &str) -> String {
    let t = raw.trim().trim_start_matches("weapon_");
    t.to_lowercase()
}

/// 从 killHistory 元素取 (killer, victim, weapon), 字段名做多别名兼容
fn hist_entry(h: &serde_json::Value) -> Option<(String, String, String)> {
    let get = |keys: &[&str]| -> Option<String> {
        for k in keys {
            if let Some(v) = h.get(*k).and_then(|v| v.as_str()) {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
        None
    };
    let killer = get(&["KillerId", "killerId", "killer"])?;
    let victim = get(&["VictimId", "victimId", "victim", "deadId", "DeadId"])?;
    let weapon = get(&["Weapon", "weapon", "gun"]).unwrap_or_default();
    Some((killer, victim, weapon))
}

/// 击杀差分 + 日志输出: 谁使用什么枪械击杀谁。
/// 矩阵是累计值, 与上一帧比较得到新增事件; 武器从 killHistory 末尾向前按 (killer,victim) 匹配。
async fn log_kills(
    payload: &MatchPayload,
    tracker: &mut KillTracker,
    ranks: &RankService,
    state: &Arc<AppState>,
    my_sid: &str,
) {
    let Some(kc) = payload.kill_count.clone() else {
        return;
    };
    let prev = tracker.prev.replace(kc.clone());
    // 首帧(新对局/重连)只建立基线, 累计旧击杀不算增量
    let Some(prev) = prev else {
        return;
    };

    // 差分出新增击杀 (killer, victim)
    let mut events: Vec<(String, String)> = Vec::new();
    for (killer, victims) in &kc {
        let Some(vmap) = victims.as_object() else { continue };
        for (victim, cnt) in vmap {
            let cnt = cnt.as_u64().unwrap_or(0);
            let prev_cnt = prev
                .get(killer)
                .and_then(|v| v.get(victim))
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            for _ in prev_cnt..cnt {
                events.push((killer.clone(), victim.clone()));
            }
        }
    }
    if events.is_empty() {
        return;
    }

    // killHistory 末尾向前匹配, 取每条事件的武器(匹配不到的武器留空)
    let hist = payload.kill_history.clone().unwrap_or_default();
    let mut remain = std::collections::HashMap::<(String, String), u64>::new();
    for (k, v) in &events {
        *remain.entry((k.clone(), v.clone())).or_insert(0) += 1;
    }
    let mut matched: Vec<(String, String, String)> = Vec::new();
    for h in hist.iter().rev() {
        if matched.len() >= events.len() {
            break;
        }
        let Some((k, v, w)) = hist_entry(h) else { continue };
        let key = (k.clone(), v.clone());
        if let Some(n) = remain.get_mut(&key) {
            if *n == 0 {
                continue;
            }
            *n -= 1;
            matched.push((k, v, weapon_code(&w)));
        }
    }
    // history 缺失/截断时补齐无武器事件
    let mut left = events.clone();
    for (k, v, w) in &matched {
        if let Some(pos) = left.iter().position(|(a, b)| a == k && b == v) {
            let _ = w;
            left.remove(pos);
        }
    }
    for (k, v) in left {
        matched.push((k, v, String::new()));
    }

    for (killer, victim, weapon) in matched {
        let kn = display_name(&killer, payload, ranks).await;
        let vn = display_name(&victim, payload, ranks).await;
        if weapon.is_empty() {
            tracing::info!(killer = %kn, victim = %vn, "击杀");
        } else {
            tracing::info!(killer = %kn, weapon = %weapon, victim = %vn, "击杀");
        }
        // 数据日志: 击杀事件(killCount 键为截断 ID, 匹配回完整 ID; self= 是否被监控账号)
        let kf = full_steam_id(&killer, payload);
        let vf = full_steam_id(&victim, payload);
        let self_kill = kf
            .as_deref()
            .is_some_and(|f| f.eq_ignore_ascii_case(my_sid.trim()));
        state.datalog.lock().unwrap().raw(&format!(
            "[KILL] self={} killer={} killer_name={} weapon={} victim={} victim_name={}",
            self_kill,
            kf.as_deref().unwrap_or(killer.trim()),
            kn,
            weapon,
            vf.as_deref().unwrap_or(victim.trim()),
            vn,
        ));
    }
}

/// killCount 的键是完整 steamId 的截断(常见末 8 位), 按后缀匹配回完整 ID
fn full_steam_id(key: &str, payload: &MatchPayload) -> Option<String> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    payload
        .players
        .iter()
        .find(|p| {
            p.steam_id
                .as_deref()
                .is_some_and(|sid| sid.ends_with(key) || key.ends_with(sid))
        })
        .and_then(|p| p.steam_id.clone())
}

/// 展示名: killCount 的键是完整 steamId 的截断(常见末 8 位), 按后缀匹配回完整 ID,
/// 再取 payload 昵称 > 段位缓存昵称 > 短 ID 兜底。
async fn display_name(key: &str, payload: &MatchPayload, ranks: &RankService) -> String {
    let key = key.trim();
    let id_match = |sid: &str| -> bool {
        !key.is_empty() && (sid.ends_with(key) || key.ends_with(sid))
    };
    // payload 自带昵称优先
    if let Some(p) = payload
        .players
        .iter()
        .find(|p| p.steam_id.as_deref().is_some_and(id_match))
    {
        if let Some(n) = p.name.as_deref().filter(|n| !n.trim().is_empty()) {
            return n.to_string();
        }
    }
    // 完整 ID 查段位缓存昵称
    if let Some(full) = payload
        .players
        .iter()
        .find(|p| p.steam_id.as_deref().is_some_and(id_match))
        .and_then(|p| p.steam_id.clone())
    {
        if let Some(n) = ranks.name(&full).await.filter(|n| !n.trim().is_empty()) {
            return n;
        }
    }
    short_id(key)
}

async fn handle_match_payload(
    state: &Arc<AppState>,
    ranks: &RankService,
    forms: &FormService,
    tracker: &mut KillTracker,
    my_sid: &str,
    data: &serde_json::Value,
) {
    let payload: MatchPayload = match serde_json::from_value(data.clone()) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "10002 载荷解析失败");
            return;
        }
    };
    let match_id = payload
        .match_id
        .clone()
        .unwrap_or_default()
        .trim()
        .to_string();
    if match_id.is_empty() {
        tracing::warn!("10002 无 matchId, 忽略");
        return;
    }

    let mut players = Vec::with_capacity(payload.players.len());
    for pp in payload.players.clone() {
        let sid = pp.steam_id.unwrap_or_default().trim().to_string();
        if sid.is_empty() {
            continue;
        }
        players.push(Player {
            steam_id: sid.clone(),
            side: normalize_side(pp.side.as_deref().unwrap_or("")),
            name: pp.name.clone().filter(|n| !n.trim().is_empty()),
            kill: pp.kill.unwrap_or(0),
            death: pp.death.unwrap_or(0),
            assist: pp.assist.unwrap_or(0),
            adr: pp.adr,
            alive: pp.alive.unwrap_or(true),
            rating: pp.rating,
        });
        // 昵称 + 段位一次反查入队
        let _ = ranks.ensure(&sid).await;
        // 近几场胜负入队(缓存命中时无网络开销)
        let _ = forms.ensure(&sid).await;
    }

    let is_new = {
        let mut lock = state.match_state.lock().await;
        let new = lock
            .as_ref()
            .map(|old| old.match_id != match_id)
            .unwrap_or(true);
        *lock = Some(MatchState {
            match_id,
            map: payload.map.clone().unwrap_or_default(),
            ct_score: payload.ct_score.unwrap_or(0),
            t_score: payload.t_score.unwrap_or(0),
            ct_half: payload.ct_half,
            t_half: payload.t_half,
            players,
        });
        *state.updated_at.lock().await = now_millis();
        new
    };
    if is_new {
        tracing::info!("进入对局");
        tracker.reset();
        set_status(state, "in-match", "").await;
        let sid = state.my.lock().await.steam_id.clone();
        sync_own_rank(state, ranks, &sid).await;
    }
    log_kills(&payload, tracker, ranks, state, my_sid).await;
}
