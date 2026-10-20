//! Local notes. All disk writes are ordered on a dedicated, idle-blocking worker.
use crate::i18n::tr;
use anyhow::{Context as _, Result, bail};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Default, Serialize, Deserialize)]
struct Note {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    images: Vec<String>,
}
#[derive(Clone, Default, Serialize, Deserialize)]
struct Library {
    notes: Vec<Note>,
    active: usize,
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
        let subscriptions = [&title, &body]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.changed(cx);
                    }
                })
            })
            .collect();
        Self {
            library: Library::default(),
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
                                this.root = Some(root);
                                match result {
                                    Ok(()) => {
                                        if revision == this.revision {
                                            this.pending_images.clear();
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
    fn changed(&mut self, cx: &mut Context<Self>) {
        if let Some(note) = self.library.notes.get_mut(self.library.active) {
            note.title = self.title.read(cx).value().to_string();
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
    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.library.notes.push(Note {
            id: unique(),
            title: tr("新笔记").into(),
            ..Default::default()
        });
        self.switch(self.library.notes.len() - 1, window, cx);
        self.persist(Vec::new(), cx);
        self.title.read(cx).focus_handle(cx).focus(window);
    }
    fn paste_image(&mut self, cx: &mut Context<Self>) -> bool {
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
                .label(tr("新建笔记"))
                .disabled(self.sender.is_none() || self.moving)
                .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
        );
        let mut images = div()
            .id("note-images")
            .flex()
            .flex_wrap()
            .gap_3()
            .overflow_y_scroll()
            .max_h(px(230.));
        if let (Some(root), Some(note)) = (&self.root, self.library.notes.get(self.library.active))
        {
            for (index, name) in note.images.iter().enumerate() {
                let path = root.join("notes-images").join(name);
                if !path.is_file() {
                    continue;
                }
                images = images.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(
                            img(path)
                                .w(px(210.))
                                .h(px(150.))
                                .object_fit(ObjectFit::Contain),
                        )
                        .child(
                            Button::new(("remove-note-image", index))
                                .ghost()
                                .label(tr("移除图片"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.library.notes[this.library.active].images.remove(index);
                                    this.persist(Vec::new(), cx);
                                })),
                        ),
                );
            }
        }
        div()
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .p_6()
            .gap_4()
            .bg(rgb(colors.bg))
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Paste, _, cx| {
                    if this.paste_image(cx) {
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
            .when(!self.library.notes.is_empty(), |s| {
                s.child(Input::new(&self.title).h(px(44.)))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .child(Input::new(&self.body).h_full()),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                Button::new("paste-note-image")
                                    .label(tr("粘贴图片"))
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        if !this.paste_image(cx) {
                                            this.status = tr("剪贴板中没有图片").into();
                                            cx.notify();
                                        }
                                    })),
                            )
                            .child(tr("Ctrl+V 粘贴图片，文字修改自动保存")),
                    )
                    .child(images)
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
    }
}
