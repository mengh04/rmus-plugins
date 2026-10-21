//! rmus-podcast：播客订阅插件（wasm，TUI「播客」tab 画布）。
//!
//! 架构分工（方案 docs/podcast-plugin-plan.md，宿主只当解码播放器）：
//! - 搜索/订阅/退订/单集浏览/状态全部插件自治（data_dir 下 JSON 存档）
//! - 搜索（iTunes 目录）与 feed 抓取解析都在 worker（task::spawn），
//!   done/failed 经 KIND_DATA 回注重绘，UI 线程零阻塞
//! - 下载与连播接管在 worker 里（纯 status 采样，不依赖推送）
//!
//! 画布三级：搜索（s 输关键词 → Enter）→ 订阅列表（a 贴 feed URL /
//! x 退订 / Enter 进单集）→ 单集列表（d 下载 / Enter 播放 / p 标已播 /
//! h 返回）。输入态支持 `KIND_BACKSPACE` 删字符，退成空串即取消；
//! 搜索命中与手贴 RSS 走同一条 fetch_feed 订阅链路。

mod download;
pub mod feed;
pub mod search;
pub mod store;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use rmus_plugin_sdk::{daemon, data_dir, debug_log, declare_plugin_manifest, http, task, ui};
use store::{Episode, Store};
use ui::style;

declare_plugin_manifest! {
    name: "rmus-podcast", version: "0.3.3", ui: true, tab: "播客",
    methods: [
        ("fetch_feed", "task"),
        ("search_podcasts", "task"),
        ("download_episode", "task"),
        ("podcast_worker", "task")
    ],
    handle: handle,
}

/// 任务号分区：feed 刷新 100+，搜索 50000+，下载 1000+（回执按 id 查表
/// 分发，号段只用来保证一次会话内不撞号）。
const FETCH_TASK_BASE: u64 = 100;
const DOWNLOAD_TASK_BASE: u64 = 1000;
const SEARCH_TASK_BASE: u64 = 50_000;
const WORKER_TASK: u64 = 7;

/// 搜索结果每页条数（iTunes 一次拉满，翻页在本地切）
const SEARCH_PAGE: usize = 25;

/// 下载中的行（内存态；完成后落到 Episode.downloaded）。
struct Download {
    task_id: u64,
    guid: String,
    received: u64,
    total: Option<u64>,
    failed: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum View {
    Subs,
    Eps,
    Search,
}

/// 输入态用途：贴 feed 地址 / 输搜索关键词。
#[derive(Debug, Clone, Copy, PartialEq)]
enum InputKind {
    FeedUrl,
    Query,
}

/// 画布输入态（a / s 开启；Enter 提交，退格删字符，空串退格取消）。
struct Input {
    kind: InputKind,
    text: String,
}

struct UiState {
    store: Store,
    data_dir: PathBuf,
    view: View,
    /// 三级列表各自的光标/窗口（切换后各自保留）。导航与窗口计算走
    /// SDK 的 ListState，与宿主其他列表同一套语义。
    subs: ui::ListState,
    eps: ui::ListState,
    /// 最近一次 render 的画布高度（半页导航用）
    view_rows: usize,
    /// a 键的 feed URL 输入态 / s 键的搜索词输入态（None = 非输入态）
    input: Option<Input>,
    /// 搜索结果（View::Search 的内容）
    hits: Vec<search::Hit>,
    /// 当前结果页（0 起；每页 SEARCH_PAGE 条）
    page: usize,
    /// 当前结果页的光标/窗口
    hit_list: ui::ListState,
    /// 在途搜索任务号（None = 没有正在跑的搜索）
    search_task: Option<u64>,
    /// 结果页标题里的关键词
    query: String,
    /// 底部状态行（订阅中/退订提示/错误信息），一次性消费
    status: Option<String>,
    /// 在途 feed 刷新：feed_url -> task_id（去重 + 回执分发）
    fetching: Vec<(String, u64)>,
    /// 在途/已完成的下载（内存进度；done 后落到 Episode.downloaded）
    downloads: Vec<Download>,
    /// 下载完成后自动播放的集（Enter 未下载时的预约）
    pending_play: Option<(String, String)>,
    next_task: u64,
    /// 下载任务号独立计数（1000+；按 id 查表分发）
    next_download_task: u64,
    /// 搜索任务号独立计数（50000+）
    next_search_task: u64,
    /// 常驻 worker（进度采样 + 连播）只起一次
    worker_spawned: bool,
}

impl UiState {
    fn ensure_worker(&mut self) {
        if self.worker_spawned {
            return;
        }
        self.worker_spawned = true;
        // 失败不重试：worker 丢了只影响进度记忆/连播，浏览不受影响
        // （TODO v3.5：DATA 事件里探测 worker 存活并补 spawn）
        let _ = task::spawn(WORKER_TASK, "podcast_worker", &serde_json::json!({}));
    }
}

impl UiState {
    fn new() -> Self {
        let dir = PathBuf::from(data_dir());
        let store = store::load(&dir);
        Self {
            store,
            data_dir: dir,
            view: View::Subs,
            subs: ui::ListState::new(),
            eps: ui::ListState::new(),
            view_rows: 24,
            input: None,
            hits: Vec::new(),
            page: 0,
            hit_list: ui::ListState::new(),
            search_task: None,
            query: String::new(),
            status: None,
            fetching: Vec::new(),
            downloads: Vec::new(),
            pending_play: None,
            next_task: FETCH_TASK_BASE,
            next_download_task: DOWNLOAD_TASK_BASE,
            next_search_task: SEARCH_TASK_BASE,
            worker_spawned: false,
        }
    }

    fn save(&self) {
        if let Err(e) = store::save(&self.data_dir, &self.store) {
            // wasm32 无 stderr：错误走 rmus.log（daemon stderr 可见）
            debug_log(&format!("[podcast] 订阅存档失败: {e:#}"));
        }
    }

    fn spawn_fetch(&mut self, feed_url: &str) -> bool {
        if self.fetching.iter().any(|(u, _)| u == feed_url) {
            return true; // 在途不重复发起，视为成功
        }
        let task_id = self.next_task;
        self.next_task += 1;
        if task::spawn(
            task_id,
            "fetch_feed",
            &serde_json::json!({ "url": feed_url }),
        ) {
            self.fetching.push((feed_url.to_string(), task_id));
            true
        } else {
            self.status = Some("宿主拒绝刷新任务".into());
            false
        }
    }

    /// 发起 iTunes 搜索；结果经 KIND_DATA 回注（号段 50000+）。
    fn spawn_search(&mut self, query: &str) -> bool {
        let task_id = self.next_search_task;
        self.next_search_task += 1;
        let spec = serde_json::json!({ "query": query });
        if task::spawn(task_id, "search_podcasts", &spec) {
            self.search_task = Some(task_id);
            self.hits.clear();
            self.page = 0;
            self.hit_list = ui::ListState::new();
            self.query = query.to_string();
            self.view = View::Search;
            true
        } else {
            self.status = Some("宿主拒绝搜索任务".into());
            false
        }
    }

    /// 结果总页数（至少 1，便于显示"第 1/1 页"）。
    fn page_count(&self) -> usize {
        self.hits.len().div_ceil(SEARCH_PAGE).max(1)
    }

    /// 当前页条数。
    fn page_len(&self) -> usize {
        let start = self.page * SEARCH_PAGE;
        self.hits.len().saturating_sub(start).min(SEARCH_PAGE)
    }
}

static UI: Mutex<Option<UiState>> = Mutex::new(None);

fn with_ui<R>(f: impl FnOnce(&mut UiState) -> R) -> R {
    let mut guard = UI.lock().unwrap();
    let state = guard.get_or_insert_with(UiState::new);
    f(state)
}

fn handle(method: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    match method {
        "fetch_feed" => fetch_feed(params).map_err(|e| format!("{e:#}")),
        // 目录搜索（iTunes Search API）：只回候选，订阅仍走 fetch_feed
        "search_podcasts" => {
            let query = params
                .get("query")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if query.is_empty() {
                return Err("缺少搜索关键词".into());
            }
            let hits = search::search(query).map_err(|e| format!("{e:#}"))?;
            Ok(serde_json::json!({ "hits": hits }))
        }
        "download_episode" => download::download_episode(
            params.get("url").and_then(|v| v.as_str()).unwrap_or(""),
            params.get("title").and_then(|v| v.as_str()).unwrap_or("episode"),
            &download::episode_dir(
                params.get("data_dir").and_then(|v| v.as_str()).unwrap_or(""),
                params.get("slug").and_then(|v| v.as_str()).unwrap_or("misc"),
            ),
            &mut download::Progress {
                on_chunk: &mut |received, total| {
                    // 150ms 节流（rmus-dl 同款）：64KB 一条的 progress
                    // 会以千计灌爆事件通道
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static LAST: AtomicU64 = AtomicU64::new(0);
                    let now = rmus_plugin_sdk::now_ms();
                    let finished = total.is_some_and(|t| received >= t);
                    if !finished && now.saturating_sub(LAST.load(Ordering::Relaxed)) < 150 {
                        return;
                    }
                    LAST.store(now, Ordering::Relaxed);
                    rmus_plugin_sdk::notify(&serde_json::json!({
                        "received": received, "total": total,
                    }));
                },
            },
        )
        .map(|file| serde_json::json!({ "file": file }))
        .map_err(|e| format!("{e:#}")),
        // 常驻循环：进度采样落盘（经事件回 UI）+ 播完推断连播。
        // daemon 停止态零推送也不影响——这里是自己的 1s 轮询。
        "podcast_worker" => podcast_worker().map_err(|e| format!("{e:#}")),
        other => Err(format!("未知方法 “{other}”")),
    }
}

/// worker：抓 feed XML 并解析成结构化 JSON（合并/存档在 UI 侧做）。
fn fetch_feed(params: &serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .context("缺少 url 参数")?
        .to_string();
    let body = http::Response::send(&http::Request::get(url.clone()))?
        .error_for_status()?
        .read_to_end()?;
    let text = String::from_utf8_lossy(&body);
    let parsed = feed::parse(&text)?;
    Ok(serde_json::json!({
        "feed_url": url,
        "title": parsed.title,
        "author": parsed.author,
        "episodes": parsed.episodes,
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn update(ev: i32) -> i32 {
    use rmus_plugin_sdk::event::{
        char_of, encode, kind, KIND_BACKSPACE, KIND_BOTTOM, KIND_CHAR, KIND_DATA, KIND_DOWN,
        KIND_ENTER, KIND_HALF_DOWN, KIND_HALF_UP, KIND_TOP, KIND_UP,
    };

    match kind(ev) {
        KIND_DATA => on_data(),
        // 上下 / 首尾 / 半页：宿主只发语义事件，光标与窗口归 ListState
        KIND_UP | KIND_DOWN | KIND_TOP | KIND_BOTTOM | KIND_HALF_UP | KIND_HALF_DOWN => {
            with_ui(|s| {
                // 输入态下导航键不挪背后列表（避免打字时选中项乱跑）
                if s.input.is_some() {
                    return 0;
                }
                let page = (s.view_rows / 2).max(1);
                let len = match s.view {
                    View::Subs => s.store.podcasts.len(),
                    View::Eps => current_episodes(s).len(),
                    View::Search => s.page_len(),
                };
                let list = match s.view {
                    View::Subs => &mut s.subs,
                    View::Eps => &mut s.eps,
                    View::Search => &mut s.hit_list,
                };
                list.handle(ev, len, page) as i32
            })
        }
        KIND_BACKSPACE => with_ui(|s| {
            let Some(input) = s.input.as_mut() else {
                return 0;
            };
            // 退成空串再按一次退格 = 取消输入（Esc 在直通模式下归宿主）
            if input.text.pop().is_none() {
                s.input = None;
                s.status = Some("已取消输入".into());
            }
            1
        }),
        KIND_ENTER => {
            self::ensure_worker();

            // 输入态：按用途提交（贴 feed / 搜关键词）
            let submitted = with_ui(|s| s.input.take());
            if let Some(input) = submitted {
                let text = input.text.trim().to_string();
                if text.is_empty() {
                    with_ui(|s| s.status = Some("已取消输入".into()));
                    return 1;
                }
                return with_ui(|s| match input.kind {
                    InputKind::FeedUrl => {
                        if !text.starts_with("http://") && !text.starts_with("https://") {
                            s.status = Some("feed 地址要以 http(s):// 开头".into());
                            s.input = Some(Input {
                                kind: InputKind::FeedUrl,
                                text,
                            });
                            return 1;
                        }
                        if s.store.podcasts.iter().any(|p| p.feed_url == text) {
                            s.status = Some("这个播客已在订阅里".into());
                            return 1;
                        }
                        if s.spawn_fetch(&text) {
                            s.status = Some(format!("订阅中：{text}"));
                        }
                        1
                    }
                    InputKind::Query => {
                        s.spawn_search(&text);
                        1
                    }
                });
            }
            // 非输入态：订阅列表进单集（顺带异步刷新）；搜索结果订阅；
            // 单集列表播放
            with_ui(|s| match s.view {
                View::Subs => {
                    let feed_url = s
                        .store
                        .podcasts
                        .get(s.subs.selected)
                        .map(|p| p.feed_url.clone());
                    if let Some(feed_url) = feed_url {
                        s.view = View::Eps;
                        s.eps = ui::ListState::new();
                        s.spawn_fetch(&feed_url);
                        return 1;
                    }
                    0
                }
                View::Search => subscribe_selected(s),
                View::Eps => play_selected(s),
            })
        }
        KIND_CHAR => {
            let ch = char_of(ev).unwrap_or('\0');
            // 输入态：收字符（中文关键词也要能输入，不用 ascii_graphic 过滤）
            if with_ui(|s| s.input.is_some()) {
                if !ch.is_control() {
                    with_ui(|s| {
                        if let Some(input) = s.input.as_mut() {
                            input.text.push(ch);
                        }
                    });
                    return 1;
                }
                return 0;
            }
            match ch {
                'j' => update(encode(KIND_DOWN, 0)),
                'k' => update(encode(KIND_UP, 0)),
                // Shift+H / Shift+L：搜索结果翻页（iTunes 不支持 offset，
                // 结果一次拉满后在本地切页）
                'H' => with_ui(|s| {
                    if s.view != View::Search {
                        return 0;
                    }
                    if s.page == 0 {
                        s.status = Some("已经是第一页".into());
                        return 1;
                    }
                    s.page -= 1;
                    s.hit_list.selected = 0;
                    s.hit_list.scroll = 0;
                    s.status = None;
                    1
                }),
                'L' => with_ui(|s| {
                    if s.view != View::Search {
                        return 0;
                    }
                    if s.page + 1 >= s.page_count() {
                        s.status = Some("已经是最后一页".into());
                        return 1;
                    }
                    s.page += 1;
                    s.hit_list.selected = 0;
                    s.hit_list.scroll = 0;
                    s.status = None;
                    1
                }),
                's' | '/' => {
                    with_ui(|s| {
                        s.input = Some(Input {
                            kind: InputKind::Query,
                            text: String::new(),
                        });
                        s.ensure_worker();
                    });
                    1
                }
                'a' => {
                    with_ui(|s| {
                        s.input = Some(Input {
                            kind: InputKind::FeedUrl,
                            text: String::new(),
                        });
                        s.ensure_worker();
                    });
                    1
                }
                'h' => {
                    with_ui(|s| {
                        if s.view != View::Subs {
                            // 离开搜索页：在途搜索取消，结果迟到也不改状态
                            if let Some(id) = s.search_task.take() {
                                let _ = task::cancel(id);
                            }
                            s.view = View::Subs;
                        }
                    });
                    1
                }
                'd' => {
                    // 下载选中集：worker 跑 download_episode
                    with_ui(|s| {
                        if s.view != View::Eps {
                            return 0;
                        }
                        let (_feed_url, _slug, spec) = {
                            let pi = s.subs.selected.min(s.store.podcasts.len().saturating_sub(1));
                            let pod = &s.store.podcasts[pi];
                            let Some(ep) = pod.episodes.get(s.eps.selected) else {
                                return 0;
                            };
                            if ep.downloaded.is_some() {
                                s.status = Some("已下载过".into());
                                return 1;
                            }
                            (
                                pod.feed_url.clone(),
                                Store::slug_of(pod),
                                serde_json::json!({
                                    "url": ep.enclosure_url,
                                    "title": ep.title,
                                    "data_dir": s.data_dir,
                                    "slug": Store::slug_of(pod),
                                }),
                            )
                        };
                        let sel = s.eps.selected;
                        let guid = current_episodes(s)
                            .get(sel)
                            .map(|e| e.guid.clone())
                            .unwrap_or_default();
                        let task_id = s.next_download_task;
                        s.next_download_task += 1;
                        let ok = task::spawn(task_id, "download_episode", &spec);
                        if ok {
                            // 同一集重试：清掉上次失败/残留的条目，避免旧状态压着新进度
                            s.downloads.retain(|d| d.guid != guid);
                            s.downloads.push(Download {
                                task_id,
                                guid,
                                received: 0,
                                total: None,
                                failed: None,
                            });
                        } else {
                            s.status = Some("宿主拒绝下载任务".into());
                        }
                        1
                    })
                }
                'p' => {
                    with_ui(|s| {
                        if s.view != View::Eps {
                            return 0;
                        }
                        let sel = s.eps.selected;
                        let Some(ep) = current_episodes(s).get_mut(sel) else {
                            return 0;
                        };
                        ep.played = true;
                        ep.position_ms = 0;
                        s.save();
                        s.status = Some("已标记已播".into());
                        1
                    })
                }
                'x' => {
                    with_ui(|s| {
                        if s.view == View::Subs
                            && s.subs.selected < s.store.podcasts.len()
                        {
                            let p = s.store.podcasts.remove(s.subs.selected);
                            s.subs.selected = s.subs.selected.min(s.store.podcasts.len().saturating_sub(1));
                            s.save();
                            s.status = Some(format!("已退订：{}", p.title));
                            return 1;
                        }
                        0
                    })
                }
                _ => 0,
            }
        }
        _ => 0,
    }
}

/// Enter 播放选中集：已下载直接试听态播放；未下载先发起下载并预约
/// 播放（done 事件里自动开播）。
fn play_selected(s: &mut UiState) -> i32 {
    let pi = s.subs.selected.min(s.store.podcasts.len().saturating_sub(1));
    let (feed_url, slug, ep_info) = {
        let pod = &s.store.podcasts[pi];
        let Some(ep) = pod.episodes.get(s.eps.selected) else {
            return 0;
        };
        (
            pod.feed_url.clone(),
            Store::slug_of(pod),
            (ep.guid.clone(), ep.downloaded.clone(), ep.title.clone(), ep.enclosure_url.clone()),
        )
    };
    let (guid, downloaded, title, enclosure_url) = ep_info;

    if downloaded.is_some() && let Some(path) = s.play_path_of(&feed_url, &guid) {
        let _ = daemon::peek(vec![path]);
        s.ensure_worker();
        return 1;
    }
    // 在途去重：同集的下载已在跑（未失败）就不重复发起
    if s.downloads.iter().any(|d| {
        d.guid == guid && d.failed.is_none() && s.store.podcasts.iter().all(|p| {
            p.episodes.iter().find(|e| e.guid == guid).and_then(|e| e.downloaded.as_ref()).is_none()
        })
    }) {
        s.pending_play = Some((guid, feed_url));
        s.status = Some("下载中，完成后自动播放".into());
        return 1;
    }

    // 未下载：发起下载 + 预约自动播放
    let task_id = s.next_download_task;
    s.next_download_task += 1;
    let ok = task::spawn(
        task_id,
        "download_episode",
        &serde_json::json!({ "url": enclosure_url, "title": title, "data_dir": s.data_dir, "slug": slug }),
    );
    if ok {
        s.pending_play = Some((guid.clone(), feed_url.clone()));
        s.downloads.retain(|d| d.guid != guid);
        s.downloads.push(Download {
            task_id,
            guid,
            received: 0,
            total: None,
            failed: None,
        });
        s.status = Some("下载中，完成后自动播放".into());
    } else {
        s.status = Some("宿主拒绝下载任务".into());
    }
    1
}

/// 搜索结果页 Enter：订阅选中命中（走 fetch_feed，与手贴 RSS 同一条链路）。
fn subscribe_selected(s: &mut UiState) -> i32 {
    let Some(hit) = s.hits.get(s.page * SEARCH_PAGE + s.hit_list.selected).cloned() else {
        return 0;
    };
    if s.store.podcasts.iter().any(|p| p.feed_url == hit.feed_url) {
        s.status = Some(format!("已在订阅里：{}", hit.title));
        return 1;
    }
    if s.spawn_fetch(&hit.feed_url) {
        s.status = Some(format!("订阅中：{}", hit.title));
    }
    1
}

/// 单集的绝对落盘路径（已下载）。
impl UiState {
    fn play_path_of(&self, feed_url: &str, guid: &str) -> Option<String> {
        let pod = self.store.podcasts.iter().find(|p| p.feed_url == feed_url)?;
        let ep = pod.episodes.iter().find(|e| e.guid == guid)?;
        let path = self.store.episode_path(&self.data_dir, pod, ep)?;
        Some(path.display().to_string())
    }
}

fn ensure_worker() {
    with_ui(|s| s.ensure_worker());
}

/// 当前单集视图对应的播客单集（可变借用侧写）。
fn current_episodes(s: &mut UiState) -> &mut Vec<Episode> {
    let idx = s.subs.selected.min(s.store.podcasts.len().saturating_sub(1));
    &mut s.store.podcasts[idx].episodes
}

/// 后台事件：feed 刷新回执（done/failed）与 daemon 事件（v1 忽略，
/// 连播接管接入后在此处理播完标记）。
fn on_data() -> i32 {
    use rmus_plugin_sdk::event::KIND_DATA;
    let _ = KIND_DATA;
    let Some(data) = task::last_event() else {
        return 0;
    };
    // daemon 事件桥（kind:"daemon"）：切歌/停止 → 画布重绘。
    // 连播决策在常驻 worker 里（纯 status 采样），不依赖这些事件。
    if data.get("kind").and_then(|v| v.as_str()) == Some("daemon") {
        return 1;
    }
    let task_id = data.get("task_id").and_then(|v| v.as_u64()).unwrap_or(0);
    let event = data.get("event").and_then(|v| v.as_str()).unwrap_or("");
    let inner = data.get("data").cloned().unwrap_or_default();

    // 常驻 worker 的批量落盘（positions）：应用到订阅库并写档
    if inner.get("worker").and_then(|v| v.as_str()) == Some("positions") {
        return with_ui(|s| {
            let updates = inner
                .get("updates")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            for u in &updates {
                let (guid, position) = (
                    u.get("guid").and_then(|v| v.as_str()).unwrap_or(""),
                    u.get("position_ms").and_then(|v| v.as_u64()).unwrap_or(0),
                );
                for pod in &mut s.store.podcasts {
                    if let Some(ep) = pod.episodes.iter_mut().find(|e| e.guid == guid) {
                        ep.position_ms = position;
                    }
                }
            }
            s.save();
            1
        });
    }
    if inner.get("worker").is_some() {
        return 1; // advance 等其他 worker 通知：重绘即可
    }

    // 搜索回执（命中在途搜索任务号）
    let is_search = with_ui(|s| s.search_task == Some(task_id));
    if is_search {
        return with_ui(|s| {
            match event {
                "done" => {
                    let hits: Vec<search::Hit> = inner
                        .get("hits")
                        .cloned()
                        .and_then(|v| serde_json::from_value(v).ok())
                        .unwrap_or_default();
                    let n = hits.len();
                    s.search_task = None;
                    s.hits = hits;
                    s.hit_list.selected = 0;
                    s.hit_list.scroll = 0;
                    s.status = (n == 0).then(|| "没有找到播客".to_string());
                }
                "failed" => {
                    let err = inner.as_str().unwrap_or("未知错误").to_string();
                    s.search_task = None;
                    s.status = Some(format!("搜索失败：{err}"));
                }
                // 搜索任务不发 progress；其它事件忽略
                _ => return 0,
            }
            1
        });
    }

    // 下载任务回执（按任务号查在途表）
    let is_download = with_ui(|s| s.downloads.iter().any(|d| d.task_id == task_id));
    if is_download {
        return with_ui(|s| {
            let Some(guid) = s
                .downloads
                .iter()
                .find(|d| d.task_id == task_id)
                .map(|d| d.guid.clone())
            else {
                return 0;
            };
            match event {
                "progress" => {
                    if let Some(dl) = s.downloads.iter_mut().find(|d| d.task_id == task_id) {
                        dl.received = inner
                            .get("received")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(dl.received);
                        if let Some(t) = inner.get("total").and_then(|v| v.as_u64()) {
                            dl.total = Some(t);
                        }
                    }
                }
                "done" => {
                    let file = inner.get("file").and_then(|v| v.as_str()).map(String::from);
                    if let Some(file) = file {
                        for pod in &mut s.store.podcasts {
                            if let Some(ep) =
                                pod.episodes.iter_mut().find(|e| e.guid == guid)
                            {
                                ep.downloaded = Some(file.clone());
                            }
                        }
                        s.save();
                    }
                    // 结束态必须摘掉在途条目：否则列表永远停在最后一条进度
                    // （收到 100% 后 done 已到，行里却还画着进度条）
                    s.downloads.retain(|d| d.task_id != task_id);
                    // 预约播放：下载完成自动开播
                    let auto = s.pending_play.take().filter(|(g, _)| g == &guid);
                    if let Some((_, feed_url)) = auto {
                        let path = s.play_path_of(&feed_url, &guid);
                        if let Some(path) = path {
                            let _ = daemon::peek(vec![path]);
                        }
                    }
                }
                "failed" => {
                    if let Some(dl) = s.downloads.iter_mut().find(|d| d.task_id == task_id) {
                        dl.failed = Some(inner.as_str().unwrap_or("未知错误").to_string());
                    }
                }
                _ => {}
            }
            1
        });
    }

    let feed_url = with_ui(|s| {
        s.fetching
            .iter()
            .find(|(_, id)| *id == task_id)
            .map(|(u, _)| u.clone())
    });
    let Some(feed_url) = feed_url else {
        return 0;
    };

    match event {
        "done" => {
            let title = inner["title"].as_str().unwrap_or("未命名播客").to_string();
            let author = inner["author"].as_str().unwrap_or("").to_string();
            let episodes: Vec<Episode> = inner
                .get("episodes")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            with_ui(|s| {
                s.fetching.retain(|(_, id)| *id != task_id);
                // 新订阅：直接开到这个播客的单集页（已有的刷新则原地不动）
                let is_new = !s.store.podcasts.iter().any(|p| p.feed_url == feed_url);
                s.store.merge_episodes(&feed_url, title, author, episodes);
                s.save();
                s.status = Some(if is_new {
                    "订阅完成".into()
                } else {
                    "刷新完成".into()
                });
                if is_new
                    && let Some(idx) = s.store.podcasts.iter().position(|p| p.feed_url == feed_url)
                {
                    s.view = View::Eps;
                    s.subs.selected = idx;
                    s.eps = ui::ListState::new();
                }
            });
            1
        }
        "failed" => {
            let err = inner.as_str().unwrap_or("未知错误").to_string();
            with_ui(|s| {
                s.fetching.retain(|(_, id)| *id != task_id);
                // 新订阅失败：半途条目不该留（merge 没发生就没有）
                s.status = Some(format!("feed 获取失败：{err}"));
            });
            1
        }
        _ => 0,
    }
}

/// 毫秒 -> "m:ss"（超 1 小时折算成总分钟，保持列宽稳定）。
fn fmt_mmss(ms: u64) -> String {
    let secs = ms / 1000;
    let (m, s) = (secs / 60, secs % 60);
    format!("{m:02}:{s:02}")
}

#[unsafe(no_mangle)]
pub extern "C" fn render(rows: i32, cols: i32) {
    let width = cols.max(20) as usize;
    let mut state = UI.lock().unwrap();
    let Some(s) = state.as_mut() else {
        ui::print_line(&[ui::span(style::STRONG, "按 i 进入直通后：a 贴 RSS · s 搜索")]);
        return;
    };
    s.view_rows = rows.max(4) as usize;

    // 头行：标题/输入态/状态消息
    match (&s.view, &s.input) {
        (_, Some(input)) => {
            let (label, action) = match input.kind {
                InputKind::FeedUrl => ("订阅 feed：", "Enter 订阅"),
                InputKind::Query => ("搜索播客：", "Enter 搜索"),
            };
            ui::print_line(&[
                ui::span(style::FAINT, label),
                ui::span(style::ACCENT, input.text.clone()),
                ui::span(style::FAINT, format!("_  （{action} · 退格删字 · 空行取消）")),
            ]);
        }
        (View::Subs, _) => {
            let n = s.store.podcasts.len();
            let mut spans = vec![
                ui::span(style::STRONG, "播客订阅".to_string()),
                ui::span(style::FAINT, format!("  ·  {n} 个")),
            ];
            if let Some(msg) = s.status.take() {
                spans.push(ui::span(style::ACCENT, format!("  ·  {msg}")));
            }
            spans.push(ui::span(
                style::FAINT,
                "  ·  i 直通：s 搜索 · a 贴 RSS · Enter 单集 · x 退订",
            ));
            ui::print_line(&spans);
        }
        (View::Eps, _) => {
            let idx = s.subs.selected.min(s.store.podcasts.len().saturating_sub(1));
            let (title, n, fetching) = match s.store.podcasts.get(idx) {
                Some(p) => (
                    p.title.clone(),
                    p.episodes.len(),
                    s.fetching.iter().any(|(u, _)| u == &p.feed_url),
                ),
                None => (String::new(), 0, false),
            };
            let mut spans = vec![
                ui::span(style::STRONG, title),
                ui::span(style::FAINT, format!("  ·  {n} 集")),
            ];
            if fetching {
                spans.push(ui::span(style::ACCENT, "  ·  刷新中…"));
            }
            if let Some(msg) = s.status.take() {
                spans.push(ui::span(style::ACCENT, format!("  ·  {msg}")));
            }
            spans.push(ui::span(
                style::FAINT,
                "  ·  d 下载 · Enter 播放 · p 已播 · h 返回",
            ));
            ui::print_line(&spans);
        }
        (View::Search, _) => {
            let n = s.hits.len();
            let pages = s.page_count();
            let mut spans = vec![
                ui::span(style::STRONG, format!("搜索“{}”", s.query)),
                ui::span(style::FAINT, format!("  ·  {n} 个结果")),
            ];
            if pages > 1 {
                spans.push(ui::span(
                    style::FAINT,
                    format!("  ·  第 {}/{} 页", s.page + 1, pages),
                ));
            }
            if s.search_task.is_some() {
                spans.push(ui::span(style::ACCENT, "  ·  搜索中…"));
            }
            if let Some(msg) = s.status.take() {
                spans.push(ui::span(style::ACCENT, format!("  ·  {msg}")));
            }
            if pages > 1 {
                spans.push(ui::span(style::FAINT, "  ·  Enter 订阅 · H/L 翻页 · h 返回"));
            } else {
                spans.push(ui::span(style::FAINT, "  ·  Enter 订阅 · h 返回"));
            }
            ui::print_line(&spans);
        }
    }

    // 输入态不画列表
    if s.input.is_some() {
        return;
    }

    match s.view {
        View::Subs => render_subs(s, width, rows),
        View::Eps => render_eps(s, width, rows),
        View::Search => render_search(s, width, rows),
    }
}

/// 订阅列表：标题<flex> 作者<16> 单集<5> 未播<5>。
fn render_subs(s: &mut UiState, width: usize, rows: i32) {
    if s.store.podcasts.is_empty() {
        s.subs.prepare(0, 1, 3);
        ui::print_line(&[
            ui::span(style::FAINT, "还没有订阅。按 "),
            ui::span(style::ACCENT, "i"),
            ui::span(style::FAINT, " 进入按键直通后："),
            ui::span(style::ACCENT, "s"),
            ui::span(style::BASE, " 搜索播客，或 "),
            ui::span(style::ACCENT, "a"),
            ui::span(style::BASE, " 贴一个 RSS feed 地址"),
        ]);
        return;
    }
    let author_w = (width / 4).clamp(8, 20);
    let count_w = 5;
    let title_w = width
        .saturating_sub(author_w + count_w * 2 + 6)
        .max(10);

    ui::print_line(&[
        ui::span(style::FAINT, ui::pad_right("标题", title_w)),
        ui::span(style::FAINT, format!("  {}", ui::pad_right("作者", author_w))),
        ui::span(style::FAINT, format!("  {}", ui::pad_left("单集", count_w))),
        ui::span(style::FAINT, format!("  {}", ui::pad_left("未播", count_w))),
    ]);
    ui::print_line(&[ui::span(style::RULE, "─".repeat(width))]);

    let list_h = (rows.max(4) as usize).saturating_sub(3);
    let total = s.store.podcasts.len();
    let first = s.subs.prepare(total, list_h, 3);

    let snapshot: Vec<_> = s.store.podcasts.clone();
    for (i, p) in snapshot[first..].iter().take(list_h).enumerate() {
        let i = first + i;
        let selected = i == s.subs.selected;
        let title_style = if selected { style::STRONG } else { style::BASE };
        let unplayed = p.episodes.iter().filter(|e| !e.played).count();
        ui::print_line(&[
            ui::span(style::FAINT, ui::pad_left(&(i + 1).to_string(), 3)),
            ui::span(title_style, format!("  {}", ui::pad_right(&p.title, title_w.saturating_sub(3)))),
            ui::span(style::BASE, format!("  {}", ui::pad_right(&p.author, author_w))),
            ui::span(style::FAINT, format!("  {}", ui::pad_left(&p.episodes.len().to_string(), count_w))),
            ui::span(
                if unplayed > 0 { style::ACCENT } else { style::FAINT },
                format!("  {}", ui::pad_left(&unplayed.to_string(), count_w)),
            ),
        ]);
    }
}

/// 搜索结果：标题<flex> 作者<16> 集数<5>（已订阅的打 ✓）。
fn render_search(s: &mut UiState, width: usize, rows: i32) {
    let author_w = (width / 4).clamp(8, 20);
    let count_w = 5;
    let title_w = width.saturating_sub(author_w + count_w + 8).max(10);

    ui::print_line(&[
        ui::span(style::FAINT, ui::pad_right("标题", title_w)),
        ui::span(style::FAINT, format!("  {}", ui::pad_right("作者", author_w))),
        ui::span(style::FAINT, format!("  {}", ui::pad_left("单集", count_w))),
    ]);
    ui::print_line(&[ui::span(style::RULE, "─".repeat(width))]);

    if s.hits.is_empty() {
        s.hit_list.prepare(0, 1, 3);
        if s.search_task.is_some() {
            ui::print_line(&[ui::span(style::FAINT, "搜索中…")]);
        } else {
            ui::print_line(&[
                ui::span(style::FAINT, "没有匹配的播客。"),
                ui::span(style::BASE, "按 h 返回后换个关键词"),
                ui::span(style::FAINT, "（或 a 直接贴 RSS）"),
            ]);
        }
        return;
    }

    let start = s.page * SEARCH_PAGE;
    let end = (start + SEARCH_PAGE).min(s.hits.len());
    let total = end - start;
    let list_h = (rows.max(4) as usize).saturating_sub(3);
    let first = s.hit_list.prepare(total, list_h, 3);

    let page_hits = s.hits[start..end].to_vec();
    for (i, hit) in page_hits[first..].iter().take(list_h).enumerate() {
        let i = first + i;
        let selected = i == s.hit_list.selected;
        let title_style = if selected { style::STRONG } else { style::BASE };
        let subscribed = s
            .store
            .podcasts
            .iter()
            .any(|p| p.feed_url == hit.feed_url);
        ui::print_line(&[
            ui::span(style::FAINT, ui::pad_left(&(start + i + 1).to_string(), 3)),
            ui::span(
                title_style,
                format!("  {}", ui::pad_right(&hit.title, title_w.saturating_sub(3))),
            ),
            ui::span(style::BASE, format!("  {}", ui::pad_right(&hit.author, author_w))),
            ui::span(
                style::FAINT,
                format!("  {}", ui::pad_left(&hit.episodes.to_string(), count_w)),
            ),
            ui::span(
                if subscribed { style::OK } else { style::FAINT },
                if subscribed { "  ✓" } else { "" },
            ),
        ]);
    }
}

/// 单集列表：日期<6> 标题<flex> 时长<6> 状态<8>。
fn render_eps(s: &mut UiState, width: usize, rows: i32) {
    let date_w = 6;
    let dur_w = 6;
    let st_w = 8;
    let title_w = width.saturating_sub(date_w + dur_w + st_w + 6).max(10);

    ui::print_line(&[
        ui::span(style::FAINT, ui::pad_right("日期", date_w)),
        ui::span(style::FAINT, format!("  {}", ui::pad_right("标题", title_w))),
        ui::span(style::FAINT, format!("  {}", ui::pad_left("时长", dur_w))),
        ui::span(style::FAINT, format!("  {}", ui::pad_left("状态", st_w))),
    ]);
    ui::print_line(&[ui::span(style::RULE, "─".repeat(width))]);

    let episodes: Vec<Episode> = current_episodes(s).clone();
    if episodes.is_empty() {
        s.eps.prepare(0, 1, 3);
        ui::print_line(&[ui::span(style::FAINT, "没有单集（feed 为空或还在刷新）")]);
        return;
    }

    let list_h = (rows.max(4) as usize).saturating_sub(3);
    let total = episodes.len();
    let first = s.eps.prepare(total, list_h, 3);

    for (i, ep) in episodes[first..].iter().take(list_h).enumerate() {
        let i = first + i;
        let selected = i == s.eps.selected;
        let title_style = if selected { style::STRONG } else { style::BASE };
        let dl = s.downloads.iter().find(|d| d.guid == ep.guid);
        let (status_text, status_style): (String, &str) = if let Some(dl) = dl {
            if let Some(err) = &dl.failed {
                (format!("✗ {err}"), style::DANGER)
            } else if let Some(t) = dl.total.filter(|t| *t > 0) {
                (format!("⤓ {}%", dl.received * 100 / t), style::ACCENT)
            } else {
                ("⤓ …".into(), style::FAINT)
            }
        } else if ep.played {
            ("已播".into(), style::FAINT)
        } else if ep.in_progress() {
            let pct = (ep.position_ms * 100)
                .checked_div(ep.duration_ms)
                .map_or(0, |p| p.min(99));
            (format!("▶ {pct}%"), style::ACCENT)
        } else if ep.downloaded.is_some() {
            ("⤓ 本地".into(), style::OK)
        } else {
            ("新".into(), style::STRONG)
        };
        ui::print_line(&[
            ui::span(style::FAINT, ui::pad_right(&ep.pub_date, date_w)),
            ui::span(title_style, format!("  {}", ui::pad_right(&ep.title, title_w))),
            ui::span(style::FAINT, format!("  {}", ui::pad_left(&fmt_mmss(ep.duration_ms), dur_w))),
            ui::span(status_style, format!("  {}", ui::pad_left(&status_text, st_w))),
        ]);
    }
}

/// 常驻采样循环（1s）：进度记忆（批量回 UI 落盘）+ 播完推断连播。
///
/// 连播语义（不依赖任何推送，纯 status 采样）：
/// - 在播播客单集 → 记 position；剩余 ≤500ms 且在播 → 预切下一集
///   （试听态内 Peek 无缝换队列，不经 restore，无闪音）
/// - 上一集 ≥95% 播完导致切走（restore 到用户队列）→ 标已播 + 续播下一集
/// - 上一集未播完就切走（用户 Back/手动切歌）→ 停止跟随，不连播
struct LoopState {
    playing: Option<(String, String, u64, u64)>, // (feed_url, guid, position, duration)
    pending_positions: Vec<(String, u64)>,
    last_emit: u64,
}

fn podcast_worker() -> anyhow::Result<serde_json::Value> {
    let data_dir = data_dir();
    let mut state = LoopState {
        playing: None,
        pending_positions: Vec::new(),
        last_emit: 0,
    };
    loop {
        rmus_plugin_sdk::sleep(1000);
        if rmus_plugin_sdk::cancelled() {
            break;
        }
        let store = store::load(Path::new(&data_dir));
        let Ok(st) = daemon::status() else {
            continue;
        };
        let path = st
            .get("track")
            .and_then(|t| t.get("path"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let position = st.get("position_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let duration = st.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0);
        let playing = st.get("state").and_then(|v| v.as_str()) == Some("playing");
        let podcasts_root = Path::new(&data_dir).join("podcasts");

        let lookup = if path.is_empty() {
            None
        } else {
            store.find_by_path(Path::new(&data_dir), &path)
        };
        if let Some((pi, ei)) = lookup {
            let (feed_url, guid) = {
                let ep = &store.podcasts[pi].episodes[ei];
                (store.podcasts[pi].feed_url.clone(), ep.guid.clone())
            };
            state.playing = Some((feed_url.clone(), guid.clone(), position, duration));
            state.pending_positions.push((guid.clone(), position));
            // 即将播完：预切下一集（同 feed 顺序的下一个）。
            // 窗口取 1.2s（> 1s 采样间隔，必命中一次；播客砍 ~1s 片尾无感）。
            // 预切后多睡 2s：引擎切队列是异步的，避免下轮采样对旧集重复 Peek。
            let remaining = duration.saturating_sub(position);
            if playing
                && duration > 0
                && remaining <= 1200
                && let Some(next) = store.next_episode(&feed_url, &guid)
                && let Some(next_path) =
                    store.episode_path(Path::new(&data_dir), &store.podcasts[pi], next)
            {
                let _ = daemon::peek(vec![next_path.display().to_string()]);
                rmus_plugin_sdk::notify(&serde_json::json!({ "worker": "advance" }));
                rmus_plugin_sdk::sleep(2000);
            }
            let _ = podcasts_root;
        } else if let Some((feed_url, guid, last_pos, last_dur)) = state.playing.take() {
            // 切走了：≥95% 视为播完（restore 的自然结果），续播下一集
            let finished = last_dur > 0 && last_pos * 100 >= last_dur * 95;
            if finished && let Some(next) = store.next_episode(&feed_url, &guid)
                && let Some(next_path) = {
                    let pod = store.podcasts.iter().find(|p| p.feed_url == feed_url);
                    pod.and_then(|p| store.episode_path(Path::new(&data_dir), p, next))
                }
            {
                let _ = daemon::peek(vec![next_path.display().to_string()]);
                state.playing = Some((feed_url.clone(), next.guid.clone(), 0, next.duration_ms));
                rmus_plugin_sdk::notify(&serde_json::json!({ "worker": "advance" }));
            }
            if finished {
                state.pending_positions.push((guid, last_dur));
            } else {
                // 未播完的用户切走：下轮采样继续判定（playing 已清空即停连播）
                let _ = feed_url;
            }
        }

        // 进度批量落盘（经事件回 UI；UI 侧写文件，worker 不写避免并发）
        let now = rmus_plugin_sdk::now_ms();
        if !state.pending_positions.is_empty() && now - state.last_emit >= 5000 {
            state.last_emit = now;
            let updates: Vec<serde_json::Value> = state
                .pending_positions
                .drain(..)
                .map(|(guid, position)| serde_json::json!({ "guid": guid, "position_ms": position }))
                .collect();
            rmus_plugin_sdk::notify(&serde_json::json!({
                "worker": "positions", "updates": updates,
            }));
        }
    }
    Ok(serde_json::json!({ "stopped": true }))
}
