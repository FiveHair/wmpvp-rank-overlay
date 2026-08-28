//! 完美世界竞技平台(wmpvp)公开接口客户端。
//!
//! - `search/user` 按 steamId 反查用户, 返回昵称/头像/`pvpScore`(天梯段位分), 无需登录。
//! - `getWebsocketInfo` 通过 MD5 签名换取实时对局推送的 WebSocket 地址。

use anyhow::{anyhow, bail, Result};
use md5::{Digest, Md5};
use serde::Deserialize;

pub const SECURITY_KEY: &str = "b2K%$5k*o^j!@Qp";
const SEARCH_URL: &str = "https://appengine.wmpvp.com/steamcn/app/search/user";
const WS_INFO_URL: &str = "https://appactivity.wmpvp.com/steamcn/match/watchStage/getWebsocketInfo";

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// `search/user` 返回的用户条目。字段用 Option 兼容缺失。
#[derive(Debug, Clone, Deserialize, Default)]
pub struct User {
    #[serde(alias = "pvpNickName", default)]
    pub nickname: Option<String>,
    #[serde(alias = "steamId", default)]
    pub steam_id: Option<String>,
    #[serde(alias = "pvpAvatar", default)]
    pub avatar: Option<String>,
    #[serde(alias = "pvpScore", alias = "aveScore", default)]
    pub score: Option<serde_json::Value>,
}

impl User {
    pub fn steam_id(&self) -> String {
        self.steam_id
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string()
    }

    pub fn score_num(&self) -> Option<f64> {
        match self.score.as_ref()? {
            serde_json::Value::Number(n) => n.as_f64(),
            serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
            _ => None,
        }
    }
}

fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 构造带浏览器指纹的 HTTP 客户端(桌面程序模拟浏览器请求头)。
pub fn build_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(UA)
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(15))
        .build()?)
}

/// 按昵称或 steamId 搜索用户(完美平台天梯反查接口)。
pub async fn search_user(client: &reqwest::Client, keyword: &str) -> Result<Vec<User>> {
    let resp = client
        .post(SEARCH_URL)
        .header("Accept", "application/json, text/plain, */*")
        .header("Referer", "https://client.wmpvp.com")
        .header("Origin", "https://news.wmpvp.com")
        .header("x-requested-with", "XMLHttpRequest")
        .header("Content-Type", "application/json;charset=UTF-8")
        .json(&serde_json::json!({ "keyword": String::from(keyword), "page": 1 }))
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    if body.get("code").and_then(|c| c.as_i64()) != Some(1) {
        bail!("search/user 返回异常: {}", body);
    }
    let users: Vec<User> = serde_json::from_value(body.get("result").cloned().unwrap_or_default())
        .unwrap_or_default();
    Ok(users)
}

/// 按 steamId 反查玩家信息 + 天梯段位分。精确匹配 steamId, 避免误取他人。
pub async fn search_by_steam_id(client: &reqwest::Client, steam_id: &str) -> Result<Option<User>> {
    let sid = steam_id.trim();
    let users = search_user(client, sid).await?;
    Ok(users
        .into_iter()
        .find(|u| u.steam_id() == sid))
}

/// 换取对局推送 WebSocket 地址。签名 = md5("steamId={sid}&securityKey={key}")。
pub async fn get_websocket_url(client: &reqwest::Client, steam_id: &str) -> Result<String> {
    let sid = steam_id.trim();
    let sign = md5_hex(&format!("steamId={}&securityKey={}", sid, SECURITY_KEY));
    let url = format!(
        "{}?steamId={}&platform=2&sign={}",
        WS_INFO_URL, sid, sign
    );
    let resp = client
        .get(&url)
        .header("Accept", "application/json, text/plain, */*")
        .header("Referer", "https://client.wmpvp.com")
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    if body.get("code").and_then(|c| c.as_i64()) != Some(1) {
        bail!("getWebsocketInfo 失败: {}", body);
    }
    body.pointer("/result/websocketUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("getWebsocketInfo 无 websocketUrl"))
}

// ================= 近几场胜负 match/list =================

const MATCH_LIST_URL: &str = "https://api.wmpvp.com/api/csgo/home/match/list";
/// match/list 需要新版 App 头(4.x), 否则拿不到天梯战绩
const MATCH_APP_VERSION: &str = "4.1.2.219";

/// 查询玩家最近几场天梯胜负(最新在前, true=胜)。
/// 参数与手机 App"最近战绩"页一致: dataSource=3(天梯) + csgoSeasonId="recent" + pvpType=-1(全部模式),
/// mySteamId 实测只能传 0(传真实 ID 返回 4013)。无战绩/接口异常返回空列表。
pub async fn fetch_recent_results(
    client: &reqwest::Client,
    token: &str,
    to_steam_id: &str,
    limit: usize,
) -> Result<Vec<bool>> {
    let to = to_steam_id.trim().to_string();
    if to.is_empty() || token.trim().is_empty() {
        return Ok(Vec::new());
    }
    let resp = client
        .post(MATCH_LIST_URL)
        .header("appversion", MATCH_APP_VERSION)
        .header("platform", "HarmonyOS")
        .header("gametype", "2")
        .header("gametypestr", "2")
        .header("token", token.trim())
        .header("User-Agent", "libcurl-agent/1.0")
        .json(&serde_json::json!({
            "mySteamId": 0,
            "toSteamId": to,
            "csgoSeasonId": "recent",
            "pvpType": -1,
            "page": 1,
            "pageSize": limit,
            "dataSource": 3,
        }))
        .send()
        .await?;
    let body: serde_json::Value = resp.json().await?;
    if body.get("statusCode").and_then(|c| c.as_i64()) != Some(0) {
        bail!("match/list 返回异常: {}", body);
    }
    let list = body
        .pointer("/data/matchList")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::with_capacity(limit);
    for item in list.into_iter().take(limit) {
        // team 与 winTeam 相同即胜; 字段缺失视为未知, 跳过该场
        let (Some(team), Some(win)) = (
            item.get("team").and_then(|v| v.as_i64()),
            item.get("winTeam").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        out.push(team == win);
    }
    Ok(out)
}

// ================= 赛季统计 detailStats =================

const DETAIL_STATS_URL: &str = "https://api.wmpvp.com/api/csgo/home/pvp/detailStats";
const APP_VERSION: &str = "3.5.4.172";

/// 完美电竞 App 个人主页统计(需登录 token, token 只需与 my_steam_id 匹配, to_steam_id 可查任意玩家)。
#[derive(Debug, Clone, Default)]
pub struct DetailStats {
    pub season_id: String,
    /// 平均 WE 制胜评价
    pub avg_we: Option<f64>,
    /// 完美 Rating(RatingPro)
    pub pw_rating: Option<f64>,
    /// 赛季场次
    pub cnt: Option<u32>,
    pub adr: Option<f64>,
    /// 胜率(0~1)
    pub win_rate: Option<f64>,
    pub kd: Option<f64>,
    /// 爆头率(0~1)
    pub head_shot_ratio: Option<f64>,
    pub rws: Option<f64>,
    /// 当前段位星级(0-7)
    pub stars: Option<u32>,
    /// 玩家评价(如"神枪不朽")
    pub summary: Option<String>,
}

/// 登录失效等业务错误, 与网络错误区分(便于上层降级提示)。
#[derive(Debug, Clone)]
pub enum StatsError {
    LoginExpired,
    Other(String),
}

/// 查询任意玩家(steam_id)的赛季统计。
/// `token` 为登录凭证(实测 mySteamId 填 0 即可查任意账号); `to_steam_id` 为被查询账号。
pub async fn fetch_detail_stats(
    client: &reqwest::Client,
    token: &str,
    to_steam_id: &str,
) -> std::result::Result<Option<DetailStats>, StatsError> {
    let to: i64 = to_steam_id.trim().parse().unwrap_or(0);
    if to == 0 || token.trim().is_empty() {
        return Ok(None);
    }
    let resp = client
        .post(DETAIL_STATS_URL)
        .header("Accept", "application/json, text/plain, */*")
        .header("appversion", APP_VERSION)
        .header("platform", "android")
        .header("token", token.trim())
        .header("User-Agent", "okhttp/4.9.2")
        .json(&serde_json::json!({
            "mySteamId": 0,
            "toSteamId": to,
            "accessToken": "",
        }))
        .send()
        .await
        .map_err(|e| StatsError::Other(format!("detailStats 网络错误: {}", e)))?;
    let body: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| StatsError::Other(format!("detailStats 响应解析失败: {}", e)))?;
    let code = body.get("statusCode").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code == 4001 {
        return Err(StatsError::LoginExpired);
    }
    if code != 0 {
        return Err(StatsError::Other(format!(
            "detailStats statusCode={}: {}",
            code,
            body.get("errorMessage").and_then(|e| e.as_str()).unwrap_or("")
        )));
    }
    let data = match body.get("data") {
        Some(d) if !d.is_null() => d,
        _ => return Ok(None),
    };
    let get = |k: &str| data.get(k).and_then(|v| v.as_f64());
    Ok(Some(DetailStats {
        season_id: data
            .get("seasonId")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        avg_we: get("avgWe"),
        pw_rating: get("pwRating"),
        cnt: data.get("cnt").and_then(|v| v.as_u64()).map(|v| v as u32),
        adr: get("adr"),
        win_rate: get("winRate"),
        kd: get("kd"),
        head_shot_ratio: get("headShotRatio"),
        rws: get("rws"),
        stars: data.get("stars").and_then(|v| v.as_u64()).map(|v| v as u32),
        summary: data.get("summary").and_then(|v| v.as_str()).map(String::from),
    }))
}
