//! 下载音频文件（rmus-dl `download.rs` 的 wasm 同步版移植）。
//! IO 全部经 SDK http/fs 宿主代理；格式按文件头自动识别。

use std::path::{Path, PathBuf};

use rmus_plugin_sdk::{cancelled, fs, http};

use crate::sources::{sanitize_filename, temp_path};

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

    let final_path = build_final_path(&temp_path, opts.dest_dir, fallback_name)?;
    let final_str = final_path.display().to_string();
    if fs::exists(&final_str) {
        let _ = fs::remove(&temp_str);
        anyhow::bail!("同名文件已存在：{final_str}");
    }
    if !fs::rename(&temp_str, &final_str) {
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

    // 读 tag 决定文件名
    let mut input = fs::File::open(&temp_str)?;
    let tagged_file = Probe::new(&mut input).set_file_type(ext.1).read()?;
    let final_path = if let Some(tag) = tagged_file.primary_tag()
        && let (Some(title), Some(artist)) = (tag.title(), tag.artist())
    {
        dest_dir.join(format!("{title} - {artist}.{}", ext.0))
    } else {
        dest_dir.join(format!("{}.{}", sanitize_filename(fallback_name), ext.0))
    };
    Ok(final_path)
}
