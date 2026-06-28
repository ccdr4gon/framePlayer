// 配置持久化：播放速度 + 自定义快捷键，存 %APPDATA%\FramePlayer\config.toml。

use crate::input::{default_bindings, Action, Binding};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// 保证每个动作恰好有一条绑定（缺失的用默认补齐、保持固定顺序），
/// 避免手改/旧版配置导致某个全局快捷键彻底失效且无法在界面里恢复。
fn normalize_bindings(loaded: &[Binding]) -> Vec<Binding> {
    let defaults = default_bindings();
    Action::ALL
        .iter()
        .map(|&a| {
            loaded
                .iter()
                .find(|b| b.action == a)
                .copied()
                .unwrap_or_else(|| defaults.iter().find(|b| b.action == a).copied().unwrap())
        })
        .collect()
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub play_interval_s: f64,
    pub hold_interval_s: f64,
    /// 代理(加速)缓存上限（MiB）。超出则按最旧(修改时间)淘汰；0 = 不限制。
    pub proxy_cache_max_mb: u64,
    pub bindings: Vec<Binding>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            play_interval_s: 0.040,
            hold_interval_s: 0.050,
            proxy_cache_max_mb: 4096, // 4 GiB
            bindings: default_bindings(),
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "FramePlayer")?;
    Some(dirs.config_dir().join("config.toml"))
}

impl Config {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let mut cfg: Config = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
                log::warn!("配置解析失败，使用默认值: {e}");
                Self::default()
            }),
            Err(_) => Self::default(),
        };
        cfg.bindings = normalize_bindings(&cfg.bindings);
        cfg
    }

    pub fn save(&self) {
        let Some(path) = config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match toml::to_string_pretty(self) {
            Ok(text) => {
                if let Err(e) = std::fs::write(&path, text) {
                    log::warn!("写入配置失败: {e}");
                }
            }
            Err(e) => log::warn!("序列化配置失败: {e}"),
        }
    }
}
