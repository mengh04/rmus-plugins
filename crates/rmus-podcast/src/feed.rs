//! podcast feed 解析：RSS 2.0 为骨 + iTunes namespace。
//!
//! 抄 AntennaPod 的 namespace 思路但按 local name 匹配（主流 feed 的
//! `itunes:` 前缀约定俗成；TODO v2 按 xmlns URI 严格识别）。要点：
//! - `<enclosure url length type>` 是单集音频灵魂（Empty 事件取属性）
//! - guid 缺失回退 hash(enclosure_url)（野生 feed 常见）
//! - `itunes:duration` 容错 `HH:MM:SS` / `MM:SS` / 纯秒数
//! - pubDate 只取显示日期（MM-DD），完整 RFC822 解析不值得引时间库

use std::collections::HashMap;

use anyhow::{Context, anyhow};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::store::Episode;

/// 解析结果：feed 级元数据 + 单集列表（feed 顺序）。
pub struct FeedParsed {
    pub title: String,
    pub author: String,
    pub episodes: Vec<Episode>,
}

/// 巨型 feed 的单集上限（状态类日更播客十年也就几千集，300 足够浏览）。
const MAX_EPISODES: usize = 300;

pub fn parse(xml: &str) -> anyhow::Result<FeedParsed> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut title = String::new();
    let mut author = String::new();
    let mut episodes: Vec<Episode> = Vec::new();

    // 层级：channel 直接子元素 vs item 子元素同名（title 等），按栈区分
    let mut in_item = 0usize;
    // 文本累积：当前正在收集的元素（Start..End 之间的 Text/CData）
    let mut text_target: Option<String> = None;
    let mut texts: HashMap<String, String> = HashMap::new();
    // 每个 item 的暂存（End(item) 时落盘）
    let mut cur: HashMap<String, String> = HashMap::new();
    let mut cur_attrs: HashMap<String, String> = HashMap::new();

    let mut run = || -> anyhow::Result<()> {
        loop {
            match reader.read_event()? {
                Event::Start(e) => {
                    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    if local == "item" {
                        in_item += 1;
                        cur.clear();
                        cur_attrs.clear();
                        continue;
                    }
                    // 元素属性先记（itunes:image href 之类 Start 形式）
                    grab_attrs(&e, &local, in_item > 0, &mut cur_attrs);
                    if text_target.is_none() && is_text_element(&local) {
                        text_target = Some(local.clone());
                        // 属性可能已给出值（如 itunes:duration 偶尔写成元素属性）
                    }
                }
                Event::Empty(e) => {
                    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    grab_attrs(&e, &local, in_item > 0, &mut cur_attrs);
                }
                Event::Text(t) => {
                    if let Some(target) = &text_target {
                        let decoded = t.unescape().context("feed 文本转义非法")?;
                        push_text(&mut texts, target, &decoded);
                    }
                }
                Event::CData(t) => {
                    if let Some(target) = &text_target {
                        let decoded = String::from_utf8_lossy(t.as_ref()).into_owned();
                        push_text(&mut texts, target, &decoded.trim());
                    }
                }
                Event::End(e) => {
                    let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                    if local == "item" {
                        in_item = in_item.saturating_sub(1);
                        if let Some(ep) = take_episode(&cur, &cur_attrs) {
                            if episodes.len() < MAX_EPISODES {
                                episodes.push(ep);
                            }
                        }
                        cur.clear();
                        cur_attrs.clear();
                        continue;
                    }
                    // 文本元素收口：channel 级进 feed 元数据，item 级进 cur
                    if let Some(value) = texts.remove(&local) {
                        if in_item > 0 {
                            cur.insert(local.clone(), value);
                        } else {
                            match local.as_str() {
                                "title" => title = value,
                                "author" => author = value,
                                _ => {}
                            }
                        }
                    }
                    if text_target.as_deref() == Some(local.as_str()) {
                        text_target = None;
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if title.is_empty() {
            return Err(anyhow!("feed 里没有 channel title（不是播客 RSS？）"));
        }
        Ok(())
    };
    run().map_err(|e| anyhow!("feed 解析失败: {e:#}"))?;

    Ok(FeedParsed { title, author, episodes })
}

/// channel 的 itunes:image href（Start/Empty 形式都在）。
fn grab_attrs(
    e: &quick_xml::events::BytesStart<'_>,
    local: &str,
    in_item: bool,
    attrs: &mut HashMap<String, String>,
) {
    let value = |name: &str| {
        e.try_get_attribute(name)
            .ok()
            .flatten()
            .map(|a| String::from_utf8_lossy(&a.value).into_owned())
    };
    if in_item && local == "enclosure" {
        if let Some(url) = value("url") {
            attrs.insert("enclosure_url".into(), url);
        }
        if let Some(len) = value("length") {
            attrs.insert("enclosure_length".into(), len);
        }
    }
    if local == "image" {
        if let Some(href) = value("href") {
            attrs.insert("image_href".into(), href);
        }
    }
}

fn is_text_element(local: &str) -> bool {
    matches!(
        local,
        "title" | "author" | "guid" | "pubDate" | "duration" | "summary" | "subtitle"
    )
}

fn push_text(texts: &mut HashMap<String, String>, target: &str, piece: &str) {
    texts.entry(target.to_string()).or_default().push_str(piece);
}

/// item 收口：暂存 + 属性 → Episode（缺 enclosure 的条目直接丢弃——
/// 不是可播的播客单集）。
fn take_episode(cur: &HashMap<String, String>, attrs: &HashMap<String, String>) -> Option<Episode> {
    let enclosure_url = attrs.get("enclosure_url")?.clone();
    let title = cur.get("title").cloned().unwrap_or_else(|| "未命名".into());
    let guid = cur
        .get("guid")
        .filter(|g| !g.is_empty())
        .cloned()
        .unwrap_or_else(|| Episode::fallback_guid(&enclosure_url));
    let duration_ms = cur
        .get("duration")
        .and_then(|d| parse_duration(d))
        .or_else(|| attrs.get("enclosure_length").and_then(|l| l.parse::<u64>().ok()))
        .unwrap_or(0);
    Some(Episode {
        guid,
        title,
        pub_date: cur
            .get("pubDate")
            .map(|d| format_pub_date(d))
            .unwrap_or_default(),
        enclosure_url,
        duration_ms,
        played: false,
        position_ms: 0,
        downloaded: None,
    })
}

/// `HH:MM:SS` / `MM:SS` / 纯秒数 → 毫秒（AntennaPod DurationParser 的
/// 容错精神，两种主流写法都接）。
fn parse_duration(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if let Some(secs) = raw.parse::<u64>().ok() {
        return Some(secs * 1000);
    }
    let parts: Vec<u64> = raw
        .split(':')
        .map(|p| p.trim().parse::<u64>().ok())
        .collect::<Option<_>>()?;
    let ms = match parts.len() {
        3 => parts[0] * 3600 + parts[1] * 60 + parts[2],
        2 => parts[0] * 60 + parts[1],
        1 => parts[0],
        _ => return None,
    };
    Some(ms * 1000)
}

/// RFC822 `Mon, 18 Sep 2026 04:00:00 GMT` → `09-18`；解析失败回原文。
fn format_pub_date(raw: &str) -> String {
    const MONTHS: [(&str, &str); 12] = [
        ("Jan", "01"), ("Feb", "02"), ("Mar", "03"), ("Apr", "04"),
        ("May", "05"), ("Jun", "06"), ("Jul", "07"), ("Aug", "08"),
        ("Sep", "09"), ("Oct", "10"), ("Nov", "11"), ("Dec", "12"),
    ];
    let parts: Vec<&str> = raw.split_whitespace().collect();
    // 月份名定位（星期可省略）：日期总在月份名前一个词
    let mon_idx = parts
        .iter()
        .position(|p| MONTHS.iter().any(|(m, _)| m == p));
    if let Some(i) = mon_idx
        && i >= 1
        && let Some((_, mon)) = MONTHS.iter().find(|(m, _)| *m == parts[i])
    {
        return format!("{mon}-{}", parts[i - 1]);
    }
    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
    <rss xmlns:itunes="http://www.itunes.com/dtds/podcast-1.0.dtd" version="2.0">
      <channel>
        <title>声动早咖啡</title>
        <itunes:author>声动传媒</itunes:author>
        <item>
          <title>周五突击复盘</title>
          <guid>ep-100</guid>
          <pubDate>Fri, 18 Sep 2026 04:00:00 GMT</pubDate>
          <itunes:duration>24:17</itunes:duration>
          <enclosure url="https://cdn.example.com/ep100.mp3" length="23456789" type="audio/mpeg"/>
        </item>
        <item>
          <title>无 guid 的野生集</title>
          <enclosure url="https://cdn.example.com/ep99.mp3" length="1" type="audio/mpeg"/>
        </item>
      </channel>
    </rss>"#;

    #[test]
    fn 解析_标题作者与单集() {
        let feed = parse(SAMPLE).unwrap();
        assert_eq!(feed.title, "声动早咖啡");
        assert_eq!(feed.author, "声动传媒");
        assert_eq!(feed.episodes.len(), 2);

        let ep = &feed.episodes[0];
        assert_eq!(ep.guid, "ep-100");
        assert_eq!(ep.pub_date, "09-18");
        assert_eq!(ep.duration_ms, 24 * 60 * 1000 + 17 * 1000);
        assert_eq!(ep.enclosure_url, "https://cdn.example.com/ep100.mp3");

        // guid 缺失 → hash(enclosure_url) 补位
        assert_eq!(
            feed.episodes[1].guid,
            Episode::fallback_guid("https://cdn.example.com/ep99.mp3")
        );
    }

    #[test]
    fn 时长_容错三种写法() {
        assert_eq!(parse_duration("3661"), Some(3661000));
        assert_eq!(parse_duration("24:17"), Some(24 * 60 * 1000 + 17 * 1000));
        assert_eq!(parse_duration("1:02:03"), Some(3723000));
        assert_eq!(parse_duration("abc"), None);
    }

    #[test]
    fn 日期_省略星期的写法也认() {
        assert_eq!(format_pub_date("18 Sep 2026 04:00:00 GMT"), "09-18");
        assert_eq!(format_pub_date("乱写的"), "乱写的");
    }
}
