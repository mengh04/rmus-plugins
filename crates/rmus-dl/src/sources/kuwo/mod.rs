//! 酷我音乐源：官方搜索接口 + mobi.s DES 加密下载解析 + h5 歌词/封面接口。
//! 接口用法抄自 musicdl (CharlesPikachu/musicdl) 的 kuwo.py。
//! rmus-dl 的 reqwest/async 版 → SDK http 同步版移植。

pub mod des;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{MusicSource, SearchResult, SongMetadata, urlencode};
use rmus_plugin_sdk::{http, sleep};

/// 可克隆：源里只有配置，克隆进下载任务零成本。
#[derive(Clone)]
pub struct Kuwo {
    /// 只要无损：mobi.s 只请求 flac，不再降级 mp3（mobi.s 的 mp3 只有 128k）
    flac_only: bool,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    abslist: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
struct SearchItem {
    #[serde(rename = "MUSICRID")]
    musicrid: String,
    name: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    /// 时长（秒）；搜索接口直接给出
    duration: Option<String>,
    /// 音质清单："level:ff,bitrate:2000,format:flac,size:52.83Mb;..."
    minfo: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SongInfoResponse {
    data: Option<SongInfoData>,
}

#[derive(Debug, Deserialize)]
struct SongInfoData {
    songinfo: Option<SongInfo>,
    lrclist: Option<Vec<LrcLine>>,
}

#[derive(Debug, Deserialize)]
struct SongInfo {
    pic: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LrcLine {
    #[serde(rename = "lineLyric")]
    line_lyric: String,
    time: String,
}

impl Kuwo {
    pub fn new(flac_only: bool) -> Result<Self> {
        Ok(Self { flac_only })
    }

    /// mobi.s 下载接口：DES 加密的 query 换取直链，响应是 key=value 文本。
    fn parse_download_url(&self, rid: &str, format: &str) -> Result<String> {
        let query = format!(
            "user=0&corp=kuwo&source=kwplayer_ar_5.1.0.0_B_jiakong_vh.apk&p2p=1&type=convert_url2&sig=0&format={format}&rid={rid}"
        );
        let q = des::encrypt_query(&query);
        let resp = http::Response::send(
            &http::Request::get(format!("http://mobi.kuwo.cn/mobi.s?f=kuwo&q={q}"))
                .header("User-Agent", "okhttp/3.10.0"),
        )?
        .error_for_status()?;
        let text = String::from_utf8_lossy(&resp.read_to_end()?).into_owned();
        for line in text.lines() {
            if let Some(url) = line.strip_prefix("url=")
                && url.starts_with("http")
            {
                return Ok(url.to_string());
            }
        }
        bail!("mobi.s returned no url for rid {rid} format {format}:\n{text}");
    }
}

impl MusicSource for Kuwo {
    /// mobi.s 下载解析：flac_only 时只请求 flac（mobi.s 的 mp3 档实测只有
    /// 128k，对"只要无损"没有意义），失败时错误里带上搜索期的品质标注。
    fn get_download_url(&self, song: &SearchResult) -> Result<String> {
        let formats: &[&str] = if self.flac_only { &["flac"] } else { &["flac", "mp3"] };
        let mut last_err = None;
        for format in formats {
            match self.parse_download_url(&song.song_id, format) {
                Ok(url) => return Ok(url),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }
        let hint = match (&song.format, last_err) {
            (Some(f), _) => format!("（搜索标注：{f}）"),
            (None, Some(e)) => format!("（{e}）"),
            (None, None) => String::new(),
        };
        bail!("kuwo: 无可下载版本 for {}{hint}", song.title)
    }

    fn search(&self, keyword: &str) -> Result<Vec<SearchResult>> {
        let url = format!(
            "https://www.kuwo.cn/search/searchMusicBykeyWord?vipver=1&client=kt&ft=music&cluster=0&strategy=2012&encoding=utf8&rformat=json&mobi=1&issubtitle=1&show_copyright_off=1&pn=0&rn=30&all={}",
            urlencode(keyword)
        );
        let resp = http::Response::send(&http::Request::get(url))?.error_for_status()?;
        let body = resp.read_to_end()?;
        let parsed: SearchResponse =
            serde_json::from_slice(&body).context("parse kuwo search response")?;
        Ok(parsed
            .abslist
            .into_iter()
            .filter_map(|item| {
                let rid = item.musicrid.strip_prefix("MUSIC_")?.to_string();
                let quality = item.minfo.as_deref().map(super::parse_minfo);
                let format = match &quality {
                    Some(q) if q.flac => Some("flac".to_string()),
                    Some(q) => q.mp3_label.clone(),
                    None => None,
                };
                let size_bytes = quality.and_then(|q| if q.flac { q.flac_size } else { None });
                Some(SearchResult {
                    title: item.name.clone().unwrap_or_default(),
                    artist: item.artist,
                    album: item.album,
                    song_id: rid,
                    source: "kuwo",
                    cover_url: None,
                    duration_ms: item
                        .duration
                        .and_then(|d| d.parse::<u64>().ok())
                        .unwrap_or(0)
                        * 1000,
                    format,
                    size_bytes,
                })
            })
            .collect())
    }

    fn get_metadata(&self, song: &SearchResult) -> Result<SongMetadata> {
        let url = format!(
            "https://m.kuwo.cn/newh5/singles/songinfoandlrc?musicId={}",
            song.song_id
        );
        // 接口间歇性返回空 data / 业务码 301（约一半概率），重试八次（间隔递增）。
        // 实测必须带 Referer: https://m.kuwo.cn/ 才有较高成功率；不带则几乎必失败。
        let mut last_err = anyhow::anyhow!("kuwo songinfo: empty data after 8 attempts");
        for attempt in 0..8 {
            match http::Response::send(
                &http::Request::get(url.clone()).header("Referer", "https://m.kuwo.cn/"),
            )
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.read_to_end())
            .and_then(|body| {
                serde_json::from_slice::<SongInfoResponse>(&body)
                    .map_err(|e| format!("parse kuwo songinfo response: {e}"))
                    .map_err(anyhow::Error::msg)
            }) {
                Ok(r) => match r.data {
                    Some(data) => {
                        let lyric = data.lrclist.map(|lines| {
                            lines
                                .iter()
                                .map(|l| format!("[{}]{}", to_lrc_time(&l.time), l.line_lyric))
                                .collect::<Vec<_>>()
                                .join("\r\n")
                        });
                        let cover_url =
                            data.songinfo.and_then(|info| info.pic).filter(|pic| {
                                pic.starts_with("http://") || pic.starts_with("https://")
                            });
                        return Ok(SongMetadata { lyric, cover_url });
                    }
                    None => {
                        last_err =
                            anyhow::anyhow!("kuwo songinfo: empty data (attempt {})", attempt + 1);
                    }
                },
                Err(e) => {
                    last_err = e.context("parse kuwo songinfo response");
                }
            }
            sleep(800 * (attempt as u64 + 1));
        }
        Err(last_err)
    }
}

/// "123.45"（秒）→ "02:03.45"
fn to_lrc_time(time: &str) -> String {
    let secs: f64 = time.parse().unwrap_or(0.0);
    let total_ms = (secs * 1000.0).round() as u64;
    format!(
        "{:02}:{:02}.{:02}",
        total_ms / 60000,
        (total_ms / 1000) % 60,
        (total_ms % 1000) / 10
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn lrc_time_format() {
        assert_eq!(super::to_lrc_time("0.0"), "00:00.00");
        assert_eq!(super::to_lrc_time("123.45"), "02:03.45");
        assert_eq!(super::to_lrc_time("2.25"), "00:02.25");
    }
}
