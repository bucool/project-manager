//! Local notes. All disk writes are ordered on a dedicated, idle-blocking worker.
use crate::i18n::tr;
use anyhow::{Context as _, Result, bail};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, IconName,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    text::{TextView, TextViewStyle},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, RwLock, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Resolve relative Markdown image URLs locally, without a server or network request.
#[derive(Default)]
struct NoteImages {
    root: PathBuf,
    pending: HashMap<String, Arc<Vec<u8>>>,
}
struct NoteImageClient {
    fallback: Arc<dyn gpui::http_client::HttpClient>,
    images: Arc<RwLock<NoteImages>>,
    executor: BackgroundExecutor,
}
impl gpui::http_client::HttpClient for NoteImageClient {
    fn send(
        &self,
        request: gpui::http_client::Request<gpui::http_client::AsyncBody>,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<
                        gpui::http_client::Response<gpui::http_client::AsyncBody>,
                    >,
                > + Send,
        >,
    > {
        let uri = request.uri();
        if uri.scheme().is_none() {
            if let Some(name) = uri
                .path()
                .trim_start_matches('/')
                .strip_prefix("notes-images/")
            {
                if image_name(name) {
                    let name = name.to_owned();
                    let images = self.images.clone();
                    return Box::pin(self.executor.spawn(async move {
                        let (path, pending) = {
                            let images = images
                                .read()
                                .map_err(|_| anyhow::anyhow!("Image storage unavailable"))?;
                            (
                                images.root.join("notes-images").join(&name),
                                images.pending.get(&name).cloned(),
                            )
                        };
                        let bytes = if let Some(bytes) = pending {
                            bytes.as_ref().clone()
                        } else {
                            fs::read(path)?
                        };
                        Ok(gpui::http_client::Response::builder()
                            .status(200)
                            .body(bytes.into())?)
                    }));
                }
            }
        }
        self.fallback.send(request)
    }
    fn user_agent(&self) -> Option<&gpui::http_client::http::HeaderValue> {
        self.fallback.user_agent()
    }
    fn proxy(&self) -> Option<&url::Url> {
        self.fallback.proxy()
    }
    fn type_name(&self) -> &'static str {
        "ProjectManagerNoteImages"
    }
}
#[derive(Clone, Copy, PartialEq)]
enum MarkdownMode {
    Edit,
    Split,
    Preview,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Note {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    images: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Library {
    notes: Vec<Note>,
    active: usize,
    #[serde(default = "default_font_size")]
    font_size: u16,
    #[serde(default)]
    inline_images: bool,
}
fn default_font_size() -> u16 {
    16
}
impl Default for Library {
    fn default() -> Self {
        Self {
            notes: Vec::new(),
            active: 0,
            font_size: default_font_size(),
            inline_images: true,
        }
    }
}
enum Command {
    Save(u64, Library, Vec<(String, Vec<u8>)>),
    Move(u64, PathBuf),
    Flush(mpsc::Sender<()>),
}
struct Writer(mpsc::Sender<Command>);
impl Global for Writer {}
pub fn flush(cx: &mut App) {
    if let Some(writer) = cx.try_global::<Writer>() {
        let (tx, rx) = mpsc::channel();
        if writer.0.send(Command::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
    }
}
fn locator() -> Result<PathBuf> {
    Ok(
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA unavailable")?)
            .join("ProjectManager/notes-location.json"),
    )
}
fn location() -> Result<Option<PathBuf>> {
    let config = locator()?;
    if config.exists() {
        return Ok(Some(serde_json::from_slice(&fs::read(config)?)?));
    }
    Ok(crate::storage::load_location()?.map(|root| root.join("notes")))
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Missing parent directory")?;
    fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}
fn image_name(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value).components().count() == 1
        && !value.contains(['/', '\\', ':'])
        && !value.starts_with('.')
}
fn save(root: &Path, library: &Library, images: Vec<(String, Vec<u8>)>) -> Result<()> {
    for (name, bytes) in images {
        if !image_name(&name) {
            bail!("Invalid image filename");
        }
        atomic(&root.join("notes-images").join(name), &bytes)?;
    }
    atomic(
        &root.join("notes.json"),
        &serde_json::to_vec_pretty(library)?,
    )
}
fn migrate(root: &mut PathBuf, target: &Path) -> Result<()> {
    if !target.is_absolute() {
        bail!("Choose an absolute folder path");
    }
    fs::create_dir_all(target)?;
    let target = target.canonicalize()?;
    let source = root.canonicalize()?;
    if target == source {
        return Ok(());
    }
    if target.starts_with(&source) || source.starts_with(&target) {
        bail!("Choose a separate folder outside the current notes folder");
    }
    if target.join("notes.json").exists() || target.join("notes-images").exists() {
        bail!("The destination already contains notes; choose an empty folder");
    }
    let mut files = Vec::new();
    let images = source.join("notes-images");
    if images.try_exists()? {
        reject_link(&images)?;
        for entry in fs::read_dir(&images)? {
            let entry = entry?;
            reject_link(&entry.path())?;
            if !entry.file_type()?.is_file() {
                bail!("Unsupported notes image entry");
            }
            files.push(PathBuf::from("notes-images").join(entry.file_name()));
        }
    }
    if source.join("notes.json").try_exists()? {
        reject_link(&source.join("notes.json"))?;
        files.push(PathBuf::from("notes.json"));
    }
    for relative in &files {
        let bytes = fs::read(source.join(relative))?;
        let destination = target.join(relative);
        atomic(&destination, &bytes)?;
        if fs::read(&destination)? != bytes {
            bail!("Notes migration verification failed");
        }
    }
    atomic(&locator()?, &serde_json::to_vec_pretty(&target)?)?;
    // The destination is now authoritative, even if a locked original cannot be removed.
    *root = target;
    for relative in files {
        let original = source.join(relative);
        if !original.starts_with(&source) {
            bail!("Invalid source path");
        }
        fs::remove_file(&original)
            .with_context(|| format!("Notes moved, but unable to remove {}", original.display()))?;
    }
    if images.try_exists()? && fs::read_dir(&images)?.next().is_none() {
        fs::remove_dir(&images)?;
    }
    if fs::read_dir(&source)?.next().is_none() {
        fs::remove_dir(&source)?;
    }
    Ok(())
}
fn reject_link(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        bail!("Linked notes paths cannot be migrated");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!("Linked notes paths cannot be migrated");
        }
    }
    Ok(())
}

fn unique() -> String {
    format!(
        "{:x}-{:x}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        std::process::id()
    )
}

pub struct Notes {
    library: Library,
    title_edit: Option<Option<String>>,
    dialog_focus: FocusHandle,
    font_settings_open: bool,
    markdown_mode: MarkdownMode,
    image_store: Arc<RwLock<NoteImages>>,
    delete_note: Option<String>,
    title: Entity<InputState>,
    body: Entity<InputState>,
    root: Option<PathBuf>,
    sender: Option<mpsc::Sender<Command>>,
    revision: u64,
    pending_images: Vec<(String, Vec<u8>)>,
    status: String,
    failed: bool,
    moving: bool,
    move_revision: Option<u64>,
    loaded: bool,
    _subscriptions: Vec<Subscription>,
    pub colors: crate::theme::Colors,
}
impl Notes {
    pub fn count(&self) -> usize {
        self.library.notes.len()
    }
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let title = cx.new(|cx| InputState::new(window, cx));
        let body = cx.new(|cx| InputState::new(window, cx).multi_line(true));
        let subscriptions = vec![cx.subscribe(&body, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.changed(cx);
            }
        })];
        Self {
            library: Library::default(),
            title_edit: None,
            dialog_focus: cx.focus_handle(),
            font_settings_open: false,
            markdown_mode: MarkdownMode::Split,
            image_store: Arc::new(RwLock::new(NoteImages::default())),
            delete_note: None,
            title,
            body,
            root: None,
            sender: None,
            revision: 0,
            pending_images: Vec::new(),
            status: String::new(),
            failed: false,
            moving: false,
            move_revision: None,
            loaded: false,
            _subscriptions: subscriptions,
            colors: crate::theme::find("default").colors(),
        }
    }
    pub fn initialize(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.loaded {
            return;
        }
        let result = (|| -> Result<Option<(PathBuf, Library)>> {
            let Some(root) = location()? else {
                return Ok(None);
            };
            let file = root.join("notes.json");
            let library: Library = if file.try_exists()? {
                serde_json::from_slice(&fs::read(file)?)?
            } else {
                Library::default()
            };
            if library
                .notes
                .iter()
                .any(|n| n.images.iter().any(|image| !image_name(image)))
            {
                bail!("Invalid notes image path");
            }
            fs::create_dir_all(&root)?;
            Ok(Some((root, library)))
        })();
        match result {
            Ok(Some((root, mut library))) => {
                library.active = library.active.min(library.notes.len().saturating_sub(1));
                let upgrade_images = !library.inline_images;
                if upgrade_images {
                    for note in &mut library.notes {
                        for name in &note.images {
                            let marker = format!("(notes-images/{name})");
                            if !note.body.contains(&marker) {
                                note.body.push_str(&format!("\n\n![image]{marker}\n"));
                            }
                        }
                    }
                    library.inline_images = true;
                }
                if let Ok(mut images) = self.image_store.write() {
                    images.root = root.clone();
                }
                let image_client = Arc::new(NoteImageClient {
                    fallback: cx.http_client(),
                    images: self.image_store.clone(),
                    executor: cx.background_executor().clone(),
                });
                cx.set_http_client(image_client);
                library.font_size = library.font_size.clamp(10, 36);
                self.library = library;
                self.root = Some(root.clone());
                self.loaded = true;
                let (tx, rx) = mpsc::channel();
                let (events, receiver) = async_channel::unbounded();
                cx.set_global(Writer(tx.clone()));
                self.sender = Some(tx);
                std::thread::spawn(move || {
                    let mut root = root;
                    while let Ok(command) = rx.recv() {
                        let (revision, result) = match command {
                            Command::Save(revision, library, images) => {
                                (revision, save(&root, &library, images))
                            }
                            Command::Move(revision, target) => {
                                let result = migrate(&mut root, &target);
                                (revision, result)
                            }
                            Command::Flush(done) => {
                                let _ = done.send(());
                                continue;
                            }
                        };
                        let _ = events.send_blocking((
                            revision,
                            root.clone(),
                            result.map_err(|e| format!("{e:#}")),
                        ));
                    }
                });
                cx.spawn(async move |this, cx| {
                    while let Ok((revision, root, result)) = receiver.recv().await {
                        if this
                            .update(cx, |this, cx| {
                                if let Ok(mut images) = this.image_store.write() {
                                    images.root = root.clone();
                                }
                                this.root = Some(root);
                                match result {
                                    Ok(()) => {
                                        if revision == this.revision {
                                            this.pending_images.clear();
                                            if let Ok(mut images) = this.image_store.write() {
                                                images.pending.clear();
                                            }
                                            this.failed = false;
                                            this.status = tr("已保存").into();
                                        }
                                    }
                                    Err(error) => {
                                        this.failed = true;
                                        this.status = error;
                                    }
                                }
                                if this.move_revision == Some(revision) {
                                    this.moving = false;
                                    this.move_revision = None;
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
                self.switch(self.library.active, window, cx);
                if upgrade_images {
                    self.persist(Vec::new(), cx);
                }
            }
            Ok(None) => {}
            Err(error) => {
                self.loaded = true;
                self.failed = true;
                self.status = format!("{error:#}");
            }
        }
    }
    fn persist(&mut self, images: Vec<(String, Vec<u8>)>, cx: &mut Context<Self>) {
        self.pending_images.extend(images);
        if let Some(sender) = &self.sender {
            self.revision += 1;
            self.status = tr("正在保存…").into();
            if sender
                .send(Command::Save(
                    self.revision,
                    self.library.clone(),
                    self.pending_images.clone(),
                ))
                .is_err()
            {
                self.failed = true;
                self.status = tr("笔记保存失败").into();
            }
        }
        cx.notify();
    }
    fn set_font_size(&mut self, size: u16, cx: &mut Context<Self>) {
        self.library.font_size = size.clamp(10, 36);
        self.persist(Vec::new(), cx);
    }
    fn changed(&mut self, cx: &mut Context<Self>) {
        if let Some(note) = self.library.notes.get_mut(self.library.active) {
            note.body = self.body.read(cx).value().to_string();
            self.persist(Vec::new(), cx);
        }
    }
    fn switch(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(note) = self.library.notes.get(index).cloned() {
            self.library.active = index;
            self.title
                .update(cx, |input, cx| input.set_value(note.title, window, cx));
            self.body
                .update(cx, |input, cx| input.set_value(note.body, window, cx));
        }
        cx.notify();
    }
    fn edit_title(&mut self, create: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (id, title) = if create {
            (None, tr("新笔记").to_owned())
        } else if let Some(note) = self.library.notes.get(self.library.active) {
            (Some(note.id.clone()), note.title.clone())
        } else {
            return;
        };
        self.title_edit = Some(id);
        self.title
            .update(cx, |input, cx| input.set_value(title, window, cx));
        self.title.read(cx).focus_handle(cx).focus(window);
        cx.notify();
    }
    fn finish_title(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().trim().to_owned();
        if title.is_empty() {
            return;
        }
        let Some(edit) = self.title_edit.take() else {
            return;
        };
        if let Some(id) = edit {
            if let Some(note) = self.library.notes.iter_mut().find(|n| n.id == id) {
                note.title = title;
            }
        } else {
            self.library.notes.push(Note {
                id: unique(),
                title,
                ..Default::default()
            });
            self.switch(self.library.notes.len() - 1, window, cx);
        }
        self.persist(Vec::new(), cx);
        self.body.read(cx).focus_handle(cx).focus(window);
    }
    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.delete_note.take() else {
            return;
        };
        if let Some(index) = self.library.notes.iter().position(|n| n.id == id) {
            let removed = self.library.notes.remove(index);
            self.pending_images
                .retain(|(name, _)| !removed.images.contains(name));
            let active = if index < self.library.active {
                self.library.active - 1
            } else {
                self.library.active
            };
            self.library.active = active.min(self.library.notes.len().saturating_sub(1));
            self.switch(self.library.active, window, cx);
            self.persist(Vec::new(), cx);
        }
        cx.notify();
    }
    fn note_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let colors = self.colors;
        let deleting = self.delete_note.is_some();
        let name = self
            .delete_note
            .as_ref()
            .and_then(|id| self.library.notes.iter().find(|n| &n.id == id))
            .map(|n| n.title.clone())
            .unwrap_or_default();
        div()
            .absolute()
            .inset_0()
            .size_full()
            .bg(rgba(0x18251c66))
            .track_focus(&self.dialog_focus)
            .flex()
            .items_center()
            .justify_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .capture_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "enter" && !deleting {
                    this.finish_title(window, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                }
            }))
            .child(
                div()
                    .w(px(420.))
                    .p_6()
                    .rounded(px(14.))
                    .bg(rgb(colors.bg))
                    .shadow_lg()
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
                                    .text_size(px(19.))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(if deleting {
                                        tr("删除笔记")
                                    } else if self.title_edit == Some(None) {
                                        tr("新建笔记")
                                    } else {
                                        tr("修改笔记名称")
                                    }),
                            )
                            .child(
                                Button::new("close-note-dialog")
                                    .ghost()
                                    .icon(IconName::Close)
                                    .tooltip(tr("取消"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.title_edit = None;
                                        this.delete_note = None;
                                        this.body.read(cx).focus_handle(cx).focus(window);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .when(deleting, |s| {
                        s.child(name)
                            .child(tr("确定删除这条笔记吗？此操作无法撤销。"))
                    })
                    .when(!deleting, |s| s.child(Input::new(&self.title).h(px(40.))))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-note-dialog")
                                    .ghost()
                                    .label(tr("取消"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.title_edit = None;
                                        this.delete_note = None;
                                        this.body.read(cx).focus_handle(cx).focus(window);
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("confirm-note-dialog")
                                    .icon(if deleting {
                                        crate::assets::ActionIcon::Trash
                                    } else {
                                        crate::assets::ActionIcon::Check
                                    })
                                    .tooltip(if deleting {
                                        tr("确认删除")
                                    } else {
                                        tr("保存")
                                    })
                                    .when(deleting, |s| {
                                        s.label(tr("确认删除"))
                                            .bg(rgb(0xc94b4b))
                                            .text_color(rgb(0xffffff))
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        if deleting {
                                            this.confirm_delete(window, cx);
                                        } else {
                                            this.finish_title(window, cx);
                                        }
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
    fn paste_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.library.notes.is_empty() || self.sender.is_none() {
            return false;
        }
        let Some(item) = cx.read_from_clipboard() else {
            return false;
        };
        let images: Vec<_> = item
            .into_entries()
            .filter_map(|entry| {
                if let ClipboardEntry::Image(image) = entry {
                    let extension = match image.format() {
                        ImageFormat::Svg => "svg",
                        ImageFormat::Jpeg => "jpg",
                        ImageFormat::Png => "png",
                        ImageFormat::Webp => "webp",
                        ImageFormat::Gif => "gif",
                        ImageFormat::Bmp => "bmp",
                        ImageFormat::Tiff => "tiff",
                    };
                    Some((
                        format!("{}.{}", unique(), extension),
                        image.bytes().to_vec(),
                    ))
                } else {
                    None
                }
            })
            .collect();
        if images.is_empty() {
            return false;
        }
        if let Ok(mut store) = self.image_store.write() {
            for (name, bytes) in &images {
                store.pending.insert(name.clone(), Arc::new(bytes.clone()));
            }
        }
        let markdown = images
            .iter()
            .map(|(name, _)| format!("![image](notes-images/{name})"))
            .collect::<Vec<_>>()
            .join("\n\n");
        self.body.update(cx, |input, cx| {
            input.replace(format!("\n\n{markdown}\n\n"), window, cx);
            cx.notify();
        });
        self.library.notes[self.library.active].body = self.body.read(cx).value().to_string();
        self.library.notes[self.library.active]
            .images
            .extend(images.iter().map(|(name, _)| name.clone()));
        self.persist(images, cx);
        true
    }
    fn choose(&mut self, cx: &mut Context<Self>) {
        if self.sender.is_none() || self.moving {
            return;
        }
        let picker = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(tr("选择笔记目录").into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picker.await {
                if let Some(target) = paths.first() {
                    let _ = this.update(cx, |this, cx| {
                        this.revision += 1;
                        this.moving = true;
                        this.move_revision = Some(this.revision);
                        this.status = tr("正在迁移笔记…").into();
                        if this
                            .sender
                            .as_ref()
                            .unwrap()
                            .send(Command::Move(this.revision, target.clone()))
                            .is_err()
                        {
                            this.moving = false;
                            this.failed = true;
                            this.status = tr("笔记保存失败").into();
                        }
                        cx.notify();
                    });
                }
            }
        })
        .detach();
    }
}
pub fn settings<T: 'static>(notes: Entity<Notes>, cx: &mut Context<T>) -> AnyElement {
    let view = notes.read(cx);
    let path = view
        .root
        .as_ref()
        .map(|p| crate::storage::display_path(p))
        .unwrap_or_else(|| tr("默认保存在数据目录下的 notes 文件夹").into());
    let status = view.status.clone();
    let available = view.sender.is_some() && !view.moving;
    div()
        .flex()
        .flex_col()
        .gap_4()
        .child(path)
        .child(status)
        .child(tr("更换目录会迁移笔记和图片，成功后删除旧文件。"))
        .child(
            Button::new("notes-location")
                .label(tr("更换笔记目录"))
                .disabled(!available)
                .on_click(move |_, _, cx| notes.update(cx, |notes, cx| notes.choose(cx))),
        )
        .into_any_element()
}
impl Render for Notes {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.initialize(window, cx);
        let colors = self.colors;
        // Theme colors are supplied through the same palette as the workbench.
        let mut tabs = div()
            .id("note-tabs")
            .flex()
            .gap_2()
            .overflow_x_scroll()
            .pb_3();
        for (index, note) in self.library.notes.iter().enumerate() {
            let title = if note.title.trim().is_empty() {
                tr("新笔记").to_owned()
            } else {
                note.title.chars().take(30).collect()
            };
            tabs = tabs.child(
                Button::new(("note-tab", index))
                    .label(title)
                    .h(px(40.))
                    .when(index == self.library.active, |s| s.bg(rgb(colors.selected)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.switch(index, window, cx);
                        this.persist(Vec::new(), cx);
                    })),
            );
        }
        tabs = tabs.child(
            Button::new("new-note")
                .icon(IconName::Plus)
                .tooltip(tr("新建笔记"))
                .w(px(40.))
                .h(px(40.))
                .disabled(self.sender.is_none() || self.moving)
                .on_click(cx.listener(|this, _, window, cx| this.edit_title(true, window, cx))),
        );
        if !self.library.notes.is_empty() {
            tabs = tabs
                .child(
                    Button::new("rename-note")
                        .ghost()
                        .icon(crate::assets::ActionIcon::Edit)
                        .tooltip(tr("修改笔记名称"))
                        .w(px(40.))
                        .h(px(40.))
                        .disabled(self.moving)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.edit_title(false, window, cx)),
                        ),
                )
                .child(
                    Button::new("delete-note")
                        .ghost()
                        .icon(crate::assets::ActionIcon::Trash)
                        .tooltip(tr("删除笔记"))
                        .w(px(40.))
                        .h(px(40.))
                        .disabled(self.moving)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.dialog_focus.focus(window);
                            this.delete_note = this
                                .library
                                .notes
                                .get(this.library.active)
                                .map(|n| n.id.clone());
                            cx.notify();
                        })),
                );
        }
        tabs = tabs.child(
            Button::new("note-font-settings")
                .ghost()
                .icon(IconName::Settings2)
                .tooltip(tr("字号设置"))
                .w(px(40.))
                .h(px(40.))
                .when(self.font_settings_open, |s| s.bg(rgb(colors.selected)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.font_settings_open = !this.font_settings_open;
                    cx.notify();
                })),
        );
        for (index, (mode, glyph, label)) in [
            (
                MarkdownMode::Edit,
                IconName::SquareTerminal,
                tr("Markdown 编辑"),
            ),
            (MarkdownMode::Split, IconName::PanelLeft, tr("双栏实时预览")),
            (MarkdownMode::Preview, IconName::Eye, tr("Markdown 预览")),
        ]
        .into_iter()
        .enumerate()
        {
            tabs = tabs.child(
                Button::new(("note-markdown-mode", index))
                    .ghost()
                    .icon(glyph)
                    .tooltip(label)
                    .w(px(40.))
                    .h(px(40.))
                    .when(self.markdown_mode == mode, |s| s.bg(rgb(colors.selected)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.markdown_mode = mode;
                        if mode == MarkdownMode::Preview {
                            this.dialog_focus.focus(window);
                        } else {
                            this.body.read(cx).focus_handle(cx).focus(window);
                        }
                        cx.notify();
                    })),
            );
        }
        let markdown = self
            .library
            .notes
            .get(self.library.active)
            .map(|note| note.body.clone())
            .unwrap_or_default();
        let preview = div()
            .id("notes-markdown-preview")
            .flex_1()
            .min_w_0()
            .h_full()
            .text_size(px(self.library.font_size as f32))
            .child(
                TextView::markdown(
                    SharedString::from(format!(
                        "notes-markdown-{}",
                        self.library
                            .notes
                            .get(self.library.active)
                            .map(|note| note.id.as_str())
                            .unwrap_or("empty")
                    )),
                    markdown,
                    window,
                    cx,
                )
                .style(TextViewStyle {
                    heading_base_font_size: px(self.library.font_size as f32),
                    ..Default::default()
                })
                .selectable(true)
                .scrollable(true),
            );
        div()
            .relative()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .p_6()
            .pb_0()
            .gap_4()
            .bg(rgb(colors.bg))
            .capture_action(
                cx.listener(|this, _: &crate::ui::CloseOverlay, window, cx| {
                    if this.title_edit.is_some() || this.delete_note.is_some() {
                        this.title_edit = None;
                        this.delete_note = None;
                        this.body.read(cx).focus_handle(cx).focus(window);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Paste, window, cx| {
                    if this.title_edit.is_none()
                        && this.delete_note.is_none()
                        && this.markdown_mode != MarkdownMode::Preview
                        && this.paste_image(window, cx)
                    {
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(24.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(tr("笔记")),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(rgb(if self.failed { 0xc44b4b } else { colors.muted }))
                            .child(self.status.clone()),
                    ),
            )
            .child(tabs)
            .when(self.font_settings_open, |s| {
                s.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .flex_shrink_0()
                        .child(tr("字号"))
                        .child(
                            Button::new("note-font-smaller")
                                .ghost()
                                .icon(IconName::Minus)
                                .tooltip(tr("缩小字号"))
                                .disabled(self.sender.is_none() || self.library.font_size <= 10)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_font_size(this.library.font_size.saturating_sub(1), cx)
                                })),
                        )
                        .child(format!("{} px", self.library.font_size))
                        .child(
                            Button::new("note-font-larger")
                                .ghost()
                                .icon(IconName::Plus)
                                .tooltip(tr("放大字号"))
                                .disabled(self.sender.is_none() || self.library.font_size >= 36)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_font_size(this.library.font_size + 1, cx)
                                })),
                        )
                        .child(
                            Button::new("note-font-reset")
                                .ghost()
                                .label(tr("恢复默认"))
                                .disabled(self.sender.is_none())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_font_size(default_font_size(), cx)
                                })),
                        ),
                )
            })
            .when(!self.library.notes.is_empty(), |s| {
                s.child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .min_w_0()
                        .flex()
                        .gap_5()
                        .when(self.markdown_mode != MarkdownMode::Preview, |s| {
                            s.child(
                                div().flex_1().min_w_0().h_full().child(
                                    Input::new(&self.body)
                                        .appearance(false)
                                        .bordered(false)
                                        .focus_bordered(false)
                                        .text_size(px(self.library.font_size as f32))
                                        .pb_0()
                                        .h_full(),
                                ),
                            )
                        })
                        .when(self.markdown_mode != MarkdownMode::Edit, |s| {
                            s.child(preview)
                        }),
                )
            })
            .when(self.library.notes.is_empty(), |s| {
                s.child(
                    div()
                        .flex_1()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(tr("新建一条笔记，记录想法。")),
                )
            })
            .when(
                self.title_edit.is_some() || self.delete_note.is_some(),
                |s| s.child(self.note_dialog(cx)),
            )
    }
}
