//! 调试用模拟数据源: 不访问任何完美接口, 全部使用占位数据。
//!
//! config.json `"mock": true` 时启用(替代真实 monitor):
//! - 账号卡填充固定的占位账号与赛季统计;
//! - 循环模拟 "空闲 -> 进局 -> 比分推进 -> 结束" 生命周期,
//!   便于反复观察两个浏览器源的入场/出场动画;
//! - 段位与近五场战绩直接写入 RankService/FormService 缓存, 零网络请求。

use std::sync::Arc;
use std::time::Duration;

use tokio::time::sleep;

use crate::form::FormService;
use crate::rank::{self, RankService};
use crate::state::{AppState, MatchState, MyCard, Player, PlayerStats};
use crate::wmpvp;

/// 占位玩家: (steamId 后缀, 昵称, 基础段位分, 近五场胜负) —— 5v5, 基线覆盖非精英档位;
/// 每局随机选 1 名玩家(非 S)提升为精英C+/B+/A+ 随机一档
const ROSTER: &[(&str, &str, f64, &[bool])] = &[
    ("00000011", "调试玩家", 2100.0, &[true, false, true, true, false]),   // A+
    ("00000012", "AceKiller", 1960.0, &[true, true, true, false, true]),   // A
    ("00000013", "BananaPeel", 1680.0, &[false, true, false, true, true]), // B+
    ("00000014", "CobraStrike", 1560.0, &[true, false, false, true, false]), // B
    ("00000015", "Dust2Daddy", 1240.0, &[false, false, true, false, true]), // C+
    ("00000021", "EcoWarrior", 2460.0, &[true, true, false, true, true]),  // S(星级随机)
    ("00000022", "FlashMaster", 2520.0, &[true, true, true, true, false]), // S(星级随机)
    ("00000023", "GhostPeek", 1050.0, &[false, true, true, false, false]), // C
    ("00000024", "HeadshotHz", 980.0, &[true, false, true, false, false]), // D
    ("00000025", "IGL_站桩", 1250.0, &[false, false, false, true, false]), // C+
];

fn sid_of(suffix: &str) -> String {
    format!("7656119{}", suffix)
}

pub async fn run(
    state: Arc<AppState>,
    ranks: Arc<RankService>,
    forms: Arc<FormService>,
    my_sid: String,
) {
    tracing::info!(" mock 模式已启用: 使用占位数据, 不访问完美接口");
    let my = MyCard {
        nickname: "调试玩家".to_string(),
        steam_id: my_sid.clone(),
        avatar: None,
        rank_label: rank::rank_label_from_score(2100.0).to_string(),
        rank_score: Some(2100),
        ws_status: "idle".to_string(),
        last_error: String::new(),
        stats: Some(PlayerStats {
            avg_we: Some(9.1),
            rating_pro: Some(1.16),
            season_cnt: Some(144),
            adr: Some(89.2),
            win_rate: Some(0.51),
            kda: Some(1.01),
            headshot_ratio: Some(0.63),
            rws: Some(11.5),
            stars: Some(3),
            season_id: "S24".to_string(),
            summary: "神枪不朽".to_string(),
        }),
    };
    *state.my.lock().await = my;

    let mut round: u32 = 0;
    loop {
        // ---- 空闲阶段(账号卡) ----
        {
            let mut m = state.my.lock().await;
            m.ws_status = "idle".to_string();
        }
        *state.match_state.lock().await = None;
        sleep(Duration::from_secs(6)).await;

        // ---- 进局: 写入占位段位/战绩缓存(刷新 TTL, 保证零网络) ----
        round += 1;
        let my_sid_here = my_sid.clone();
        // 每局随机 1 名非 S 玩家成为精英段(C++/B++/A++ 三档随机选一)
        let h = round.wrapping_mul(2654435761);
        // 非固定 S 槽位里随机挑 1 个(idx5/6 是 S, 跳过)
        let j = (h % (ROSTER.len() as u32 - 2)) as usize;
        let elite_idx = if j >= 5 { j + 2 } else { j };
        let elite_score = match (h >> 16) % 3 {
            0 => 1350.0 + ((h >> 8) % 100) as f64, // 精英C+
            1 => 1800.0 + ((h >> 8) % 100) as f64, // 精英B+
            _ => 2250.0 + ((h >> 8) % 150) as f64, // 精英A+
        };
        for (i, (suffix, nick, score, form)) in ROSTER.iter().enumerate() {
            let sid = if i == 0 {
                my_sid_here.clone()
            } else {
                sid_of(suffix)
            };
            let real_score = if i == elite_idx { elite_score } else { *score };
            let user = wmpvp::User {
                nickname: Some(nick.to_string()),
                steam_id: Some(sid.clone()),
                avatar: None,
                score: Some(serde_json::json!(real_score)),
            };
            ranks.seed_user(&sid, user).await;
            forms.seed(&sid, form.to_vec()).await;
            // S 段玩家: 每局伪随机星级, 两个 S 分别覆盖低档(普通/金色/钻石)与魔王档
            if *score > 2400.0 {
                let rand = (round.wrapping_mul(2654435761)
                    ^ (i as u32).wrapping_mul(40503))
                    % 101;
                let stars = if i % 2 == 0 { rand % 50 } else { 50 + rand % 51 };
                ranks.seed_stars(&sid, stars).await;
            }
        }

        let players = ROSTER
            .iter()
            .enumerate()
            .map(|(i, (suffix, _nick, _s, _f))| {
                let sid = if i == 0 {
                    my_sid.clone()
                } else {
                    sid_of(suffix)
                };
                Player {
                    side: if i < 5 { "CT" } else { "T" }.to_string(),
                    name: None, // 强制走缓存昵称, 与真实路径一致
                    steam_id: sid,
                    kill: 24 - (i as u32) * 2,
                    death: 10 + (i as u32),
                    assist: 6 - (i as u32 % 5),
                    adr: Some(95.0 - (i as f64) * 6.5),
                    alive: i % 2 == 0,
                    rating: Some(1.5 - (i as f64) * 0.1),
                }
            })
            .collect::<Vec<_>>();
        {
            let mut m = state.my.lock().await;
            m.ws_status = "in-match".to_string();
        }
        *state.match_state.lock().await = Some(MatchState {
            match_id: format!("MOCK-{round}"),
            map: "de_mirage".to_string(),
            ct_score: 4,
            t_score: 3,
            ct_half: Some(3),
            t_half: Some(2),
            players,
        });
        *state.updated_at.lock().await = now_millis();
        tracing::info!(round, "mock: 对局开始");

        // ---- 局内: 比分缓慢推进(观察原位刷新不重播动画) ----
        for _ in 0..8 {
            sleep(Duration::from_secs(3)).await;
            let mut ms = state.match_state.lock().await;
            if let Some(m) = ms.as_mut() {
                m.ct_score += 1;
                if m.ct_score % 3 == 0 {
                    m.t_score += 1;
                }
            }
            *state.updated_at.lock().await = now_millis();
        }

        // ---- 结束: 清对局回空闲, 前端播出场动画后回空闲 ----
        *state.match_state.lock().await = None;
        state.my.lock().await.ws_status = "idle".to_string();
        *state.updated_at.lock().await = now_millis();
        tracing::info!(round, "mock: 对局结束");
    }
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
