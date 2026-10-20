//! 播客搜索：iTunes Search API（Apple 播客目录，AntennaPod 的默认目录之一）。
//!
//! 只负责“找一个 feed”：命中里的 `feedUrl` 交给现有的 `fetch_feed` 订阅
//! 链路，所以搜索不引入新数据模型——结果页只是一行候选，订阅后与手贴
//! RSS 走同一条路。
//!
//! 目录按国家分库（AntennaPod 也有国家设置）；这里先固定 `cn`，中文播客
//! 命中率优先。TODO 后续经宿主配置读地区 / 无结果时回退 US。

use anyhow::{Context, anyhow};
use rmus_plugin_sdk::http;
use serde::{Deserialize, Serialize};

/// 搜索命中。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub title: String,
    pub author: String,
    /// feed 地址（订阅身份）
    pub feed_url: String,
    /// 目录里登记的单集数（仅展示用）
    pub episodes: u64,
}

const SEARCH_API: &str = "https://itunes.apple.com/search";
/// 一次拉多少条（iTunes 上限 200；25 条足够一屏浏览）
const LIMIT: usize = 25;

/// 关键词搜索播客（网络 + 解析，必须在 worker 里跑）。
pub fn search(query: &str) -> anyhow::Result<Vec<Hit>> {
    let url = format!(
        "{SEARCH_API}?media=podcast&entity=podcast&limit={LIMIT}&country=cn&term={}",
        encode(query)
    );
    let body = http::Response::send(&http::Request::get(url))?
        .error_for_status()?
        .read_to_end()?;
    parse(&body)
}

/// 解析 iTunes 响应。非播客/已下架条目没有 `feedUrl`，直接跳过。
pub fn parse(body: &[u8]) -> anyhow::Result<Vec<Hit>> {
    let json: serde_json::Value =
        serde_json::from_slice(body).context("iTunes 搜索结果不是 JSON")?;
    let results = json
        .get("results")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("iTunes 响应缺少 results 数组"))?;
    Ok(results
        .iter()
        .filter_map(|r| {
            let feed_url = r.get("feedUrl").and_then(|v| v.as_str())?.trim();
            if feed_url.is_empty() {
                return None;
            }
            Some(Hit {
                title: r
                    .get("collectionName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("未命名播客")
                    .to_string(),
                author: r
                    .get("artistName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                feed_url: feed_url.to_string(),
                episodes: r.get("trackCount").and_then(|v| v.as_u64()).unwrap_or(0),
            })
        })
        .collect())
}

/// 查询串的 percent-encoding（wasm 侧不引 url crate；iTunes 只要求
/// 标准 UTF-8 百分号编码，空格编成 %20 比 `+` 更稳妥）。
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 中文关键词按_utf8_百分号编码() {
        // 硬核电台：4 个字 × 3 字节
        assert_eq!(
            encode("硬核电台"),
            "%E7%A1%AC%E6%A0%B8%E7%94%B5%E5%8F%B0"
        );
        assert_eq!(encode("a b&c"), "a%20b%26c");
        assert_eq!(encode("news-1_2.3~"), "news-1_2.3~");
    }

    #[test]
    fn 解析只留带_feedurl_的条目() {
        let body = r#"{"resultCount":2,"results":[
            {"collectionName":"硬核电台","artistName":"机核","feedUrl":
             "https://alioss.gcores.com/feeds/apple_podcast_v1.xml","trackCount":128},
            {"collectionName":"某单曲","artistName":"某人"}
        ]}"#;
        let hits = parse(body.as_bytes()).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "硬核电台");
        assert_eq!(hits[0].author, "机核");
        assert_eq!(hits[0].episodes, 128);
        assert!(hits[0].feed_url.starts_with("https://alioss.gcores.com/"));
    }

    #[test]
    fn 空结果与坏_json() {
        assert!(parse(br#"{"resultCount":0,"results":[]}"#).unwrap().is_empty());
        assert!(parse(b"not json").is_err());
    }
}
