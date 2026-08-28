//! 配置文件: 与 exe 同目录的 config.json, 保存 token / steamId / 端口等。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ConfigFile {
    #[serde(default)]
    pub token: String,
    #[serde(default)]
    pub steam_id: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_poll_ms")]
    pub poll_ms: u64,
    /// 调试模式: 全部使用占位数据, 不访问任何完美接口(避免调试期间触发风控)。
    /// 调试完成后改为 false 重启即恢复真实数据。
    #[serde(default)]
    pub mock: bool,
    /// 插件模拟输出: 勾选模拟数据时, 插件不再真实抓取, 各变量输出随机递增的模拟值(便于调试前端动画)。
    #[serde(default)]
    pub mock_plugins: bool,
    /// 对局板动画退出: 勾选后入场动画播完展示 5 秒再倒序播放退出动画;
    /// 不勾选则一直展示直至下一局。
    #[serde(default = "default_true")]
    pub anim_exit: bool,
}

fn default_true() -> bool {
    true
}

fn default_port() -> u16 {
    8910
}

fn default_poll_ms() -> u64 {
    3000
}

pub fn config_path() -> PathBuf {
    let mut p = std::env::current_exe().unwrap_or_default();
    p.set_file_name("config.json");
    p
}

pub fn load() -> ConfigFile {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!(error = %e, path = %path.display(), "配置文件解析失败, 使用默认配置");
            ConfigFile::default()
        }),
        Err(_) => ConfigFile::default(),
    }
}

pub fn save(cfg: &ConfigFile) -> std::io::Result<()> {
    let path = config_path();
    let s = serde_json::to_string_pretty(cfg)?;
    std::fs::write(path, s)
}

/// web/ 目录: 前后端解耦, OBS 页面为该目录下的独立 HTML, 直接从磁盘读取(不内嵌)。
/// 只读 exe 同级 web/, 不做其他查找
pub fn web_dir() -> PathBuf {
    let mut p = config_path();
    p.set_file_name("web");
    p
}

fn web_file_path(name: &str) -> PathBuf {
    web_dir().join(name)
}

/// 读取定制页面(不存在返回 None)
pub fn read_web_file(name: &str) -> Option<String> {
    std::fs::read_to_string(web_file_path(name)).ok()
}

