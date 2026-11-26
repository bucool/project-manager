//! Local visual references. Media stays at its original location; only small
//! thumbnails and the reference index live in the user's configured data folder.
use crate::{backend, i18n::tr, smooth_scroll::SmoothScroll, theme::Colors};
use anyhow::{Context as _, Result, ensure};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, Icon, IconName,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    scroll::{Scrollbar, ScrollbarShow},
};
use serde::{Deserialize, Serialize};
#[path = "inspiration_media.rs"]
mod media;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy)]
pub enum Collection {
    Aesthetics,
    Ideas,
}
impl Collection {
    fn title(self) -> &'static str {
        tr(match self {
            Self::Aesthetics => "审美记录",
            Self::Ideas => "灵感",
        })
    }
    fn folder(self) -> &'static str {
        match self {
            // Keep the original location for existing aesthetic references.
            Self::Aesthetics => "inspiration",
            Self::Ideas => "ideas",
        }
    }
    fn loading_message(self) -> &'static str {
        match self {
            Self::Aesthetics => "正在读取审美记录…",
            Self::Ideas => "正在读取灵感…",
        }
    }
    fn save_error(self) -> &'static str {
        tr(match self {
            Self::Aesthetics => "审美记录保存失败",
            Self::Ideas => "灵感保存失败",
        })
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
enum Kind {
    #[default]
    Link,
    Image,
    Video,
}
impl Kind {
    fn label(self) -> &'static str {
        tr(match self {
            Self::Link => "链接",
            Self::Image => "图片",
            Self::Video => "视频",
        })
    }
    fn action(self) -> &'static str {
        tr(match self {
            Self::Link => "添加链接",
            Self::Image => "导入图片",
            Self::Video => "导入视频",
        })
    }
    fn icon(self) -> IconName {
        match self {
            Self::Link => IconName::Globe,
            Self::Image => IconName::GalleryVerticalEnd,
            Self::Video => IconName::Frame,
        }
    }
    fn accepts(self, path: &Path) -> bool {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        match self {
            Self::Image => [
                "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "avif", "svg", "ico",
                "heic", "heif",
            ]
            .contains(&extension.as_str()),
            Self::Video => [
                "mp4", "mkv", "mov", "avi", "webm", "wmv", "m4v", "mpeg", "mpg", "flv", "ts",
                "mts", "m2ts", "3gp",
            ]
            .contains(&extension.as_str()),
            Self::Link => false,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    id: String,
    kind: Kind,
    title: String,
    target: String,
    #[serde(default)]
    bytes: u64,
    #[serde(skip)]
    thumbnail: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Library {
    #[serde(default)]
    entries: Vec<Entry>,
}
enum Job {
    Load(PathBuf),
    AddLink { title: String, target: String },
    Import { kind: Kind, paths: Vec<PathBuf> },
    Remove(String),
    Open(Entry),
    Flush(mpsc::Sender<()>),
}
struct Event {
    root: PathBuf,
    library: Option<Library>,
    done: bool,
    close_dialog: bool,
    message: &'static str,
    error: Option<String>,
}
#[derive(Default)]
struct Writers(Vec<mpsc::Sender<Job>>);
impl Global for Writers {}
pub fn flush(cx: &mut App) {
    if let Some(writers) = cx.try_global::<Writers>() {
        let pending: Vec<_> = writers
            .0
            .iter()
            .filter_map(|writer| {
                let (tx, rx) = mpsc::channel();
                writer.send(Job::Flush(tx)).is_ok().then_some(rx)
            })
            .collect();
        for rx in pending {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }
}

fn unique() -> String {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    )
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}
fn thumbnail_path(root: &Path, id: &str) -> PathBuf {
    root.join("thumbnails").join(format!("{id}-large.jpg"))
}
fn save(root: &Path, library: &Library) -> Result<()> {
    fs::create_dir_all(root)?;
    let mut file = tempfile::NamedTempFile::new_in(root)?;
    file.write_all(&serde_json::to_vec_pretty(library)?)?;
    file.as_file().sync_all()?;
    file.persist(root.join("index.json"))
        .map_err(|error| error.error)?;
    Ok(())
}
fn validate_url(raw: &str) -> Result<url::Url> {
    let url = url::Url::parse(raw.trim()).context(tr("请输入完整的 http 或 https 链接。"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some(),
        "{}",
        tr("请输入完整的 http 或 https 链接。")
    );
    Ok(url)
}
fn load(root: &Path) -> Result<Library> {
    let file = root.join("index.json");
    let mut library: Library = if file.try_exists()? {
        serde_json::from_slice(&fs::read(file)?)?
    } else {
        Library::default()
    };
    for entry in &mut library.entries {
        ensure!(valid_id(&entry.id), "Invalid reference identifier");
        if entry.kind == Kind::Link {
            validate_url(&entry.target)?;
        } else {
            ensure!(Path::new(&entry.target).is_absolute(), "Invalid media path");
            ensure!(
                entry.kind.accepts(Path::new(&entry.target)),
                "Invalid media type"
            );
        }
        entry.thumbnail = entry.kind != Kind::Link && thumbnail_path(root, &entry.id).is_file();
    }
    Ok(library)
}
fn make_thumbnail(root: &Path, entry: &Entry) -> Result<()> {
    let source = if entry.kind == Kind::Video {
        media::video_thumbnail(&entry.target)?
    } else {
        let mut reader = image::ImageReader::open(&entry.target)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(64 * 1024 * 1024);
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        reader.limits(limits);
        reader.decode()?
    };
    // Shell providers may return an older, smaller cached frame. Scale it to
    // fit the card too, rather than centering a tiny frame in a large canvas.
    let image = if source.width() < 768 && source.height() < 480 {
        source.resize(768, 480, image::imageops::FilterType::Triangle)
    } else {
        source.thumbnail(768, 480)
    }
    .into_rgba8();
    let mut canvas = image::RgbaImage::from_pixel(768, 480, image::Rgba([244, 245, 240, 255]));
    image::imageops::overlay(
        &mut canvas,
        &image,
        ((768 - image.width()) / 2) as i64,
        ((480 - image.height()) / 2) as i64,
    );
    let directory = root.join("thumbnails");
    fs::create_dir_all(&directory)?;
    let mut file = tempfile::NamedTempFile::new_in(&directory)?;
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut file, 80)
        .encode_image(&image::DynamicImage::ImageRgba8(canvas).into_rgb8())?;
    file.persist(thumbnail_path(root, &entry.id))
        .map_err(|error| error.error)?;
    Ok(())
}
fn file_key(value: &str) -> String {
    if cfg!(windows) {
        value.to_lowercase()
    } else {
        value.to_owned()
    }
}

fn worker(jobs: mpsc::Receiver<Job>, events: async_channel::Sender<Event>) {
    let mut root = None;
    let mut library = Library::default();
    while let Ok(job) = jobs.recv() {
        if let Job::Flush(done) = job {
            let _ = done.send(());
            continue;
        }
        if let Job::Load(directory) = job {
            root = Some(directory.clone());
            let result = load(&directory);
            if let Ok(loaded) = &result {
                library = loaded.clone();
                let _ = events.send_blocking(Event {
                    root: directory.clone(),
                    library: Some(library.clone()),
                    done: false,
                    close_dialog: false,
                    message: "正在读取缩略图…",
                    error: None,
                });
                for entry in &mut library.entries {
                    if entry.kind != Kind::Link && !entry.thumbnail {
                        entry.thumbnail = make_thumbnail(&directory, entry).is_ok();
                    }
                }
            }
            let _ = events.send_blocking(Event {
                root: directory,
                library: result.as_ref().ok().map(|_| library.clone()),
                done: true,
                close_dialog: false,
                message: "已保存",
                error: result.err().map(|error| format!("{error:#}")),
            });
            continue;
        }
        let Some(root) = &root else {
            continue;
        };
        let mut close_dialog = false;
        let mut message = "已保存";
        let result = (|| -> Result<()> {
            match job {
                Job::AddLink { title, target } => {
                    let url = validate_url(&target)?;
                    let target = url.to_string();
                    close_dialog = true;
                    if library
                        .entries
                        .iter()
                        .any(|entry| entry.kind == Kind::Link && entry.target == target)
                    {
                        message = "记录已存在";
                        return Ok(());
                    }
                    let title = if title.trim().is_empty() {
                        url.host_str().unwrap_or(&target).to_owned()
                    } else {
                        title.trim().to_owned()
                    };
                    let mut next = library.clone();
                    next.entries.insert(
                        0,
                        Entry {
                            id: unique(),
                            kind: Kind::Link,
                            title,
                            target,
                            bytes: 0,
                            thumbnail: false,
                        },
                    );
                    save(root, &next)?;
                    library = next;
                }
                Job::Import { kind, paths } => {
                    let mut next = library.clone();
                    let mut ids = Vec::new();
                    let mut skipped = false;
                    for path in paths {
                        let Ok(path) = fs::canonicalize(path) else {
                            skipped = true;
                            continue;
                        };
                        if !path.is_file() || !kind.accepts(&path) {
                            skipped = true;
                            continue;
                        }
                        let target = crate::storage::display_path(&path);
                        if next.entries.iter().any(|entry| {
                            entry.kind == kind && file_key(&entry.target) == file_key(&target)
                        }) {
                            continue;
                        }
                        let title = path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned();
                        let id = unique();
                        next.entries.insert(
                            0,
                            Entry {
                                id: id.clone(),
                                kind,
                                title,
                                target,
                                bytes: fs::metadata(path)?.len(),
                                thumbnail: false,
                            },
                        );
                        ids.push(id);
                    }
                    save(root, &next)?;
                    library = next;
                    // Persist references before thumbnail generation, so a quick exit
                    // cannot lose the imported list. Decoding stays on this worker.
                    let _ = events.send_blocking(Event {
                        root: root.clone(),
                        library: Some(library.clone()),
                        done: false,
                        close_dialog: false,
                        message: "正在导入…",
                        error: None,
                    });
                    if kind != Kind::Link {
                        for id in ids {
                            if let Some(entry) =
                                library.entries.iter_mut().find(|entry| entry.id == id)
                            {
                                entry.thumbnail = make_thumbnail(root, entry).is_ok();
                            }
                        }
                    }
                    message = if skipped {
                        "部分文件未添加，请选择对应类型的文件。"
                    } else {
                        "已添加"
                    };
                }
                Job::Remove(id) => {
                    let mut next = library.clone();
                    next.entries.retain(|entry| entry.id != id);
                    save(root, &next)?;
                    library = next;
                    if valid_id(&id) {
                        let _ = fs::remove_file(thumbnail_path(root, &id));
                    }
                    close_dialog = true;
                    message = "已移除";
                }
                Job::Open(entry) => {
                    if entry.kind == Kind::Link {
                        media::open_link(&entry.target)?;
                    } else {
                        ensure!(
                            Path::new(&entry.target).is_file(),
                            "{}",
                            tr("文件已移动或删除，请重新添加。")
                        );
                        backend::open(&entry.target)?;
                    }
                }
                Job::Load(_) | Job::Flush(_) => unreachable!(),
            }
            Ok(())
        })();
        let _ = events.send_blocking(Event {
            root: root.clone(),
            library: result.as_ref().ok().map(|_| library.clone()),
            done: true,
            close_dialog,
            message,
            error: result.err().map(|error| format!("{error:#}")),
        });
    }
}

#[derive(Clone)]
enum Dialog {
    Link,
    Remove(String),
}
pub struct Inspiration {
    collection: Collection,
    library: Library,
    root: Option<PathBuf>,
    sender: mpsc::Sender<Job>,
    loaded: bool,
    busy: bool,
    status: String,
    failed: bool,
    active: Kind,
    dialog: Option<Dialog>,
    dialog_error: String,
    title: Entity<InputState>,
    url: Entity<InputState>,
    focus: FocusHandle,
    list: ListState,
    list_key: (Kind, Vec<String>, usize, i32),
    smooth: SmoothScroll,
    localized_english: bool,
    _subscriptions: Vec<Subscription>,
    pub colors: Colors,
}
impl Inspiration {
    pub fn new(collection: Collection, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| InputState::new(window, cx).placeholder(tr("标题（可选）")));
        let url = cx.new(|cx| InputState::new(window, cx).placeholder("https://"));
        let (sender, jobs) = mpsc::channel();
        let (events, receiver) = async_channel::unbounded();
        std::thread::spawn(move || worker(jobs, events));
        cx.default_global::<Writers>().0.push(sender.clone());
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = receiver.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        if this.root.as_ref() != Some(&event.root) {
                            return;
                        }
                        this.busy = !event.done;
                        if let Some(library) = event.library {
                            this.library = library;
                            this.loaded = true;
                        }
                        this.failed = event.error.is_some();
                        this.status = event.error.unwrap_or_else(|| tr(event.message).into());
                        if this.failed && this.dialog.is_some() {
                            this.dialog_error = this.status.clone();
                        }
                        if event.close_dialog && !this.failed {
                            this.dialog = None;
                            this.dialog_error.clear();
                            this.focus.focus(window);
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        let mut subscriptions = Vec::new();
        for input in [&title, &url] {
            subscriptions.push(cx.subscribe_in(input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. })
                    && matches!(this.dialog, Some(Dialog::Link))
                {
                    this.add_link(cx);
                }
            }));
        }
        Self {
            collection,
            library: Library::default(),
            root: None,
            sender,
            loaded: false,
            busy: false,
            status: String::new(),
            failed: false,
            active: Kind::Link,
            dialog: None,
            dialog_error: String::new(),
            title,
            url,
            focus: cx.focus_handle(),
            list: ListState::new(0, ListAlignment::Top, px(300.)),
            list_key: (Kind::Link, Vec::new(), 1, 0),
            smooth: SmoothScroll::default(),
            localized_english: crate::i18n::english(),
            _subscriptions: subscriptions,
            colors: crate::theme::find("default").colors(),
        }
    }
    pub fn count(&self) -> usize {
        self.library.entries.len()
    }
    pub fn is_busy(&self) -> bool {
        self.busy
    }
    pub fn set_storage(&mut self, directory: Option<&str>, cx: &mut Context<Self>) {
        let root =
            directory.map(|directory| PathBuf::from(directory).join(self.collection.folder()));
        if root == self.root {
            return;
        }
        self.root = root.clone();
        self.library = Library::default();
        self.loaded = false;
        self.dialog = None;
        self.dialog_error.clear();
        if let Some(root) = root {
            self.send(Job::Load(root), self.collection.loading_message(), cx);
        }
    }
    fn send(&mut self, job: Job, message: &'static str, cx: &mut Context<Self>) {
        if self.sender.send(job).is_err() {
            self.busy = false;
            self.status = self.collection.save_error().into();
            self.failed = true;
            if self.dialog.is_some() {
                self.dialog_error = self.status.clone();
            }
        } else {
            self.busy = true;
            self.failed = false;
            self.status = tr(message).into();
        }
        cx.notify();
    }
    fn add_link(&mut self, cx: &mut Context<Self>) {
        if self.busy || !self.loaded {
            return;
        }
        let target = self.url.read(cx).value().to_string();
        if let Err(error) = validate_url(&target) {
            self.dialog_error = error.to_string();
            cx.notify();
            return;
        }
        self.dialog_error.clear();
        self.send(
            Job::AddLink {
                title: self.title.read(cx).value().to_string(),
                target,
            },
            "正在保存…",
            cx,
        );
    }
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || !self.loaded {
            return;
        }
        if self.active == Kind::Link {
            self.dialog = Some(Dialog::Link);
            self.dialog_error.clear();
            self.title
                .update(cx, |input, cx| input.set_value("", window, cx));
            self.url.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.focus(window, cx);
            });
            cx.notify();
            return;
        }
        let kind = self.active;
        let expected_root = self.root.clone();
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(kind.action().into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picker.await {
                let _ = this.update(cx, |this, cx| {
                    if !paths.is_empty() && !this.busy && this.loaded && this.root == expected_root
                    {
                        this.send(Job::Import { kind, paths }, "正在导入…", cx);
                    }
                });
            }
        })
        .detach();
    }
    fn close_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.busy {
            self.dialog = None;
            self.dialog_error.clear();
            self.focus.focus(window);
            cx.notify();
        }
    }
    fn row(&self, entry: Entry, cx: &Context<Self>) -> AnyElement {
        let colors = self.colors;
        let open = entry.clone();
        let id = entry.id.clone();
        let copy_link = entry.target.clone();
        let mut art = div()
            .w(px(112.))
            .h(px(70.))
            .flex_shrink_0()
            .rounded(px(7.))
            .overflow_hidden()
            .bg(rgb(colors.soft))
            .flex()
            .items_center()
            .justify_center();
        if entry.thumbnail {
            if let Some(root) = &self.root {
                art = art.child(
                    img(thumbnail_path(root, &entry.id))
                        .w(px(112.))
                        .h(px(70.))
                        .object_fit(ObjectFit::Contain),
                );
            }
        } else {
            art = art.child(
                if entry.kind == Kind::Video {
                    Icon::new(crate::assets::ActionIcon::Play).size(px(26.))
                } else {
                    Icon::new(entry.kind.icon()).size(px(26.))
                }
                .text_color(rgb(colors.accent)),
            );
        }
        let caption = if entry.kind == Kind::Link {
            entry.target.clone()
        } else {
            format!(
                "{} · {}",
                Path::new(&entry.target)
                    .extension()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_uppercase(),
                format_bytes(entry.bytes)
            )
        };
        div()
            .pr(px(16.))
            .pb_3()
            .child(
                div()
                    .id(SharedString::from(format!("reference-{}", entry.id)))
                    .flex()
                    .items_center()
                    .gap_4()
                    .p_3()
                    .rounded(px(11.))
                    .border_1()
                    .border_color(rgb(colors.line))
                    .bg(rgb(colors.surface))
                    .hover(move |s| s.bg(rgb(colors.hover)))
                    .cursor_pointer()
                    .child(art)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_size(px(15.))
                                    .truncate()
                                    .child(entry.title),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors.muted))
                                    .mt_2()
                                    .truncate()
                                    .child(caption),
                            )
                            .when(entry.kind != Kind::Link, |s| {
                                s.child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(colors.muted))
                                        .mt_1()
                                        .truncate()
                                        .child(entry.target),
                                )
                            }),
                    )
                    .when(entry.kind == Kind::Link, |s| {
                        s.child(
                            Button::new(SharedString::from(format!("copy-reference-{id}")))
                                .ghost()
                                .icon(IconName::Copy)
                                .tooltip(tr("复制链接"))
                                .w(px(36.))
                                .h(px(36.))
                                .disabled(self.busy)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        copy_link.clone(),
                                    ));
                                    this.status = tr("链接已复制").into();
                                    this.failed = false;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("remove-reference-{id}")))
                            .ghost()
                            .icon(crate::assets::ActionIcon::Trash)
                            .tooltip(tr("移除记录"))
                            .w(px(36.))
                            .h(px(36.))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.dialog = Some(Dialog::Remove(id.clone()));
                                this.focus.focus(window);
                                cx.notify();
                            })),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.busy {
                            this.send(Job::Open(open.clone()), "正在打开…", cx);
                        }
                    })),
            )
            .into_any_element()
    }

    fn media_card(&self, entry: Entry, width: f32, cx: &Context<Self>) -> AnyElement {
        let colors = self.colors;
        let open = entry.clone();
        let id = entry.id.clone();
        let height = width * 0.625;
        let mut preview = div()
            .relative()
            .w(px(width))
            .h(px(height))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(colors.soft))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2();
        if entry.thumbnail {
            if let Some(root) = &self.root {
                preview = preview.child(
                    img(thumbnail_path(root, &entry.id))
                        .w(px(width))
                        .h(px(height))
                        .object_fit(ObjectFit::Contain),
                );
            }
        } else {
            preview = preview
                .child(
                    Icon::new(entry.kind.icon())
                        .size(px(42.))
                        .text_color(rgb(colors.accent)),
                )
                .child(
                    div()
                        .text_size(px(12.))
                        .text_color(rgb(colors.muted))
                        .child(tr("暂无缩略图")),
                );
        }
        if entry.kind == Kind::Video {
            preview = preview.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(
                        div()
                            .w(px(48.))
                            .h(px(48.))
                            .rounded_full()
                            .bg(rgba(0x00000088))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                Icon::new(crate::assets::ActionIcon::Play)
                                    .size(px(24.))
                                    .text_color(rgb(0xffffff)),
                            ),
                    ),
            );
        }
        div()
            .id(SharedString::from(format!("media-reference-{id}")))
            .w(px(width))
            .flex_shrink_0()
            .rounded(px(12.))
            .overflow_hidden()
            .border_1()
            .border_color(rgb(colors.line))
            .bg(rgb(colors.surface))
            .hover(move |s| s.border_color(rgb(colors.accent_border)))
            .cursor_pointer()
            .child(preview)
            .child(
                div()
                    .p_3()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .truncate()
                                    .child(entry.title),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors.muted))
                                    .child(format!(
                                        "{} · {}",
                                        Path::new(&entry.target)
                                            .extension()
                                            .unwrap_or_default()
                                            .to_string_lossy()
                                            .to_uppercase(),
                                        format_bytes(entry.bytes)
                                    )),
                            )
                            .child(
                                div()
                                    .mt_1()
                                    .text_size(px(11.))
                                    .text_color(rgb(colors.muted))
                                    .truncate()
                                    .child(entry.target),
                            ),
                    )
                    .child(
                        Button::new(SharedString::from(format!("remove-media-{id}")))
                            .ghost()
                            .icon(crate::assets::ActionIcon::Trash)
                            .tooltip(tr("移除记录"))
                            .w(px(36.))
                            .h(px(36.))
                            .disabled(self.busy)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.dialog = Some(Dialog::Remove(id.clone()));
                                this.focus.focus(window);
                                cx.notify();
                            })),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.busy {
                    this.send(Job::Open(open.clone()), "正在打开…", cx);
                }
            }))
            .into_any_element()
    }
    fn modal(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(dialog) = &self.dialog else {
            return div().into_any_element();
        };
        let colors = self.colors;
        let deleting = matches!(dialog, Dialog::Remove(_));
        let name = match dialog {
            Dialog::Remove(id) => self
                .library
                .entries
                .iter()
                .find(|entry| &entry.id == id)
                .map(|entry| entry.title.clone()),
            _ => None,
        };
        div()
            .absolute()
            .inset_0()
            .occlude()
            .bg(rgba((colors.overlay << 8) | 0xb5))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(480.))
                    .p_6()
                    .rounded(px(14.))
                    .bg(rgb(colors.bg))
                    .border_1()
                    .border_color(rgb(colors.line))
                    .shadow_xl()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if deleting {
                                        tr("移除记录")
                                    } else {
                                        tr("添加链接")
                                    }),
                            )
                            .child(
                                Button::new("close-reference-dialog")
                                    .ghost()
                                    .icon(IconName::Close)
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_dialog(window, cx)
                                    })),
                            ),
                    )
                    .when_some(name, |s, name| {
                        s.child(name).child(tr("确定移除这条记录吗？"))
                    })
                    .when(!deleting, |s| {
                        s.child(Input::new(&self.title).h(px(40.)).disabled(self.busy))
                            .child(Input::new(&self.url).h(px(40.)).disabled(self.busy))
                    })
                    .when(!self.dialog_error.is_empty(), |s| {
                        s.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(0xbf4b45))
                                .child(self.dialog_error.clone()),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-reference")
                                    .ghost()
                                    .label(tr("取消"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.close_dialog(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("save-reference")
                                    .primary()
                                    .label(if deleting { tr("移除") } else { tr("添加") })
                                    .disabled(self.busy)
                                    .on_click(cx.listener(
                                        |this, _, _, cx| match this.dialog.clone() {
                                            Some(Dialog::Remove(id)) => {
                                                this.send(Job::Remove(id), "正在保存…", cx)
                                            }
                                            Some(Dialog::Link) => this.add_link(cx),
                                            _ => {}
                                        },
                                    )),
                            ),
                    ),
            )
            .into_any_element()
    }
}
fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024. * 1024. * 1024.))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024. * 1024.))
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.)
    }
}
impl Render for Inspiration {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let english = crate::i18n::english();
        if self.localized_english != english {
            self.title.update(cx, |input, cx| {
                input.set_placeholder(tr("标题（可选）"), window, cx);
            });
            self.localized_english = english;
        }
        let colors = self.colors;
        let entries: Vec<_> = self
            .library
            .entries
            .iter()
            .filter(|entry| entry.kind == self.active)
            .cloned()
            .collect();
        let available = (f32::from(window.viewport_size().width) - 228. - 48. - 16.).max(200.);
        let columns = if self.active == Kind::Link {
            1
        } else {
            ((available + 16.) / 336.).floor().max(1.) as usize
        };
        let card_width = (available - (columns - 1) as f32 * 16.) / columns as f32;
        let row_count = entries.len().div_ceil(columns);
        let key = (
            self.active,
            entries.iter().map(|entry| entry.id.clone()).collect(),
            columns,
            if self.active == Kind::Link {
                0
            } else {
                card_width.round() as i32
            },
        );
        if self.list_key != key {
            self.smooth.cancel();
            self.list.reset(row_count);
            self.list_key = key;
        }
        let empty = entries.is_empty();
        div()
            .size_full()
            .min_w_0()
            .relative()
            .flex()
            .flex_col()
            .p_6()
            .pb_0()
            .gap_4()
            .key_context("Inspiration")
            .track_focus(&self.focus)
            .text_color(rgb(colors.ink))
            .bg(rgb(colors.bg))
            .on_action(
                cx.listener(|this, _: &crate::ui::CloseOverlay, window, cx| {
                    if this.dialog.is_some() {
                        this.close_dialog(window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_4()
                    .flex_shrink_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(24.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(self.collection.title()),
                            )
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(colors.muted))
                                    .child(self.count().to_string()),
                            ),
                    )
                    .child(
                        Button::new("add-reference")
                            .primary()
                            .icon(IconName::Plus)
                            .h(px(38.))
                            .label(self.active.action())
                            .disabled(self.busy || !self.loaded)
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .flex_shrink_0()
                    .children(
                        [Kind::Link, Kind::Image, Kind::Video]
                            .into_iter()
                            .enumerate()
                            .map(|(index, kind)| {
                                let count = self
                                    .library
                                    .entries
                                    .iter()
                                    .filter(|entry| entry.kind == kind)
                                    .count();
                                Button::new(("reference-tab", index))
                                    .ghost()
                                    .h(px(38.))
                                    .min_w(px(106.))
                                    .rounded(px(8.))
                                    .border_1()
                                    .border_color(rgb(if self.active == kind {
                                        colors.accent_border
                                    } else {
                                        colors.line
                                    }))
                                    .bg(rgb(if self.active == kind {
                                        colors.selected
                                    } else {
                                        colors.bg
                                    }))
                                    .label(format!("{}  {}", kind.label(), count))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.active = kind;
                                        this.smooth.cancel();
                                        cx.notify();
                                    }))
                            }),
                    ),
            )
            .when(!self.status.is_empty(), |s| {
                s.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .flex_shrink_0()
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(if self.failed { 0xbf4b45 } else { colors.muted }))
                                .child(self.status.clone()),
                        )
                        .when(self.failed && !self.loaded, |s| {
                            s.child(
                                Button::new("retry-references")
                                    .ghost()
                                    .icon(crate::assets::ActionIcon::Refresh)
                                    .label(tr("重试"))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if let Some(root) = this.root.clone() {
                                            this.send(
                                                Job::Load(root),
                                                this.collection.loading_message(),
                                                cx,
                                            );
                                        }
                                    })),
                            )
                        }),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .relative()
                    .when(empty, |s| {
                        s.child(
                            div()
                                .size_full()
                                .flex()
                                .flex_col()
                                .items_center()
                                .justify_center()
                                .gap_3()
                                .child(
                                    Icon::new(self.active.icon())
                                        .size(px(40.))
                                        .text_color(rgb(colors.accent)),
                                )
                                .child(
                                    div()
                                        .text_size(px(16.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(tr("还没有记录")),
                                )
                                .child(
                                    div()
                                        .text_size(px(13.))
                                        .text_color(rgb(colors.muted))
                                        .child(match self.active {
                                            Kind::Link => tr("添加喜欢的网站、作品或参考链接。"),
                                            Kind::Image => {
                                                tr("添加图片，点击后使用默认看图软件打开。")
                                            }
                                            Kind::Video => {
                                                tr("添加视频，点击后使用默认视频软件打开。")
                                            }
                                        }),
                                ),
                        )
                    })
                    .when(!empty, |s| {
                        s.child(
                            list(
                                self.list.clone(),
                                cx.processor(move |this, index: usize, _, cx| {
                                    if this.active == Kind::Link {
                                        this.row(entries[index].clone(), cx)
                                    } else {
                                        let start = index * columns;
                                        let end = (start + columns).min(entries.len());
                                        div()
                                            .flex()
                                            .gap(px(16.))
                                            .pr(px(16.))
                                            .pb_4()
                                            .children(entries[start..end].iter().map(|entry| {
                                                this.media_card(entry.clone(), card_width, cx)
                                            }))
                                            .into_any_element()
                                    }
                                }),
                            )
                            .size_full(),
                        )
                        .child(self.smooth.layer(self.list.clone()))
                        .child(
                            Scrollbar::vertical(&self.smooth.handle(&self.list))
                                .scrollbar_show(ScrollbarShow::Always),
                        )
                    }),
            )
            .when(self.dialog.is_some(), |s| s.child(self.modal(cx)))
    }
}
