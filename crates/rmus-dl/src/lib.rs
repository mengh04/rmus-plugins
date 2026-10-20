//! rmus-dl：在线音乐源插件（酷我 / 酷狗）。混合插件 = 方法通道 + UI 画布。
//!
//! 方法协议与进程外版一致（search/download/cancel，形状见
//! docs/plugins.md）；`rmus dl` CLI 走方法通道。
//!
//! UI 画布（TUI「在线」tab 的替代品）：`PluginData submit` 注入搜索词 →
//! 后台 worker 跑 search → 结果列表渲染；CHAR 'd' 对选中项发起后台
//! download，进度条实时；CHAR 'x' 取消最近一个下载任务。全部长任务经
//! `task::spawn` 在 worker 实例执行，UI 线程零阻塞。

mod download;
mod sources;

use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Mutex;

use download::DownloadOptions;
use sources::{MusicSource, SearchResult, kuwo::Kuwo, kugou::Kugou, write_metadata};
use rmus_plugin_sdk::{declare_plugin_manifest, fs, library_dir, notify, task, ui};
use ui::style;

/// 只要无损（与 rmus-dl 的默认一致；TODO 经宿主读用户配置）。
const FLAC_ONLY: bool = true;

/// 任务号约定：搜索固定 1，下载从 100 起自增。
const SEARCH_TASK: u64 = 1;

declare_plugin_manifest! {
    name: "rmus-dl", version: "0.3.0", ui: true, tab: "在线",
    methods: [("search", "call"), ("download", "task"), ("cancel", "call")],
    handle: handle,
}

/// 客户端回传的 song 对象（下载时的寻址凭据 + 搜索期品质标注）。
#[derive(Debug, Deserialize)]
struct SongRef {
    title: String,
    #[serde(default)]
    artist: String,
    source: String,
    song_id: String,
    #[serde(default)]
    format: Option<String>,
}

impl From<&SongRef> for SearchResult {
    fn from(s: &SongRef) -> Self {
        Self {
            title: s.title.clone(),
            artist: (!s.artist.is_empty()).then_some(s.artist.clone()),
            album: None,
            song_id: s.song_id.clone(),
            source: match s.source.as_str() {
                "kugou" => "kugou",
                _ => "kuwo",
            },
            cover_url: None,
            duration_ms: 0,
            format: s.format.clone(),
            size_bytes: None,
        }
    }
}

fn handle(method: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    match method {
        "search" => search(params).map_err(|e| format!("{e:#}")),
        "download" => download(params).map_err(|e| format!("{e:#}")),
        "cancel" => Ok(serde_json::json!({"cancelled": true})),
        other => Err(format!("未知方法 “{other}”")),
    }
}

/// 串行查酷我 + 酷狗；单源失败不挡另一源（错误拼进结果说明）。
fn search(params: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if query.is_empty() {
        anyhow::bail!("缺少 query 参数");
    }
    let mut notes = Vec::new();
    let mut songs = Vec::new();

    match Kuwo::new(FLAC_ONLY) {
        Ok(k) => match k.search(&query) {
            Ok(list) => songs.extend(list.into_iter().map(song_out)),
            Err(e) => notes.push(format!("kuwo: {e}")),
        },
        Err(e) => notes.push(format!("kuwo: {e}")),
    }
    match Kugou::new(FLAC_ONLY) {
        Ok(k) => match k.search(&query) {
            Ok(list) => songs.extend(list.into_iter().map(song_out)),
            Err(e) => notes.push(format!("kugou: {e}")),
        },
        Err(e) => notes.push(format!("kugou: {e}")),
    }

    if songs.is_empty() {
        anyhow::bail!("没有结果（{}）", notes.join("; "));
    }
    Ok(serde_json::json!({ "songs": songs }))
}

/// 搜索结果 → 对外 JSON 形状（与 rmus TUI 的 OnlineSong 对齐）。
fn song_out(r: SearchResult) -> serde_json::Value {
    serde_json::json!({
        "title": r.title,
        "artist": r.artist.unwrap_or_default(),
        "album": r.album.unwrap_or_default(),
        "source": r.source,
        "song_id": r.song_id,
        "duration_ms": r.duration_ms,
        "format": r.format,
        "size_bytes": r.size_bytes,
    })
}

fn download(params: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let song: SongRef = serde_json::from_value(
        params.get("song").cloned().unwrap_or_default(),
    )
    .map_err(|_| anyhow::anyhow!("song 参数不合法"))?;

    let dest_dir = PathBuf::from(library_dir());
    if dest_dir.as_os_str().is_empty() {
        anyhow::bail!("缺少库目录（rmusd 未注入 library_dir）");
    }

    let search_result = SearchResult::from(&song);
    if FLAC_ONLY
        && search_result.source == "kuwo"
        && search_result.format.as_deref().is_some_and(|f| f != "flac")
    {
        let label = search_result.format.as_deref().unwrap_or("未知");
        anyhow::bail!("无 flac 版本（搜索标注：{label}）");
    }
    let fallback_probe = format!("{} - {}", song.title, song.artist);
    for ext in ["flac", "mp3", "m4a", "ogg"] {
        let candidate = dest_dir.join(format!("{fallback_probe}.{ext}"));
        if fs::exists(&candidate.display().to_string()) {
            anyhow::bail!("同名文件已存在：{}", candidate.display());
        }
    }

    match search_result.source {
        "kuwo" => {
            let source = Kuwo::new(FLAC_ONLY)?;
            finish_download(&source, &search_result, &dest_dir)
        }
        "kugou" => {
            let source = Kugou::new(FLAC_ONLY)?;
            finish_download(&source, &search_result, &dest_dir)
        }
        other => anyhow::bail!("未知来源 “{other}”"),
    }
}

/// 下载后半程（源已定）：直链 → 探针 → 下载 → 门禁 → 落盘。
fn finish_download<S: MusicSource>(
    source: &S,
    search_result: &SearchResult,
    dest_dir: &std::path::Path,
) -> anyhow::Result<serde_json::Value> {
    let url = source.get_download_url(search_result)?;

    // 下载前最后确认：Range 探针验魔数 + 精确大小。上游偷偷降级 mp3
    // 会在这里被拦下，而不是下完 30MB 才发现。
    let probe = sources::probe_audio(&url, FLAC_ONLY)?;
    if let Some(total) = probe.total {
        notify(&serde_json::json!({"total": total}));
    }

    let fallback_name = match &search_result.artist {
        Some(artist) => format!("{} - {}", search_result.title, artist),
        None => search_result.title.clone(),
    };
    let mut received: u64 = 0;
    let mut last_notify = rmus_plugin_sdk::now_ms().saturating_sub(150);
    let mut opts = DownloadOptions {
        dest_dir,
        on_chunk: &mut |chunk, total| {
            received += chunk;
            let now = rmus_plugin_sdk::now_ms();
            let finished = total.is_some_and(|t| received >= t);
            if !finished && now.saturating_sub(last_notify) < 150 {
                return;
            }
            last_notify = now;
            notify(&serde_json::json!({"received": received, "total": total}));
        },
    };
    let file_path = download::download_file(&url, &fallback_name, &mut opts)?;

    // 元数据是门禁：拿不到或写不进去，这次下载就算失败，清理产物
    notify(&serde_json::json!({"phase": "metadata"}));
    let meta_result = source
        .get_metadata(search_result)
        .and_then(|meta| write_metadata(&file_path, &meta, probe.is_flac));
    if let Err(e) = meta_result {
        let path_str = file_path.display().to_string();
        if !fs::remove(&path_str) {
            // 清理失败只留痕（宿主 stderr），不掩盖主错误
        }
        anyhow::bail!("元数据门禁失败：{e:#}");
    }

    Ok(serde_json::json!({"path": file_path}))
}

// ---- UI 画布：搜索结果列表 + 下载进度（状态全在插件侧）----

/// 一个下载条目在画布上的状态。
#[derive(Debug, Clone, PartialEq)]
enum DlState {
    Running,
    Done,
    Failed(String),
}

struct Download {
    /// 对应结果行（时长列显示进度）
    source: String,
    song_id: String,
    task_id: u64,
    received: u64,
    total: Option<u64>,
    state: DlState,
}

/// UI 状态（wasm 单线程；Mutex 只为满足 static 的 Sync 要求）。
struct UiState {
    query: String,
    searching: bool,
    search_err: Option<String>,
    songs: Vec<serde_json::Value>,
    selected: usize,
    /// 结果列表的窗口滚动（画布自行滚动；TUI 全量投影画布行）
    scroll: usize,
    downloads: Vec<Download>,
    next_task: u64,
}

impl UiState {
    fn new() -> Self {
        Self {
            query: String::new(),
            searching: false,
            search_err: None,
            songs: Vec::new(),
            selected: 0,
            downloads: Vec::new(),
            scroll: 0,
            next_task: 100,
        }
    }
}

static UI: Mutex<Option<UiState>> = Mutex::new(None);

fn with_ui<R>(f: impl FnOnce(&mut UiState) -> R) -> R {
    let mut guard = UI.lock().unwrap();
    let state = guard.get_or_insert_with(UiState::new);
    f(state)
}

fn ui_update(ev: i32) -> i32 {
    use rmus_plugin_sdk::event::{char_of, kind, KIND_CHAR, KIND_DATA, KIND_DOWN, KIND_UP};

    match kind(ev) {
        KIND_DATA => {
            // 后台任务事件：submit 回执 / 搜索结果 / 下载进度 / 完成
            let Some(data) = task::last_event() else {
                return 0;
            };
            let task_id = data.get("task_id").and_then(|v| v.as_u64()).unwrap_or(0);
            let event = data.get("event").and_then(|v| v.as_str()).unwrap_or("");
            let inner = data.get("data").cloned().unwrap_or_default();

            if inner.get("submit").and_then(|v| v.as_str()).is_some() {
                // 搜索请求（来自 PluginData）：发起后台搜索
                let query = inner["submit"].as_str().unwrap_or("").to_string();
                with_ui(|s| {
                    s.query = query.clone();
                    s.searching = true;
                    s.search_err = None;
                    s.songs.clear();
                    s.selected = 0;
                });
                return if task::spawn(SEARCH_TASK, "search", &serde_json::json!({"query": query})) {
                    1
                } else {
                    with_ui(|s| {
                        s.searching = false;
                        s.search_err = Some("宿主拒绝任务".into());
                    });
                    1
                };
            }

            if task_id == SEARCH_TASK {
                // 搜索任务回执
                match event {
                    "done" => {
                        // 宿主 emit("done", 返回值)：inner 即 {"songs":[...]}
                        let songs = inner
                            .get("songs")
                            .and_then(|v| v.as_array())
                            .cloned()
                            .unwrap_or_default();
                        with_ui(|s| {
                            s.searching = false;
                            s.songs = songs;
                            s.selected = 0;
                        });
                        return 1;
                    }
                    "failed" => {
                        let err = inner.as_str().unwrap_or("未知错误").to_string();
                        with_ui(|s| {
                            s.searching = false;
                            s.search_err = Some(err);
                        });
                        return 1;
                    }
                    _ => return 0,
                }
            }

            // 下载任务回执（task_id >= 100）
            if task_id >= 100 {
                with_ui(|s| {
                    let Some(dl) = s.downloads.iter_mut().find(|d| d.task_id == task_id) else {
                        return;
                    };
                    match event {
                        "progress" => {
                            dl.received = inner.get("received").and_then(|v| v.as_u64()).unwrap_or(dl.received);
                            if let Some(t) = inner.get("total").and_then(|v| v.as_u64()) {
                                dl.total = Some(t);
                            }
                            if inner.get("phase").and_then(|v| v.as_str()) == Some("metadata") {
                                // 门禁阶段：进度条已满，等 done/failed
                            }
                        }
                        "done" => dl.state = DlState::Done,
                        "failed" => {
                            dl.state =
                                DlState::Failed(inner.as_str().unwrap_or("未知").to_string());
                        }
                        _ => {}
                    }
                });
                return 1;
            }
            0
        }
        KIND_UP => {
            with_ui(|s| s.selected = s.selected.saturating_sub(1));
            1
        }
        KIND_DOWN => {
            with_ui(|s| {
                if s.selected + 1 < s.songs.len() {
                    s.selected += 1;
                }
            });
            1
        }
        KIND_CHAR => {
            match char_of(ev) {
                Some('j') => return ui_update(rmus_plugin_sdk::event::encode(rmus_plugin_sdk::event::KIND_DOWN, 0)),
                Some('k') => return ui_update(rmus_plugin_sdk::event::encode(rmus_plugin_sdk::event::KIND_UP, 0)),
                Some('d') => {
                    // 下载选中项：worker 实例跑 download task
                    let song = with_ui(|s| {
                        match s.songs.get(s.selected) {
                            Some(song) => song.clone(),
                            None => serde_json::Value::Null,
                        }
                    });
                    if song.is_null() {
                        return 0;
                    }
                    let task_id = with_ui(|s| {
                        let id = s.next_task;
                        s.next_task += 1;
                        s.downloads.push(Download {
                            source: song["source"].as_str().unwrap_or("?").to_string(),
                            song_id: song["song_id"].as_str().unwrap_or("?").to_string(),
                            task_id: id,
                            received: 0,
                            total: song["size_bytes"].as_u64(),
                            state: DlState::Running,
                        });
                        id
                    });
                    return if task::spawn(
                        task_id,
                        "download",
                        &serde_json::json!({"song": song}),
                    ) {
                        1
                    } else {
                        with_ui(|s| {
                            if let Some(dl) = s.downloads.iter_mut().find(|d| d.task_id == task_id) {
                                dl.state = DlState::Failed("宿主拒绝任务".into());
                            }
                        });
                        1
                    };
                }
                Some('x') => {
                    // 取消最近一个进行中的下载
                    let running = with_ui(|s| {
                        s.downloads
                            .iter()
                            .rev()
                            .find(|d| d.state == DlState::Running)
                            .map(|d| d.task_id)
                    });
                    if let Some(id) = running {
                        task::cancel(id);
                        return 1;
                    }
                    return 0;
                }
                _ => return 0,
            }
        }
        _ => 0,
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn update(ev: i32) -> i32 {
    ui_update(ev)
}

#[unsafe(no_mangle)]
pub extern "C" fn render(rows: i32, cols: i32) {
    let width = cols.max(20) as u16;
    let c = ui::columns(width);
    let mut state = UI.lock().unwrap();
    let Some(s) = state.as_mut() else {
        // 待机：原版空态文案（画布只能画文本行，居中是宿主渲染的事）
        ui::print_line(&[
            ui::span(style::STRONG, "按 / 搜索在线音乐"),
            ui::span(style::FAINT, "  结果可直接下载，完成后自动进入音乐库"),
        ]);
        return;
    };

    // 第一行：关键词 + 结果数（搜索中/失败时给状态）
    if let Some(err) = &s.search_err {
        ui::print_line(&[
            ui::span(style::FAINT, "搜索 "),
            ui::span(style::STRONG, s.query.clone()),
            ui::span(style::DANGER, format!("  ·  失败：{err}")),
        ]);
    } else if s.searching {
        ui::print_line(&[
            ui::span(style::FAINT, "搜索 "),
            ui::span(style::STRONG, s.query.clone()),
            ui::span(style::ACCENT, "  ·  搜索中…"),
        ]);
    } else if s.songs.is_empty() && !s.query.is_empty() {
        ui::print_line(&[
            ui::span(style::FAINT, "搜索 "),
            ui::span(style::STRONG, s.query.clone()),
            ui::span(style::FAINT, "  ·  没有结果，换个关键词试试"),
        ]);
        return;
    } else {
        ui::print_line(&[
            ui::span(style::FAINT, "搜索 "),
            ui::span(style::STRONG, s.query.clone()),
            ui::span(style::FAINT, format!("  ·  {} 首", s.songs.len())),
        ]);
    }

    // 第二行：列标题（暗灰），第三行分隔线（原版在线表结构）
    ui::print_line(&[
        ui::span(style::FAINT, ui::pad_right("#", c.index_x as usize + c.index_w as usize)),
        ui::span(style::FAINT, ui::pad_right("曲目", c.title_w as usize)),
        if c.artist_w > 0 {
            ui::span(style::FAINT, format!("  {}", ui::pad_right("艺术家", c.artist_w as usize)))
        } else {
            ui::span(style::FAINT, "")
        },
        if c.album_w > 0 {
            // 右侧给音质+来源让位（各 ~10/8 列 + 间隔），与原版一致
            ui::span(style::FAINT, format!("  {}", ui::pad_right("专辑", (c.album_w as usize).saturating_sub(21))))
        } else {
            ui::span(style::FAINT, "")
        },
        if c.album_w > 0 {
            ui::span(style::FAINT, ui::pad_left("音质", 10))
        } else {
            ui::span(style::FAINT, "")
        },
        if c.album_w > 0 {
            ui::span(style::FAINT, format!("  {}", ui::pad_left("来源", 8)))
        } else {
            ui::span(style::FAINT, "")
        },
        if c.dur_w > 0 {
            ui::span(style::FAINT, format!("  {}", ui::pad_left("时长", c.dur_w as usize)))
        } else {
            ui::span(style::FAINT, "")
        },
    ]);
    ui::print_line(&[ui::span(style::RULE, "─".repeat(width as usize))]);

    if s.searching {
        return;
    }

    // 结果行窗口化：画布高度减去头部 3 行（搜索行/列标题/分隔线），
    // 选中行始终滚入窗口；选中画布行号声明给宿主（铺选中底色）
    let list_h = (rows.max(4) as usize).saturating_sub(3);
    let first = ui::window(&mut s.scroll, s.selected, s.songs.len(), list_h);
    ui::set_selected(3 + s.selected - first);

    // 结果行（列布局；时长列被下载状态覆盖：… / 47% / ✓ / ✗；
    // 下载中的行按已收比例画绿色进度条）
    for (i, song) in s.songs[first..].iter().take(list_h).enumerate() {
        let i = first + i;
        let title = song["title"].as_str().unwrap_or("?");
        let artist = song["artist"].as_str().unwrap_or("");
        let album = song["album"].as_str().unwrap_or("");
        let source = song["source"].as_str().unwrap_or("?");
        let song_id = song["song_id"].as_str().unwrap_or("");
        let selected = i == s.selected;
        let title_style = if selected { style::STRONG } else { style::BASE };

        // 行首空位：index 列右对齐（行背景由宿主画，空格自然带底色）
        // 流式拼行：每段必须 pad 到列定宽，否则后续列错位
        let mut spans = vec![
            ui::span(style::FAINT, ui::pad_left(&(i + 1).to_string(), c.index_x as usize + c.index_w as usize)),
            ui::span(title_style, format!("  {}", ui::pad_right(title, c.title_w as usize))),
        ];
        if c.artist_w > 0 {
            spans.push(ui::span(style::FAINT, format!("  {}", ui::pad_right(artist, c.artist_w as usize))));
        }
        if c.album_w > 0 {
            spans.push(ui::span(style::FAINT, format!("  {}", ui::pad_right(album, (c.album_w as usize).saturating_sub(21)))));
        }
        // 音质（flac 绿色）+ 来源（冷色弱化），挤在时长左边
        let format = song["format"].as_str();
        let size = song["size_bytes"].as_u64();
        let quality = match (format, size) {
            (Some(f), Some(bytes)) => format!("{f} {}M", (bytes + 512 * 1024) / (1024 * 1024)),
            (Some(f), None) => f.to_string(),
            (None, _) => "—".to_string(),
        };
        if c.album_w > 0 {
            let q_style = if format == Some("flac") { style::OK } else { style::FAINT };
            spans.push(ui::span(q_style, ui::pad_left(&quality, 10)));
            spans.push(ui::span(style::COOL, format!("  {}", ui::pad_left(source, 8))));
        }

        // 时长列：下载中换百分比，完成 ✓，失败 ✗（原版语义）
        let download = s.downloads.iter().find(|d| d.source == source && d.song_id == song_id);
        let (right, rstyle) = match download {
            Some(d) => match &d.state {
                DlState::Running if d.received == 0 => ("…".to_string(), style::ACCENT),
                DlState::Running => {
                    let pct = match d.total {
                        Some(total) if total > 0 => format!("{}%", d.received * 100 / total),
                        _ => {
                            let (v, unit) = if d.received >= 1 << 20 {
                                (d.received as f64 / (1 << 20) as f64, "MiB")
                            } else {
                                (d.received as f64 / (1 << 10) as f64, "KiB")
                            };
                            format!("{v:.1}{unit}")
                        }
                    };
                    (pct, style::ACCENT)
                }
                DlState::Done => ("✓".to_string(), style::OK_BOLD),
                DlState::Failed(_) => ("✗".to_string(), style::DANGER_BOLD),
            },
            None => {
                let ms = song["duration_ms"].as_u64().unwrap_or(0);
                let text = if ms > 0 {
                    format!("{:02}:{:02}", ms / 60_000, (ms % 60_000) / 1000)
                } else {
                    "--:--".to_string()
                };
                (text, style::FAINT)
            }
        };
        if c.dur_w > 0 {
            spans.push(ui::span(rstyle, format!("  {}", ui::pad_left(&right, c.dur_w as usize))));
        }
        // 下载中且总量已知：整行按已收比例铺绿色进度条（行内文本不变，
        // 只是前 N 宽的底色变绿）
        let spans = if let Some(d) = download {
            match (&d.state, d.total) {
                (DlState::Running, Some(total)) if total > 0 => {
                    ui::paint_progress(&spans, (width as usize) * d.received as usize / total as usize)
                }
                _ => spans,
            }
        } else {
            spans
        };
        ui::print_line(&spans);
    }
}
