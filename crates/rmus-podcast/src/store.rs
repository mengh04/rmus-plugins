//! 订阅数据模型与 JSON 存取（`data_dir()/podcasts/subscriptions.json`）。
//!
//! 模型抄 AntennaPod 的三层精简版：Podcast（feed_url 即身份）/
//! Episode（guid 即身份，缺失回退 hash(enclosure_url)）。进度/已播/
//! 下载状态挂在 Episode 上，feed 刷新时按 guid 合并保留。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use rmus_plugin_sdk::fs;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub podcasts: Vec<Podcast>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Podcast {
    /// feed URL，订阅身份（退订/刷新的寻址键）
    pub feed_url: String,
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    /// 身份；feed 缺 guid 时由 enclosure URL 哈希补位
    pub guid: String,
    pub title: String,
    /// 显示用日期（MM-DD；解析失败存原文）
    #[serde(default)]
    pub pub_date: String,
    pub enclosure_url: String,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub played: bool,
    /// 听到哪（毫秒；worker 采样回写）
    #[serde(default)]
    pub position_ms: u64,
    /// 已下载的落盘文件名（相对本播客目录；下载功能后续版本接入）
    #[serde(default)]
    pub downloaded: Option<String>,
}

impl Episode {
    /// guid 缺失时的身份回退：hash(enclosure_url)。
    pub fn fallback_guid(enclosure_url: &str) -> String {
        let mut hasher = DefaultHasher::new();
        enclosure_url.hash(&mut hasher);
        format!("{:016x}", hasher.finish())
    }

    /// 未播完判定（播放中的显示 ▶ 百分比）。
    pub fn in_progress(&self) -> bool {
        !self.played && self.position_ms > 0
    }
}

impl Store {
    pub fn find_mut(&mut self, feed_url: &str) -> Option<&mut Podcast> {
        self.podcasts.iter_mut().find(|p| p.feed_url == feed_url)
    }

    /// 播客落盘子目录名（feed 标题清理；TODO title 变更时已下载文件不迁移）。
    pub fn slug_of(p: &Podcast) -> String {
        crate::download::sanitize_filename(&p.title)
    }

    /// 单集的绝对落盘路径（已下载才有）。
    pub fn episode_path(&self, data_dir: &Path, pod: &Podcast, ep: &Episode) -> Option<PathBuf> {
        let file = ep.downloaded.as_ref()?;
        Some(
            Path::new(data_dir)
                .join("podcasts")
                .join(Self::slug_of(pod))
                .join(file),
        )
    }

    /// 路径反查：`.../podcasts/<slug>/<file>` → (podcast 下标, episode 下标)。
    /// worker 的进度记忆 / 连播决策用它把引擎播放的文件对回订阅库。
    pub fn find_by_path(&self, data_dir: &Path, path: &str) -> Option<(usize, usize)> {
        let path = Path::new(path);
        let rel = path.strip_prefix(data_dir.join("podcasts")).ok()?;
        let mut it = rel.iter().filter_map(|s| s.to_str());
        let slug = it.next()?;
        let file = it.next()?;
        let pi = self.podcasts.iter().position(|p| Self::slug_of(p) == slug)?;
        let ei = self.podcasts[pi]
            .episodes
            .iter()
            .position(|e| e.downloaded.as_deref() == Some(file))?;
        Some((pi, ei))
    }

    /// 连播的下一集：同播客里当前集的下一个（feed 顺序 = 新→旧，往更旧听）。
    pub fn next_episode(&self, feed_url: &str, guid: &str) -> Option<&Episode> {
        let p = self.podcasts.iter().find(|p| p.feed_url == feed_url)?;
        let i = p.episodes.iter().position(|e| e.guid == guid)?;
        p.episodes.get(i + 1)
    }

    /// feed 刷新合并：按 guid 匹配保留本地状态（played/position/
    /// downloaded），新集按 feed 顺序插入，feed 里已消失的集丢弃。
    pub fn merge_episodes(&mut self, feed_url: &str, title: String, author: String, fresh: Vec<Episode>) {
        let Some(p) = self.find_mut(feed_url) else {
            self.podcasts.push(Podcast {
                feed_url: feed_url.to_string(),
                title,
                author,
                episodes: fresh,
            });
            return;
        };
        p.title = title;
        p.author = author;
        p.episodes = fresh
            .into_iter()
            .map(|ep| {
                match p.episodes.iter().find(|old| old.guid == ep.guid) {
                    Some(old) => Episode {
                        played: old.played,
                        position_ms: old.position_ms,
                        downloaded: old.downloaded.clone(),
                        ..ep
                    },
                    None => ep,
                }
            })
            .collect();
    }
}

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join("podcasts").join("subscriptions.json")
}

/// 读取订阅库；文件不存在/损坏返回空库（不报错——首启体验）。
pub fn load(data_dir: &Path) -> Store {
    // wasm32 的 std::fs 是 stub，文件 IO 必须走 SDK 宿主代理
    let bytes = match fs::File::open(&store_path(data_dir).display().to_string()) {
        Ok(mut f) => {
            use std::io::Read;
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_err() {
                return Store::default();
            }
            buf
        }
        Err(_) => return Store::default(),
    };
    serde_json::from_slice(&bytes).unwrap_or_default()
}

/// 原子写：临时文件 + rename（半截文件不会污染订阅库）。
pub fn save(data_dir: &Path, store: &Store) -> anyhow::Result<()> {
    let path = store_path(data_dir);
    let path_str = path.display().to_string();
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(store)?;
    {
        let mut f = fs::File::create(&tmp.display().to_string())?;
        f.write_all(&bytes)?;
    }
    if !fs::rename(&tmp.display().to_string(), &path_str) {
        anyhow::bail!("rename 失败：{} → {}", tmp.display(), path_str);
    }
    Ok(())
}
