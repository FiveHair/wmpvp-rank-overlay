//! 本地 HTTP 服务: 供 OBS 浏览器源加载的覆盖层页面 + `/api/state` JSON 快照。
//!
//! 两个独立浏览器源:
//! - `/`      账号信息卡(始终显示)
//! - `/match` 当前对局板(每局展示一次: 两队从屏幕两侧向中间入场, 结束反向出场)
//!
//! 段位展示统一使用完美官方素材(抓自 pvp.wanmei.com 天梯页, 已存本地):
//! 非 S 段 = 盾牌底 + 字母 SVG + 分数; S 段 = 按星级的钻石图标(APNG 动画), 无分数。

use std::sync::Arc;

use axum::{
    extract::State as AxumState,
    http::{header, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};

use crate::form::FormService;
use crate::rank::RankService;
use crate::state::{build_api_state, AppState};

pub struct ServerCtx {
    pub app: Arc<AppState>,
    pub ranks: Arc<RankService>,
    pub forms: Arc<FormService>,
    pub cfg: tokio::sync::watch::Receiver<Arc<crate::monitor::MonitorConfig>>,
}

pub async fn serve(
    app: Arc<AppState>,
    ranks: Arc<RankService>,
    forms: Arc<FormService>,
    cfg_rx: tokio::sync::watch::Receiver<Arc<crate::monitor::MonitorConfig>>,
) -> anyhow::Result<()> {
    // 前后端解耦: 页面源码直接从 exe 同级 web/ 目录读取, 不内嵌不生成
    let ctx = Arc::new(ServerCtx {
        app,
        ranks,
        forms,
        cfg: cfg_rx.clone(),
    });
    let router = Router::new()
        .route("/", get(account_page))
        .route("/match", get(match_page))
        .route("/all", get(all_page))
        .route("/assets/:name", get(asset))
        .route("/assets/avatar.png", get(avatar_png))
        .route("/api/state", get(api_state))
        // web/ 下与页面同名的定制样式: /account.css、/match.css
        .route("/:name", get(web_css))
        .with_state(ctx);
    let mut cfg_rx = cfg_rx;
    let mut last_port: u16 = 0;
    loop {
        let port = cfg_rx.borrow_and_update().port;
        if port == last_port {
            if cfg_rx.changed().await.is_err() {
                return Ok(());
            }
            continue;
        }
        last_port = port;
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, port = port, "面板端口绑定失败, 稍后重试");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            }
        };
        tracing::info!(addr = %addr, "本地覆盖层面板已启动。账号卡: http://{}/  对局板: http://{}/match", addr, addr);
        tracing::info!(dir = %crate::config::web_dir().display(), "页面目录(web/, 修改此处 html 后刷新浏览器源即生效)");
        tokio::select! {
            r = axum::serve(listener, router.clone()) => {
                r?;
                return Ok(());
            }
            // 等待端口真正变化才重启; 其他配置变化(mock/token/steamId)不影响面板服务
            _ = wait_port_change(&mut cfg_rx, port) => {
                tracing::info!("端口配置已变化, 重启面板");
            }
        }
    }
}

/// 端口变化时返回; 其他配置字段变化忽略
async fn wait_port_change(
    cfg_rx: &mut tokio::sync::watch::Receiver<Arc<crate::monitor::MonitorConfig>>,
    cur: u16,
) {
    loop {
        if cfg_rx.changed().await.is_err() {
            return;
        }
        if cfg_rx.borrow().port != cur {
            return;
        }
    }
}

// ===== 页面与样式: 直接读 exe 同级 web/ 目录下的文件, 不内嵌不生成、不做其他查找。
// 同名 css 存在时自动注入 <link>。 =====

/// 账号信息卡页面(/)。
async fn account_page() -> axum::response::Response {
    html_page("account")
}

/// 对局板页面(/match)。
async fn match_page() -> axum::response::Response {
    html_page("match")
}

/// 全量数据示例页(/all): 展示 /api/state 的全部数据。
async fn all_page() -> axum::response::Response {
    html_page("all")
}

/// 页面组装: 读 web/<view>.html, 同名 css 存在时注入 <link>; 缺失时给出放置位置提示。
fn html_page(view: &str) -> axum::response::Response {
    let Some(mut body) = crate::config::read_web_file(&format!("{view}.html")) else {
        return (
            StatusCode::NOT_FOUND,
            format!(
                "未找到 web/{view}.html\n页面源码在仓库 web/ 目录, 程序按 exe 同级 web/ 优先、逐级向上查找。当前查找目录: {}",
                crate::config::web_dir().display()
            ),
        )
            .into_response();
    };
    if crate::config::read_web_file(&format!("{view}.css")).is_some() {
        body = body.replace(
            "</head>",
            &format!("<link rel=\"stylesheet\" href=\"/{view}.css\">\n</head>"),
        );
    }
    (
        [
            (header::CACHE_CONTROL, "no-cache, no-store, must-revalidate"),
            (header::PRAGMA, "no-cache"),
        ],
        axum::response::Html(body),
    )
        .into_response()
}

/// web/ 下与页面同名的定制样式(仅允许 *.css, 其余 404)。
async fn web_css(axum::extract::Path(name): axum::extract::Path<String>) -> impl IntoResponse {
    let safe = name.ends_with(".css")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.');
    if !safe {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match crate::config::read_web_file(&name) {
        Some(body) => {
            let mut resp = (StatusCode::OK, body).into_response();
            resp.headers_mut().insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/css; charset=utf-8"),
            );
            resp.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-cache"),
            );
            resp
        }
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn api_state(
    AxumState(ctx): AxumState<Arc<ServerCtx>>,
) -> impl IntoResponse {
    let anim_exit = ctx.cfg.borrow().anim_exit;
    let api = build_api_state(&ctx.app, &ctx.ranks, &ctx.forms, anim_exit).await;
    Json(api)
}

/// 本地缓存的头像 PNG(由 monitor 下载自完美 CDN)。
/// no-cache + 前端带 ?v=<头像URL> 版本参数, 切换账号后浏览器必定重新拉取。
async fn avatar_png(AxumState(ctx): AxumState<Arc<ServerCtx>>) -> impl IntoResponse {
    let bytes = ctx.app.avatar.lock().await.as_ref().map(|a| a.bytes.clone());
    match bytes {
        Some(b) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "image/png"),
                (header::CACHE_CONTROL, "no-cache, must-revalidate"),
            ],
            b,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// 嵌入的官方段位素材(SVG 字母/盾牌 + S 段钻石 APNG)。
async fn asset(axum::extract::Path(name): axum::extract::Path<String>) -> impl IntoResponse {
    let Some((body, ctype)) = ASSETS
        .iter()
        .find(|(n, _, _)| *n == name.as_str())
        .map(|(_, body, ct)| (*body, *ct))
    else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ctype),
            (header::CACHE_CONTROL, "public, max-age=86400"),
        ],
        body,
    )
        .into_response()
}

/// (文件名, 内容, Content-Type)。素材抓自完美官方天梯页, 存本地避免依赖外网 CDN。
#[rustfmt::skip]
const ASSETS: &[(&str, &[u8], &str)] = &[
    ("ABC_bg.svg",      include_bytes!("../assets/ABC_bg.svg"),      "image/svg+xml; charset=utf-8"),
    ("S.svg",           include_bytes!("../assets/S.svg"),           "image/svg+xml; charset=utf-8"),
    ("A.svg",           include_bytes!("../assets/A.svg"),           "image/svg+xml; charset=utf-8"),
    ("A1.svg",          include_bytes!("../assets/A1.svg"),          "image/svg+xml; charset=utf-8"),
    ("B.svg",           include_bytes!("../assets/B.svg"),           "image/svg+xml; charset=utf-8"),
    ("B1.svg",          include_bytes!("../assets/B1.svg"),          "image/svg+xml; charset=utf-8"),
    ("C.svg",           include_bytes!("../assets/C.svg"),           "image/svg+xml; charset=utf-8"),
    ("C1.svg",          include_bytes!("../assets/C1.svg"),          "image/svg+xml; charset=utf-8"),
    ("D.svg",           include_bytes!("../assets/D.svg"),           "image/svg+xml; charset=utf-8"),
    ("D1.svg",          include_bytes!("../assets/D1.svg"),          "image/svg+xml; charset=utf-8"),
    ("A11.svg",         include_bytes!("../assets/A11.svg"),         "image/svg+xml; charset=utf-8"),
    ("B11.svg",         include_bytes!("../assets/B11.svg"),         "image/svg+xml; charset=utf-8"),
    ("C11.svg",         include_bytes!("../assets/C11.svg"),         "image/svg+xml; charset=utf-8"),
    ("A11_bg.svg",      include_bytes!("../assets/A11_bg.svg"),      "image/svg+xml; charset=utf-8"),
    ("B11_bg.svg",      include_bytes!("../assets/B11_bg.svg"),      "image/svg+xml; charset=utf-8"),
    ("C11_bg.svg",      include_bytes!("../assets/C11_bg.svg"),      "image/svg+xml; charset=utf-8"),
    ("star.svg",        include_bytes!("../assets/star.svg"),        "image/svg+xml; charset=utf-8"),
    ("s-brass-s21.png",     include_bytes!("../assets/s-brass-s21.png"),     "image/png"),
    ("s-silver-1.png",      include_bytes!("../assets/s-silver-1.png"),      "image/png"),
    ("s-gold-1.png",        include_bytes!("../assets/s-gold-1.png"),        "image/png"),
    ("s-diamond-1.png",     include_bytes!("../assets/s-diamond-1.png"),     "image/png"),
];


