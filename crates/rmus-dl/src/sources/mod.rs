//! 音乐源公共类型与工具（rmus-dl `sources/mod.rs` 的 wasm 同步版移植）。
//!
//! 差异：async → 同步（SDK http 阻塞调用）；`reqwest::Url` → `String`
//! （SDK http 用字符串 URL）；文件/tag IO 经 SDK fs 代理。

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use rmus_plugin_sdk::{fs, http};

pub mod kugou;
pub mod kuwo;

/// 极简 percent-encoding，仅用于拼搜索 URL 的关键词。
pub(crate) fn urlencode(input: &str) -> String {
    let mut out = String::new();
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// 酷我搜索结果里的 MINFO 品质信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct QualityInfo {
    pub flac: bool,
    pub flac_size: Option<u64>,
    pub mp3_label: Option<String>,
}

/// Range 探针结果：真实格式与精确总大小。
pub struct Probe {
    pub is_flac: bool,
    pub total: Option<u64>,
}

/// 下载前的最后确认：取 URL 开头几十字节看魔数，顺带从 Content-Range
/// 拿精确总大小。`expect_flac` 时魔数不是 fLaC 直接报错——"只要无损"
/// 的最终闸门。
pub(crate) fn probe_audio(url: &str, expect_flac: bool) -> Result<Probe> {
    let resp = http::Response::send(
        &http::Request::get(url).header("Range", "bytes=0-15"),
    )
    .context("探针请求失败")?
    .error_for_status()?;

    let mut head: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 64];
    while head.len() < 16 {
        let n = resp.read(&mut chunk).context("探针读 body 失败")?;
        if n == 0 {
            break;
        }
        let want = 16 - head.len();
        head.extend_from_slice(&chunk[..chunk.len().min(want)]);
    }

    let is_flac = head.starts_with(b"fLaC");
    if expect_flac && !is_flac {
        let got = if head.starts_with(b"ID3") || head.first().is_some_and(|b| b & 0xE0 == 0xE0) {
            "mp3"
        } else {
            "未知格式"
        };
        anyhow::bail!("上游返回的不是 FLAC（实际 {got}），已取消下载");
    }
    let total = resp
        .header("content-range")
        .and_then(|value| value.rsplit('/').next().and_then(|s| s.parse::<u64>().ok()));

    Ok(Probe { is_flac, total })
}

pub(crate) fn parse_minfo(minfo: &str) -> QualityInfo {
    let mut quality = QualityInfo { flac: false, flac_size: None, mp3_label: None };
    for entry in minfo.split(';') {
        let mut level = "";
        let mut format = "";
        let mut bitrate: u64 = 0;
        let mut size_mb: Option<f64> = None;
        for pair in entry.split(',') {
            let Some((key, value)) = pair.split_once(':') else { continue };
            match key.trim() {
                "level" => level = value.trim(),
                "format" => format = value.trim(),
                "bitrate" => bitrate = value.trim().parse().unwrap_or(0),
                "size" => {
                    size_mb = value.trim().strip_suffix("Mb").and_then(|s| s.parse().ok());
                }
                _ => {}
            }
        }
        match format {
            "flac" => {
                quality.flac = true;
                if quality.flac_size.is_none() {
                    quality.flac_size = size_mb.map(|mb| (mb * 1024.0 * 1024.0) as u64);
                }
            }
            "mp3" => {
                let better = quality
                    .mp3_label
                    .as_deref()
                    .and_then(|label| label.strip_suffix('k'))
                    .and_then(|k| k.parse::<u64>().ok())
                    .is_none_or(|best| bitrate > best);
                if better && bitrate > 0 {
                    quality.mp3_label = Some(format!("{bitrate}k"));
                }
            }
            _ => {}
        }
        let _ = level;
    }
    quality
}

/// 一条搜索结果：足够展示 + 回源取下载链接和元数据。
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub song_id: String,
    pub source: &'static str,
    pub cover_url: Option<String>,
    pub duration_ms: u64,
    pub format: Option<String>,
    pub size_bytes: Option<u64>,
}

/// 下载所需的补充元数据（歌词、封面）。
#[derive(Debug, Default)]
pub struct SongMetadata {
    pub lyric: Option<String>,
    pub cover_url: Option<String>,
}

impl SongMetadata {
    /// 门禁：歌词和封面至少要拿到一样。
    pub fn gate(&self) -> Result<()> {
        if self.lyric.is_none() && self.cover_url.is_none() {
            anyhow::bail!("metadata gate failed: neither lyric nor cover was resolved");
        }
        Ok(())
    }
}

/// 一个音乐源（同步版：SDK http 阻塞调用，插件线程独占模型）。
pub(crate) trait MusicSource {
    fn search(&self, keyword: &str) -> Result<Vec<SearchResult>>;
    fn get_download_url(&self, song: &SearchResult) -> Result<String>;
    fn get_metadata(&self, song: &SearchResult) -> Result<SongMetadata>;
}

/// 把歌词和封面写进音频文件的 tag（lofty 经 SDK File 的 IO 代理）。
/// `is_flac` 定显式 FileType——temp/无扩展名文件的自行探测会失败
/// （rmus-dl 同款坑）。门禁：没有歌词也没有封面时直接失败；写入后读回验证。
pub(crate) fn write_metadata(
    file_path: &Path,
    meta: &SongMetadata,
    is_flac: bool,
) -> Result<()> {
    use lofty::{
        config::WriteOptions,
        file::{AudioFile, FileType, TaggedFileExt},
        picture::{Picture, PictureType},
        tag::ItemKey,
    };
    use std::io::Cursor;

    meta.gate()?;
    let path_str = file_path.display().to_string();
    let file_type = if is_flac { FileType::Flac } else { FileType::Mpeg };

    // 读写打开（不截断）：lofty 就地修改 tag（rewind 重写 tag 区、音频原位）
    let mut file = fs::File::open_rw(&path_str)?;
    let mut tagged_file = lofty::probe::Probe::new(&mut file)
        .set_file_type(file_type)
        .read()
        .context("lofty 解析音频文件失败")?;

    let Some(tag) = tagged_file.primary_tag_mut() else {
        anyhow::bail!("No primary tag found in the audio file");
    };

    let lyrics_key = match tag.tag_type() {
        lofty::tag::TagType::Id3v2 => ItemKey::UnsyncLyrics,
        _ => ItemKey::Lyrics,
    };

    if let Some(lyric) = &meta.lyric {
        tag.insert_text(lyrics_key, lyric.clone());
    }

    if let Some(cover_url) = &meta.cover_url {
        let cover = http::Response::send(&http::Request::get(cover_url.clone()))
            .context("封面下载失败")?
            .error_for_status()?;
        let cover_bytes = cover.read_to_end().context("封面读取失败")?;
        let mut cursor = Cursor::new(cover_bytes);
        let mut picture = Picture::from_reader(&mut cursor)?;
        picture.set_pic_type(PictureType::CoverFront);
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(picture);
    }

    // 就地保存（同一句柄；lofty rewind 后重写 tag 区）
    tagged_file
        .save_to(&mut file, WriteOptions::default())
        .context("lofty 保存失败")?;
    drop(file);

    // 门禁复核：重新读文件（显式类型），确认要求写入的内容真的在 tag 里
    let mut verified_input = fs::File::open(&path_str).map_err(anyhow::Error::msg)?;
    let verified = lofty::probe::Probe::new(&mut verified_input)
        .set_file_type(file_type)
        .read()?;
    let Some(tag) = verified.primary_tag() else {
        anyhow::bail!("verification failed: no primary tag after save");
    };
    if meta.lyric.is_some() && tag.get_string(lyrics_key).is_none() {
        anyhow::bail!("verification failed: LYRICS missing from tag after save");
    }
    if meta.cover_url.is_some() && tag.pictures().is_empty() {
        anyhow::bail!("verification failed: picture missing from tag after save");
    }
    Ok(())
}

/// 临时文件路径工具：`.名字-<随机>.part`（点前缀不会被库扫描收录）。
pub(crate) fn temp_path(dest_dir: &Path, fallback_name: &str) -> PathBuf {
    let unique = rmus_plugin_sdk::now_ms() as u64;
    dest_dir.join(format!(
        ".{}.{unique:x}.part",
        sanitize_filename(&fallback_name.chars().take(40).collect::<String>())
    ))
}

/// 去掉文件名里的路径分隔符等常见非法字符。
pub(crate) fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

