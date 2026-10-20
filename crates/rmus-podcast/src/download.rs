//! 单集下载（rmus-dl download.rs 的播客精简版）：http 流式 → 临时文件
//! → 原子改名。文件名直接用 EP 标题（播客常无 tag，不读 lofty）；
//! 扩展名按魔数（flac / mp3 / ogg / m4a / wav）——播客源里 m4a(AAC) 很常见，
//! 认错扩展名会让解码器按错误的容器解析。

use std::path::{Path, PathBuf};

use rmus_plugin_sdk::{cancelled, fs, http};

/// 去掉文件名里的路径分隔符等常见非法字符。
pub fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            _ => c,
        })
        .collect()
}

/// 单集落盘目录：`<data_dir>/podcasts/<slug>/`。
pub fn episode_dir(data_dir: &str, slug: &str) -> PathBuf {
    Path::new(data_dir).join("podcasts").join(sanitize_filename(slug))
}

/// 下载进度回调：已收字节 / 总字节（content-length 缺失时 None）。
pub struct Progress<'a> {
    pub on_chunk: &'a mut dyn FnMut(u64, Option<u64>),
}

/// 下载 enclosure 到 `dir/`，返回最终文件名（相对 dir）。
/// 取消安全：临时文件 `.title.<hex>.part`（无扩展名，音乐库扫描不收），
/// 取消/失败时删除。
pub fn download_episode(
    url: &str,
    title: &str,
    dir: &Path,
    progress: &mut Progress<'_>,
) -> anyhow::Result<String> {
    let response = http::Response::send(&http::Request::get(url.to_string()))?
        .error_for_status()?;
    let total_size = response.header("content-length").and_then(|v| v.parse().ok());

    let stem = sanitize_filename(&title.chars().take(80).collect::<String>());
    let unique = rmus_plugin_sdk::now_ms() as u64;
    let temp = dir.join(format!(".{stem}.{unique:x}.part"));
    let temp_str = temp.display().to_string();
    let mut file = fs::File::create(&temp_str)?;

    let mut magic = [0u8; 8];
    let mut received: u64 = 0;
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
        // 魔数在文件头：决定扩展名（m4a 的 "ftyp" 在第 4–8 字节，要多读 4 字节）
        let fill = (received as usize).min(magic.len());
        if fill < magic.len() {
            let need = magic.len() - fill;
            magic[fill..fill + need.min(n)].copy_from_slice(&chunk[..need.min(n)]);
        }
        file.write_all(&chunk[..n])?;
        received += n as u64;
        (progress.on_chunk)(received, total_size);
    }
    drop(file);

    let ext = match &magic {
        m if m[..4] == *b"fLaC" => "flac",
        m if m[..3] == *b"ID3" || (m[0] == 0xFF && m[1] & 0xE0 == 0xE0) => "mp3",
        m if m[..4] == *b"OggS" => "ogg",
        // ISO BMFF（m4a/mp4）：播客里大量 AAC 单集都是这个容器
        m if m[4..8] == *b"ftyp" => "m4a",
        m if m[..4] == *b"RIFF" => "wav",
        _ => "mp3", // 默认按 mp3（播客绝对主流）
    };
    let final_name = format!("{stem}.{ext}");
    let final_str = dir.join(&final_name).display().to_string();
    if fs::exists(&final_str) {
        let _ = fs::remove(&temp_str);
        anyhow::bail!("同名文件已存在：{final_str}");
    }
    if !fs::rename(&temp_str, &final_str) {
        let _ = fs::remove(&temp_str);
        anyhow::bail!("rename 失败：{temp_str} → {final_str}");
    }
    Ok(final_name)
}
