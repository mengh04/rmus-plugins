//! 下载音频文件（rmus-dl `download.rs` 的 wasm 同步版移植）。
//! IO 全部经 SDK http/fs 宿主代理；格式按文件头自动识别。

use std::path::{Path, PathBuf};

use rmus_plugin_sdk::{cancelled, fs, http};

use crate::sources::{normalize_artists, sanitize_filename, split_artists, temp_path, SearchResult};

/// 下载参数：产物目录、进度回调、取消探测。
pub struct DownloadOptions<'a> {
    /// 产物目录（= rmusd 的库根目录）
    pub dest_dir: &'a Path,
    pub on_chunk: &'a mut dyn FnMut(u64, Option<u64>),
}

/// 下载音频文件。格式按文件头自动识别（flac / mp3，酷狗的解析站可能给
/// 128k mp3），文件名优先用音频自带 tag 的 title/artist，没有 tag
/// （酷我 CDN 直链常见）时退回 fallback_name。
///
/// 先写隐藏临时文件（不会被音乐库扫描到），成功后原子改名落到 `dest_dir`。
pub fn download_file(
    url: &str,
    fallback_name: &str,
    search: &SearchResult,
    opts: &mut DownloadOptions<'_>,
) -> anyhow::Result<PathBuf> {
    let response = http::Response::send(&http::Request::get(url.to_string()))?
        .error_for_status()?;

    let total_size = response.header("content-length").and_then(|v| v.parse().ok());
    let temp_path = temp_path(opts.dest_dir, fallback_name);
    let temp_str = temp_path.display().to_string();
    let mut file = fs::File::create(&temp_str)?;

    let mut chunk = [0u8; 64 * 1024];
    loop {
        if cancelled() {
            drop(file);
            let _ = fs::remove(&temp_str);
            anyhow::bail!("已取消");
        }
        let n = response.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        file.write_all(&chunk[..n])?;
        (opts.on_chunk)(n as u64, total_size);
    }
    drop(file);

    let final_path = build_final_path(&temp_path, opts.dest_dir, fallback_name, search)?;
    let final_str = final_path.display().to_string();
    if fs::exists(&final_str) {
        let _ = fs::remove(&temp_str);
        anyhow::bail!("同名文件已存在：{final_str}");
    }
    if !fs::rename(&temp_str, &final_str) {
        let _ = fs::remove(&temp_str);
        anyhow::bail!("rename 失败：{temp_str} → {final_str}");
    }
    Ok(final_path)
}

/// 依据文件头魔数定扩展名，再读 tag 决定文件名。
/// 显式指定 FileType：temp 文件没有扩展名，lofty 自行探测会失败。
fn build_final_path(
    temp_path: &Path,
    dest_dir: &Path,
    fallback_name: &str,
    search: &SearchResult,
) -> anyhow::Result<PathBuf> {
    use lofty::file::{FileType, TaggedFileExt};
    use lofty::tag::Accessor;
    use lofty::probe::Probe;

    let temp_str = temp_path.display().to_string();
    let mut input = fs::File::open(&temp_str)?;
    let mut magic = [0u8; 4];
    std::io::Read::read_exact(&mut input, &mut magic)?;
    let ext = match &magic {
        b"fLaC" => ("flac", FileType::Flac),
        m if &m[..3] == b"ID3" || (m[0] == 0xFF && m[1] & 0xE0 == 0xE0) => ("mp3", FileType::Mpeg),
        _ => anyhow::bail!("unrecognized audio format (magic: {magic:?})"),
    };
    drop(input);

    // 读 tag 决定文件名：tag 优于搜索文本，但 tag 与搜索文本对不上时不可信，
    // 退回 fallback_name。CDN 自带 tag 常缺斤短两：合唱只写第一个歌手
    // （知否知否 → ARTIST=胡夏），整段 junk（"kuwo"/"kuwo"）也有。
    let mut input = fs::File::open(&temp_str)?;
    let tagged_file = Probe::new(&mut input).set_file_type(ext.1).read()?;
    let tagged = tagged_file
        .primary_tag()
        .and_then(|tag| {
            let (title, artist) = (tag.title()?, tag.artist()?);
            tag_matches_search(&title, &artist, search)
                .then(|| (title.to_string(), artist.to_string()))
        });
    // tag 里的 title/artist 也得过 sanitize：CDN tag 歌手串常用 “/” 分隔
    // （胡夏/郁可唯），不清理会把文件名拆成两级路径，rename 必败；
    // 歌手分隔符再归一成 “&”，与搜索文本、ARTIST 标签保持一致
    let final_path = if let Some((title, artist)) = tagged {
        dest_dir.join(format!(
            "{} - {}.{}",
            sanitize_filename(&title),
            sanitize_filename(&normalize_artists(&artist)),
            ext.0
        ))
    } else {
        dest_dir.join(format!("{}.{}", sanitize_filename(fallback_name), ext.0))
    };
    Ok(final_path)
}

/// 自带 tag 是否可信：标题要包含搜索标题（CDN tag 常带副标题尾巴），
/// 搜索歌手要逐一出现在 tag 歌手里——合唱 tag 只写第一个歌手（知否知否
/// → ARTIST=胡夏）就靠这条拦下。大小写与空白不敏感，歌手串按常见
/// 分隔符拆开比对；搜索歌手为空时只查标题。
fn tag_matches_search(tag_title: &str, tag_artist: &str, search: &SearchResult) -> bool {
    if tag_title.is_empty() || tag_artist.is_empty() || tag_title == tag_artist {
        return false;
    }
    let norm = |s: &str| s
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    if !norm(tag_title).contains(&norm(&search.title)) {
        return false;
    }
    match search.artist.as_deref().map(str::trim) {
        Some(artist) if !artist.is_empty() => {
            split_artists(artist).all(|name| norm(tag_artist).contains(&norm(name)))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::tag_matches_search;
    use crate::sources::SearchResult;

    fn search(title: &str, artist: &str) -> SearchResult {
        SearchResult {
            title: title.to_string(),
            artist: Some(artist.to_string()),
            album: None,
            song_id: "1".into(),
            source: "kuwo",
            cover_url: None,
            duration_ms: 0,
            format: None,
            size_bytes: None,
        }
    }

    #[test]
    fn duet_tag_missing_second_artist_is_distrusted() {
        let s = search("知否知否", "胡夏&郁可唯");
        assert!(!tag_matches_search("知否知否", "胡夏", &s));
    }

    #[test]
    fn complete_duet_tag_is_trusted() {
        let s = search("知否知否", "胡夏&郁可唯");
        assert!(tag_matches_search("知否知否", "胡夏/郁可唯", &s));
        assert!(tag_matches_search("知否知否", "胡夏、郁可唯", &s));
    }

    #[test]
    fn solo_tag_is_trusted() {
        let s = search("记得你", "郁可唯");
        assert!(tag_matches_search("记得你", "郁可唯", &s));
    }

    #[test]
    fn junk_or_self_equal_tag_is_distrusted() {
        let s = search("晴天", "周杰伦");
        assert!(!tag_matches_search("kuwo", "kuwo", &s));
        assert!(!tag_matches_search("", "", &s));
    }

    #[test]
    fn tag_title_with_subtitle_suffix_is_trusted() {
        let s = search("水中花", "郁可唯");
        assert!(tag_matches_search("水中花 (Live)", "郁可唯", &s));
    }

    #[test]
    fn case_and_whitespace_insensitive() {
        let s = search("Shape of You", "Ed Sheeran");
        assert!(tag_matches_search("shape of you", "ed sheeran", &s));
    }
}
