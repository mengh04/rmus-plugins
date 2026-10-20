//! 酷狗音乐源：官方搜索接口 + baka.plus 第三方解析下载 + 官方 krcs 歌词接口。
//! 接口用法抄自 musicdl (CharlesPikachu/musicdl) 的 kugou.py。
//! rmus-dl 的 reqwest/async 版 → SDK http 同步版移植。

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde::Deserialize;

use super::{MusicSource, SearchResult, SongMetadata, urlencode};
use rmus_plugin_sdk::{http, sleep};

/// 可克隆：源里只有配置。
#[derive(Clone)]
pub struct Kugou {
    /// 只要无损：先查 songinfo 拿无损专属 sqhash 再解析，拿不到就报错
    flac_only: bool,
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    data: Option<SearchData>,
}

#[derive(Debug, Deserialize)]
struct SearchData {
    #[serde(default)]
    lists: Vec<SearchItem>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SearchItem {
    #[serde(rename = "FileHash")]
    file_hash: String,
    song_name: Option<String>,
    singer_name: Option<String>,
    album_name: Option<String>,
    file_name: Option<String>,
    /// 封面，一般带 {size} 占位符
    image: Option<String>,
    #[serde(rename = "trans_param")]
    trans_param: Option<TransParam>,
}

#[derive(Debug, Deserialize)]
struct TransParam {
    union_cover: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LyricSearchResponse {
    candidates: Option<Vec<LyricCandidate>>,
}

#[derive(Debug, Deserialize)]
struct LyricCandidate {
    /// 注意：JSON 里是字符串形式的数字
    id: String,
    accesskey: String,
}

#[derive(Debug, Deserialize)]
struct LyricDownloadResponse {
    content: Option<String>,
}

/// meting 式解析站（tom.moe / 90svip）的 POST 响应。
#[derive(Debug, Deserialize)]
struct MetingResolveResponse {
    data: Option<Vec<MetingItem>>,
    code: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct MetingItem {
    url: Option<String>,
}

/// songinfo 接口的 extra 对象：各档位的 hash 与字节数。
#[derive(Debug, Deserialize)]
struct QualityExtra {
    #[serde(rename = "sqhash")]
    sq_hash: Option<String>,
    #[serde(rename = "320hash")]
    hash_320: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SongQualityResponse {
    #[serde(default)]
    extra: Option<QualityExtra>,
}

/// http GET → JSON（错误信息带 URL 上下文）。
fn get_json<T: serde::de::DeserializeOwned>(url: &str) -> Result<T> {
    let resp = http::Response::send(&http::Request::get(url.to_string()))?;
    let body = resp.read_to_end()?;
    serde_json::from_slice(&body).with_context(|| format!("parse response: {url}"))
}

impl Kugou {
    pub fn new(flac_only: bool) -> Result<Self> {
        Ok(Self { flac_only })
    }

    /// 单曲品质信息：songinfo 接口的 extra 里按档位给了各格式的专属
    /// hash 与精确大小。搜索返回的 hash 常是 128k 档的，无损必须用
    /// 这里的 sqhash 去解析，否则拿到手的经常是 mp3。
    fn song_quality(&self, hash: &str) -> Result<QualityExtra> {
        let url = format!(
            "https://m.kugou.com/app/i/getSongInfo.php?cmd=playInfo&hash={hash}"
        );
        let resp: SongQualityResponse = get_json(&url)?;
        resp.extra.ok_or_else(|| anyhow::anyhow!("kugou songinfo: no extra"))
    }

    /// 逐候选解析站拿直链。expect_flac 时对每个候选做 Range 探针验魔数，
    /// 不是 flac 就换下一个（解析站经常静默降级 mp3）；非 flac 模式取第一个。
    fn resolve_hash(&self, hash: &str, expect_flac: bool) -> Result<String> {
        let mut candidates: Vec<String> = Vec::new();
        let mut last_err = None;

        // 1) baka：GET 302 到酷狗 CDN（重试 2 次）
        for attempt in 0..2 {
            let url = format!(
                "https://api.baka.plus/meting/?server=kugou&type=url&id={}&br=2000",
                hash
            );
            match http::Response::send(&http::Request::get(url.clone())) {
                Ok(resp) => {
                    let final_url = resp.url();
                    if !final_url.contains("api.baka.plus") {
                        candidates.push(downgrade_to_http(&final_url));
                        break;
                    }
                    last_err = Some(anyhow::anyhow!(
                        "baka did not redirect (status {})",
                        resp.status()
                    ));
                }
                Err(e) => last_err = Some(anyhow::anyhow!("{e}")),
            }
            if attempt == 0 {
                sleep(800);
            }
        }

        // 2) meting 式解析站
        for base in ["https://music.tom.moe/", "https://music.90svip.cn/"] {
            match resolve_with_meting_site(base, hash) {
                Ok(url) => candidates.push(downgrade_to_http(&url)),
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        if candidates.is_empty() {
            bail!("kugou: all resolvers failed for hash {hash}: {last_err:?}");
        }
        for url in candidates {
            if !expect_flac {
                return Ok(url);
            }
            if super::probe_audio(&url, true).is_ok() {
                return Ok(url);
            }
        }
        bail!("kugou: 所有解析站都给不出 flac for hash {hash}")
    }
}

impl MusicSource for Kugou {
    fn search(&self, keyword: &str) -> Result<Vec<SearchResult>> {
        let url = format!(
            "https://songsearch.kugou.com/song_search_v2?keyword={}&platform=WebFilter&page=1&pagesize=30",
            urlencode(keyword)
        );
        let resp: SearchResponse = get_json(&url)?;
        let lists = resp.data.map(|d| d.lists).unwrap_or_default();
        Ok(lists
            .into_iter()
            .filter(|item| !item.file_hash.is_empty())
            .map(|item| {
                let cover_url = item
                    .trans_param
                    .and_then(|tp| tp.union_cover)
                    .or(item.image)
                    .map(|u| u.replace("{size}", "400"));
                SearchResult {
                    title: item.song_name.clone().unwrap_or_else(|| {
                        item.file_name.clone().unwrap_or_else(|| item.file_hash.clone())
                    }),
                    artist: item.singer_name,
                    album: item.album_name,
                    song_id: item.file_hash,
                    source: "kugou",
                    cover_url,
                    // 搜索接口不带品质信息；下载时会查 songinfo 补齐
                    duration_ms: 0,
                    format: None,
                    size_bytes: None,
                }
            })
            .collect())
    }

    /// 解析下载直链。flac_only 时先查 songinfo 拿无损专属 sqhash
    /// （搜索 hash 常是 128k 档的），逐候选解析站探活直到拿到真 flac；
    /// 非 flac_only 时优先用 songinfo 的 320hash 精确拿 320k。
    fn get_download_url(&self, song: &SearchResult) -> Result<String> {
        if self.flac_only {
            let extra = self.song_quality(&song.song_id)?;
            let Some(sq_hash) = extra.sq_hash.filter(|h| !h.is_empty()) else {
                let hint = if extra.hash_320.is_some() {
                    "仅 320k mp3"
                } else {
                    "无无损档"
                };
                bail!("kugou: 无 flac 版本 for {}（{hint}）", song.title);
            };
            return self.resolve_hash(&sq_hash, true);
        }
        let hash = match self.song_quality(&song.song_id) {
            Ok(extra) => extra
                .hash_320
                .filter(|h| !h.is_empty())
                .unwrap_or_else(|| song.song_id.clone()),
            Err(_) => song.song_id.clone(),
        };
        self.resolve_hash(&hash, false)
    }

    fn get_metadata(&self, song: &SearchResult) -> Result<SongMetadata> {
        // 歌词：krcs 搜索候选，再下载 base64 的 lrc
        let keyword = song
            .title
            .split('-')
            .next()
            .unwrap_or(&song.title)
            .trim()
            .to_string();
        let search_url = format!(
            "https://krcs.kugou.com/search?ver=1&man=yes&client=mobi&keyword={}&duration=-1&hash={}",
            urlencode(&keyword),
            song.song_id
        );
        let lyric = get_json::<LyricSearchResponse>(&search_url)
            .ok()
            .and_then(|r| r.candidates)
            .and_then(|c| c.into_iter().next())
            .and_then(|candidate| {
                let download_url = format!(
                    "https://lyrics.kugou.com/download?ver=1&client=pc&id={}&accesskey={}&fmt=lrc&charset=utf8",
                    candidate.id, candidate.accesskey
                );
                get_json::<LyricDownloadResponse>(&download_url)
                    .ok()
                    .and_then(|r| r.content)
                    .and_then(|content| {
                        base64::engine::general_purpose::STANDARD
                            .decode(content)
                            .ok()
                            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    })
            });

        let cover_url = song.cover_url.clone();
        Ok(SongMetadata { lyric, cover_url })
    }
}

/// meting 式解析站：POST hash 拿相对 url，HEAD 跟随重定向到 CDN 直链。
fn resolve_with_meting_site(base: &str, hash: &str) -> Result<String> {
    let form = format!(
        "input={}&filter=id&type=kugou&page=1",
        urlencode(hash)
    );
    let resp = http::Response::send(
        &http::Request::post_form(base.to_string(), &form)
            .header("Referer", base)
            .header("X-Requested-With", "XMLHttpRequest"),
    )?
    .error_for_status()?;
    let body = resp.read_to_end()?;
    let parsed: MetingResolveResponse = serde_json::from_slice(&body)?;
    if parsed.code == Some(0) {
        bail!("meting site {base} returned code 0");
    }
    let relative = parsed
        .data
        .unwrap_or_default()
        .into_iter()
        .find_map(|item| item.url)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| anyhow::anyhow!("meting site {base} returned no url"))?;
    let full = join_url(base, &relative);
    // 跟随重定向拿 CDN 直链（不下载 body）
    let head = http::Response::send(&http::Request::get(full))?.error_for_status()?;
    Ok(head.url())
}

/// https → http 降级：酷狗部分 CDN 节点的证书不覆盖自己的域名。
fn downgrade_to_http(url: &str) -> String {
    url.replacen("https://", "http://", 1)
}

/// 极简 URL join：相对路径拼到 base 的根（meting 返回的都是绝对路径）。
fn join_url(base: &str, relative: &str) -> String {
    if relative.starts_with("http://") || relative.starts_with("https://") {
        return relative.to_string();
    }
    let root = base
        .split('/')
        .take(3)
        .collect::<Vec<_>>()
        .join("/");
    format!("{root}{relative}")
}
