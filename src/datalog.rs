//! 数据日志: exe 同级 data.log, 输出程序获取到的原始数据(WS 流/击杀/账号),
//! 供本地插件(file: 源)或外部工具读取筛选。
//!
//! 每次启动清空重建(计数类插件按本次运行统计);
//! 超过 20MB 自动清空, 防止长期运行撑满磁盘。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

const MAX_SIZE: u64 = 20 * 1024 * 1024;

pub struct DataLog {
    file: Option<File>,
    path: PathBuf,
    /// 同 key 内容去重(如 ws10002 只在变化时写)
    last: HashMap<String, String>,
    writes: u32,
}

impl DataLog {
    /// 创建并清空 exe 同级 data.log
    pub fn create() -> DataLog {
        let mut p = std::env::current_exe().unwrap_or_default();
        p.set_file_name("data.log");
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&p);
        if let Err(e) = &file {
            tracing::warn!(error = %e, "数据日志打开失败");
        }
        DataLog {
            file: file.ok(),
            path: p,
            last: HashMap::new(),
            writes: 0,
        }
    }

    /// 同 key 内容变化才写(WS 流等周期消息去重)
    pub fn line(&mut self, key: &str, body: &str) {
        if self.last.get(key).map(String::as_str) == Some(body) {
            return;
        }
        self.last.insert(key.to_string(), body.to_string());
        self.raw(body);
    }

    /// 无条件写一行: "[本地时间] body"
    pub fn raw(&mut self, body: &str) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        if let Some(f) = self.file.as_mut() {
            let _ = writeln!(f, "[{}] {}", ts, body);
        }
        self.writes += 1;
        if self.writes % 200 == 0 && std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0) > MAX_SIZE
        {
            let reopened = OpenOptions::new().write(true).truncate(true).open(&self.path);
            match reopened {
                Ok(f) => self.file = Some(f),
                Err(e) => tracing::warn!(error = %e, "数据日志清空失败"),
            }
        }
    }
}
