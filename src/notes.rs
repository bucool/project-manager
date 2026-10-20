//! Local notes. All disk writes are ordered on a dedicated, idle-blocking worker.
use crate::i18n::tr;
use anyhow::{Context as _, Result, bail};
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, IconName,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
#[path = "notes_text.rs"]
mod text;
use serde::{Deserialize, Serialize};
use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    io::{Cursor, Write},
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// Keep source ranges so editing a rendered block only changes that block.
fn markdown_blocks(source: &str) -> Vec<std::ops::Range<usize>> {
    let mut blocks = Vec::new();
    if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        if let Some(children) = root.children() {
            for node in children {
                if let Some(position) = node.position() {
                    let range = position.start.offset..position.end.offset;
                    if range.end <= source.len()
                        && source.is_char_boundary(range.start)
                        && source.is_char_boundary(range.end)
                    {
                        blocks.push(range);
                    } else {
                        return vec![0..source.len()];
                    }
                }
            }
        }
    }
    if blocks.is_empty() && !source.is_empty() {
        blocks.push(0..source.len());
    }
    blocks
}
fn local_images(source: &str) -> Vec<(std::ops::Range<usize>, String)> {
    fn visit(node: &markdown::mdast::Node, out: &mut Vec<(std::ops::Range<usize>, String)>) {
        if let markdown::mdast::Node::Image(image) = node {
            if let (Some(name), Some(position)) =
                (image.url.strip_prefix("notes-images/"), node.position())
            {
                if image_name(name) {
                    out.push((position.start.offset..position.end.offset, name.to_owned()));
                }
            } else if let Some(position) = node.position() {
                if image.url.starts_with("https://") || image.url.starts_with("http://") {
                    out.push((
                        position.start.offset..position.end.offset,
                        image.url.clone(),
                    ));
                }
            }
        }
        if let Some(children) = node.children() {
            for child in children {
                visit(child, out);
            }
        }
    }
    let mut images = Vec::new();
    if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        visit(&root, &mut images);
    }
    images
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Note {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    images: Vec<String>,
    #[serde(default)]
    image_sizes: HashMap<String, ImageSize>,
}
#[derive(Clone, Copy, Serialize, Deserialize)]
struct ImageSize {
    width: f32,
    height: f32,
}
struct ImageResize {
    name: String,
    origin: Point<Pixels>,
    original: ImageSize,
    handle: usize,
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
    editing: Option<std::ops::Range<usize>>,
    document_scroll: ScrollHandle,
    document_focus: FocusHandle,
    text_selection: Option<(usize, usize)>,
    text_drag: Option<(Point<Pixels>, bool)>,
    text_layouts: RefCell<Vec<text::FragmentLayout>>,
    editor_bounds: RefCell<Option<Bounds<Pixels>>>,
    delete_note: Option<String>,
    title: Entity<InputState>,
    body: Entity<InputState>,
    image_width: Entity<InputState>,
    image_height: Entity<InputState>,
    selected_image: Option<(String, usize)>,
    image_resize: Option<ImageResize>,
    intrinsic_sizes: RefCell<HashMap<String, ImageSize>>,
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
        let body = cx.new(|cx| InputState::new(window, cx).auto_grow(1, 3000));
        let image_width = cx.new(|cx| InputState::new(window, cx));
        let image_height = cx.new(|cx| InputState::new(window, cx));
        cx.bind_keys([
            KeyBinding::new("ctrl-a", gpui_component::input::SelectAll, Some("Notes")),
            KeyBinding::new("ctrl-c", gpui_component::input::Copy, Some("Notes")),
            KeyBinding::new("ctrl-x", gpui_component::input::Cut, Some("Notes")),
            KeyBinding::new("ctrl-v", gpui_component::input::Paste, Some("Notes")),
        ]);
        let mut subscriptions =
            vec![
                cx.subscribe_in(
                    &body,
                    window,
                    |this, _, event: &InputEvent, window, cx| match event {
                        InputEvent::Change => this.changed(cx),
                        InputEvent::Blur
                            if !this.body.read(cx).focus_handle(cx).is_focused(window) =>
                        {
                            this.changed(cx);
                            this.editing = None;
                            cx.notify();
                        }
                        InputEvent::PressEnter { .. } => this.changed(cx),
                        _ => {}
                    },
                ),
            ];
        for (input, width) in [(&image_width, true), (&image_height, false)] {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                        this.apply_image_dimension(width, window, cx);
                    }
                },
            ));
        }
        Self {
            library: Library::default(),
            title_edit: None,
            dialog_focus: cx.focus_handle(),
            font_settings_open: false,
            editing: None,
            document_scroll: ScrollHandle::new(),
            document_focus: cx.focus_handle(),
            text_selection: None,
            text_drag: None,
            text_layouts: RefCell::new(Vec::new()),
            editor_bounds: RefCell::new(None),
            delete_note: None,
            title,
            body,
            image_width,
            image_height,
            selected_image: None,
            image_resize: None,
            intrinsic_sizes: RefCell::new(HashMap::new()),
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
        let Some(range) = self.editing.clone() else {
            return;
        };
        let value = self.body.read(cx).value().to_string();
        if let Some(note) = self.library.notes.get_mut(self.library.active) {
            if range.end <= note.body.len()
                && note.body.is_char_boundary(range.start)
                && note.body.is_char_boundary(range.end)
            {
                if note.body[range.clone()] == value {
                    return;
                }
                note.body.replace_range(range.clone(), &value);
                self.editing = Some(range.start..range.start + value.len());
                self.persist(Vec::new(), cx);
            }
        }
    }
    fn focus_editor(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            self.body.read(cx).focus_handle(cx).focus(window);
        } else {
            self.document_focus.focus(window);
        }
    }
    fn begin_edit(
        &mut self,
        range: std::ops::Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.finish_image_resize(cx);
        self.selected_image = None;
        self.text_selection = None;
        self.text_drag = None;
        let Some(note) = self.library.notes.get(self.library.active) else {
            return;
        };
        let Some(value) = note.body.get(range.clone()).map(str::to_owned) else {
            return;
        };
        self.editing = None;
        self.body
            .update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        self.editing = Some(range);
        let line = value.lines().count().saturating_sub(1) as u32;
        let column = value
            .lines()
            .last()
            .map(|line| line.chars().count())
            .unwrap_or(0) as u32;
        self.body.update(cx, |input, cx| {
            input.set_cursor_position(
                gpui_component::input::Position::new(line, column),
                window,
                cx,
            )
        });
        cx.notify();
    }
    fn switch(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_image_resize(cx);
        self.selected_image = None;
        self.text_selection = None;
        self.text_drag = None;
        self.editing = None;
        self.document_scroll.set_offset(point(px(0.), px(0.)));
        if let Some(note) = self.library.notes.get(index).cloned() {
            self.library.active = index;
            self.title
                .update(cx, |input, cx| input.set_value(note.title, window, cx));
            self.body
                .update(cx, |input, cx| input.set_value("", window, cx));
            if note.body.is_empty() {
                self.editing = Some(0..0);
            }
        }
        cx.notify();
    }
    fn natural_image_size(&self, name: &str) -> ImageSize {
        if name.starts_with("https://") || name.starts_with("http://") {
            return ImageSize {
                width: 640.,
                height: 480.,
            };
        }
        if let Some(size) = self.intrinsic_sizes.borrow().get(name) {
            return *size;
        }
        // Read image headers once, rather than decoding the image on every drag frame.
        let dimensions =
            if let Some((_, bytes)) = self.pending_images.iter().find(|(file, _)| file == name) {
                image::ImageReader::new(Cursor::new(bytes))
                    .with_guessed_format()
                    .ok()
                    .and_then(|reader| reader.into_dimensions().ok())
            } else {
                self.root.as_ref().and_then(|root| {
                    image::image_dimensions(root.join("notes-images").join(name)).ok()
                })
            };
        let (width, height) = dimensions.unwrap_or((640, 480));
        let size = ImageSize {
            width: width.max(1) as f32,
            height: height.max(1) as f32,
        };
        self.intrinsic_sizes
            .borrow_mut()
            .insert(name.to_owned(), size);
        size
    }
    fn image_size(&self, name: &str, window: &Window) -> ImageSize {
        let available = (f32::from(window.viewport_size().width) - 300.).max(100.);
        if let Some(size) = self
            .library
            .notes
            .get(self.library.active)
            .and_then(|note| note.image_sizes.get(name))
        {
            if size.width.is_finite()
                && size.height.is_finite()
                && size.width > 0.
                && size.height > 0.
            {
                let mut size = ImageSize {
                    width: size.width.clamp(16., 4096.),
                    height: size.height.clamp(16., 4096.),
                };
                let scale = (available / size.width).min(1.);
                size.width *= scale;
                size.height *= scale;
                return size;
            }
        }
        let natural = self.natural_image_size(name);
        let scale = (available / natural.width)
            .min(720. / natural.height)
            .min(1.);
        ImageSize {
            width: natural.width * scale,
            height: natural.height * scale,
        }
    }
    fn sync_image_fields(&mut self, size: ImageSize, window: &mut Window, cx: &mut Context<Self>) {
        self.image_width.update(cx, |input, cx| {
            input.set_value(format!("{:.0}", size.width), window, cx)
        });
        self.image_height.update(cx, |input, cx| {
            input.set_value(format!("{:.0}", size.height), window, cx)
        });
    }
    fn select_image(
        &mut self,
        name: String,
        insertion: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changed(cx);
        self.editing = None;
        self.text_selection = None;
        self.text_drag = None;
        self.finish_image_resize(cx);
        let size = self.image_size(&name, window);
        self.selected_image = Some((name, insertion));
        self.sync_image_fields(size, window, cx);
        self.document_focus.focus(window);
        cx.notify();
    }
    fn delete_selected_image(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some((name, end)) = self.selected_image.clone() else {
            return false;
        };
        let Some(note) = self.library.notes.get(self.library.active) else {
            return false;
        };
        let Some((range, _)) = local_images(&note.body)
            .into_iter()
            .find(|(range, file)| file == &name && range.end == end)
        else {
            return false;
        };
        self.image_resize = None;
        self.selected_image = None;
        self.text_selection = Some((range.start, range.end));
        // Remove this occurrence from the document. Cached files may be shared by other notes.
        self.replace_selection("", window, cx)
    }
    fn apply_image_dimension(&mut self, width: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some((name, _)) = self.selected_image.clone() else {
            return;
        };
        let input = if width {
            &self.image_width
        } else {
            &self.image_height
        };
        let Ok(value) = input.read(cx).value().trim().parse::<f32>() else {
            self.sync_image_fields(self.image_size(&name, window), window, cx);
            return;
        };
        if !value.is_finite() {
            self.sync_image_fields(self.image_size(&name, window), window, cx);
            return;
        }
        let mut size = self.image_size(&name, window);
        let dimension = if width {
            &mut size.width
        } else {
            &mut size.height
        };
        let maximum = if width {
            (f32::from(window.viewport_size().width) - 300.)
                .max(100.)
                .min(4096.)
        } else {
            4096.
        };
        let value = value.clamp(16., maximum);
        let changed = (*dimension - value).abs() > 0.01;
        *dimension = value;
        if changed {
            self.library.notes[self.library.active]
                .image_sizes
                .insert(name, size);
            self.persist(Vec::new(), cx);
        }
        self.sync_image_fields(size, window, cx);
    }
    fn resize_image(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = &self.image_resize else {
            return;
        };
        if !event.dragging() {
            self.finish_image_resize(cx);
            return;
        }
        let dx = f32::from(event.position.x - drag.origin.x);
        let dy = f32::from(event.position.y - drag.origin.y);
        let max_width = (f32::from(window.viewport_size().width) - 300.)
            .max(100.)
            .min(4096.);
        let mut size = drag.original;
        let x_sign = if [0, 6, 7].contains(&drag.handle) {
            -1.
        } else {
            1.
        };
        let y_sign = if [0, 1, 2].contains(&drag.handle) {
            -1.
        } else {
            1.
        };
        if drag.handle % 2 == 0 {
            let sx = dx * x_sign / size.width;
            let sy = dy * y_sign / size.height;
            let min_scale = (16. / size.width).max(16. / size.height);
            let max_scale = (max_width / size.width).min(4096. / size.height);
            let scale = (1. + if sx.abs() > sy.abs() { sx } else { sy })
                .clamp(min_scale.min(max_scale), max_scale);
            size.width *= scale;
            size.height *= scale;
        } else if drag.handle == 3 || drag.handle == 7 {
            size.width = (size.width + dx * x_sign).clamp(16., max_width);
        } else {
            size.height = (size.height + dy * y_sign).clamp(16., 4096.);
        }
        self.library.notes[self.library.active]
            .image_sizes
            .insert(drag.name.clone(), size);
        self.sync_image_fields(size, window, cx);
        cx.stop_propagation();
        cx.notify();
    }
    fn finish_image_resize(&mut self, cx: &mut Context<Self>) {
        if self.image_resize.take().is_some() {
            self.persist(Vec::new(), cx);
        }
    }
    fn image_controls(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .flex_shrink_0()
            .text_size(px(13.))
            .child(tr("宽"))
            .child(Input::new(&self.image_width).w(px(76.)).h(px(34.)))
            .child(tr("高"))
            .child(Input::new(&self.image_height).w(px(76.)).h(px(34.)))
            .child("px")
            .child(
                Button::new("reset-note-image")
                    .ghost()
                    .icon(crate::assets::ActionIcon::Refresh)
                    .tooltip(tr("恢复原始比例"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        if let Some((name, _)) = this.selected_image.clone() {
                            let natural = this.natural_image_size(&name);
                            let mut size = this.image_size(&name, window);
                            size.height = size.width * natural.height / natural.width;
                            let scale = (4096. / size.height).min(1.);
                            size.width *= scale;
                            size.height *= scale;
                            this.library.notes[this.library.active]
                                .image_sizes
                                .insert(name, size);
                            this.sync_image_fields(size, window, cx);
                            this.persist(Vec::new(), cx);
                        }
                    })),
            )
            .into_any_element()
    }
    fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        let (anchor, end) = self.text_selection?;
        (anchor != end).then_some(anchor.min(end)..anchor.max(end))
    }
    fn set_editor_cursor(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        let value = self.body.read(cx).value();
        let mut offset = offset.min(value.len());
        while !value.is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let prefix = &value[..offset];
        let line = prefix.bytes().filter(|byte| *byte == b'\n').count() as u32;
        let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() as u32;
        self.body.update(cx, |input, cx| {
            input.set_cursor_position(
                gpui_component::input::Position::new(line, column),
                window,
                cx,
            );
        });
    }
    fn start_text_selection(
        &mut self,
        offset: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.changed(cx);
        self.editing = None;
        self.selected_image = None;
        let anchor = if event.modifiers.shift {
            self.text_selection
                .map(|(anchor, _)| anchor)
                .unwrap_or(offset)
        } else {
            offset
        };
        self.text_selection = Some((anchor, offset));
        self.text_drag = Some((event.position, false));
        self.document_focus.focus(window);
        cx.notify();
    }
    fn move_text_selection(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((origin, dragged)) = self.text_drag else {
            return;
        };
        if !event.dragging() {
            return;
        }
        let moved = f32::from(event.position.x - origin.x).abs()
            + f32::from(event.position.y - origin.y).abs()
            > 4.;
        if !dragged && !moved {
            return;
        }
        if self.editing.is_some() {
            if self
                .editor_bounds
                .borrow()
                .is_some_and(|bounds| bounds.contains(&event.position))
            {
                return;
            }
            self.changed(cx);
            self.editing = None;
            self.document_focus.focus(window);
        }
        let endpoint = {
            let layouts = self.text_layouts.borrow();
            layouts
                .iter()
                .min_by(|a, b| {
                    let distance = |bounds: Bounds<Pixels>| {
                        let y = f32::from(event.position.y);
                        let top = f32::from(bounds.top());
                        let bottom = f32::from(bounds.bottom());
                        if y < top {
                            top - y
                        } else if y > bottom {
                            y - bottom
                        } else {
                            0.
                        }
                    };
                    distance(a.bounds()).total_cmp(&distance(b.bounds()))
                })
                .map(|layout| layout.source_at(event.position))
        };
        if let Some(endpoint) = endpoint {
            if let Some((anchor, _)) = self.text_selection {
                self.text_selection = Some((anchor, endpoint));
                self.text_drag = Some((origin, true));
                // Follow a drag beyond the visible document edge.
                let height = f32::from(window.viewport_size().height);
                let y = f32::from(event.position.y);
                let delta = if y > height - 36. {
                    -18.
                } else if y < 190. {
                    18.
                } else {
                    0.
                };
                if delta != 0. {
                    let offset = self.document_scroll.offset();
                    self.document_scroll
                        .set_offset(point(offset.x, (offset.y + px(delta)).min(px(0.))));
                }
                cx.notify();
            }
        }
    }
    fn select_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.changed(cx);
        self.editing = None;
        self.selected_image = None;
        self.text_drag = None;
        let end = self
            .library
            .notes
            .get(self.library.active)
            .map(|note| note.body.len())
            .unwrap_or(0);
        self.text_selection = Some((0, end));
        self.document_focus.focus(window);
        cx.notify();
    }
    fn document_has_focus(&self, window: &Window, cx: &App) -> bool {
        self.title_edit.is_none()
            && self.delete_note.is_none()
            && (self.document_focus.is_focused(window)
                || self.body.read(cx).focus_handle(cx).is_focused(window))
    }
    fn copy_selection(&self, cx: &mut Context<Self>) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let Some(note) = self.library.notes.get(self.library.active) else {
            return false;
        };
        let content = text::Content::parse(&note.body, 0);
        cx.write_to_clipboard(ClipboardItem::new_string(content.selected_text(&range)));
        true
    }
    fn replace_selection(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(range) = self.selection_range() else {
            return false;
        };
        let Some(note) = self.library.notes.get_mut(self.library.active) else {
            return false;
        };
        if note.body.get(range.clone()).is_none() {
            return false;
        }
        note.body.replace_range(range.clone(), value);
        let cursor = range.start + value.len();
        self.text_selection = None;
        self.text_drag = None;
        self.persist(Vec::new(), cx);
        let editing = markdown_blocks(&self.library.notes[self.library.active].body)
            .into_iter()
            .find(|range| range.start <= cursor && cursor <= range.end)
            .unwrap_or(cursor..cursor);
        self.begin_edit(editing.clone(), window, cx);
        self.set_editor_cursor(cursor.saturating_sub(editing.start), window, cx);
        true
    }
    fn render_text(&self, source: &str, base: usize, cx: &mut Context<Self>) -> AnyElement {
        let content = text::Content::parse(source, base);
        let font_size = self.library.font_size as f32;
        let size = match content.heading {
            Some(1) => font_size * 1.9,
            Some(2) => font_size * 1.5,
            Some(_) => font_size * 1.2,
            None => font_size,
        };
        div()
            .text_size(px(size))
            .line_height(relative(1.5))
            .when(content.code, |element| {
                element
                    .p_3()
                    .bg(rgb(self.colors.panel))
                    .rounded(px(6.))
                    .font_family("Consolas")
            })
            .child(text::SelectableText::new(
                SharedString::from(format!("note-text-{base}")),
                content,
                cx.entity(),
                self.selection_range(),
            ))
            .into_any_element()
    }
    fn rendered_block(
        &self,
        source: &str,
        start: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut block = div()
            .flex()
            .flex_col()
            .gap_2()
            .text_size(px(self.library.font_size as f32));
        let mut offset = 0;
        for (range, name) in local_images(source) {
            if range.start < offset
                || range.end > source.len()
                || !source.is_char_boundary(range.start)
                || !source.is_char_boundary(range.end)
            {
                continue;
            }
            if range.start > offset {
                block =
                    block.child(self.render_text(&source[offset..range.start], start + offset, cx));
            }
            let size = self.image_size(&name, window);
            let selected = self
                .selected_image
                .as_ref()
                .is_some_and(|(file, end)| file == &name && *end == start + range.end);
            let text_selected = self.selection_range().is_some_and(|selection| {
                selection.start < start + range.end && selection.end > start + range.start
            });
            let image: AnyElement = if let Some((_, bytes)) =
                self.pending_images.iter().find(|(file, _)| file == &name)
            {
                let format = match name.rsplit('.').next().unwrap_or("") {
                    "jpg" | "jpeg" => ImageFormat::Jpeg,
                    "webp" => ImageFormat::Webp,
                    "gif" => ImageFormat::Gif,
                    "bmp" => ImageFormat::Bmp,
                    "tiff" => ImageFormat::Tiff,
                    "svg" => ImageFormat::Svg,
                    _ => ImageFormat::Png,
                };
                img(Arc::new(Image::from_bytes(format, bytes.clone())))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            } else if name.starts_with("https://") || name.starts_with("http://") {
                img(SharedUri::from(name.clone()))
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            } else if let Some(path) = self
                .root
                .as_ref()
                .map(|root| root.join("notes-images").join(&name))
                .filter(|path| path.is_file())
            {
                img(path)
                    .w_full()
                    .h_full()
                    .object_fit(ObjectFit::Fill)
                    .into_any_element()
            } else {
                div()
                    .text_color(rgb(0xc44b4b))
                    .child(tr("图片文件不存在"))
                    .into_any_element()
            };
            let image_end = start + range.end;
            let insertion = start + range.end;
            let select_name = name.clone();
            let mut frame = div()
                .id(SharedString::from(format!("note-picture-{image_end}")))
                .relative()
                .flex_shrink_0()
                .w(px(size.width + 12.))
                .h(px(size.height + 12.))
                .p(px(5.))
                .border_1()
                .border_color(rgb(if selected || text_selected {
                    self.colors.accent
                } else {
                    self.colors.line
                }))
                .cursor(CursorStyle::PointingHand)
                .child(image)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.text_drag = None;
                    }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.text_drag.is_some_and(|(_, dragged)| dragged) {
                        return;
                    }
                    cx.stop_propagation();
                    // A resize must keep its selection when the mouse is released.
                    if this
                        .selected_image
                        .as_ref()
                        .is_none_or(|(file, end)| file != &select_name || *end != insertion)
                    {
                        this.select_image(select_name.clone(), insertion, window, cx);
                    }
                }));
            if selected {
                let positions = [
                    (0., 0.),
                    (0.5, 0.),
                    (1., 0.),
                    (1., 0.5),
                    (1., 1.),
                    (0.5, 1.),
                    (0., 1.),
                    (0., 0.5),
                ];
                for (handle, (x, y)) in positions.into_iter().enumerate() {
                    let drag_name = name.clone();
                    let cursor = match handle {
                        0 | 4 => CursorStyle::ResizeUpLeftDownRight,
                        2 | 6 => CursorStyle::ResizeUpRightDownLeft,
                        1 | 5 => CursorStyle::ResizeUpDown,
                        _ => CursorStyle::ResizeLeftRight,
                    };
                    frame = frame.child(
                        div()
                            .id((
                                SharedString::from(format!("image-handle-{image_end}")),
                                handle,
                            ))
                            .absolute()
                            .left(px(x * (size.width + 2.)))
                            .top(px(y * (size.height + 2.)))
                            .size(px(10.))
                            .border_1()
                            .border_color(rgb(self.colors.accent))
                            .bg(rgb(self.colors.surface))
                            .cursor(cursor)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    window.prevent_default();
                                    this.image_resize = Some(ImageResize {
                                        name: drag_name.clone(),
                                        origin: event.position,
                                        original: size,
                                        handle,
                                    });
                                }),
                            )
                            .on_click(|_, _, cx| cx.stop_propagation()),
                    );
                }
            }
            block = block.child(frame);
            offset = range.end;
        }
        if offset < source.len() {
            block = block.child(self.render_text(&source[offset..], start + offset, cx));
        }
        block.into_any_element()
    }
    fn blank_lines(
        &self,
        source: &str,
        range: std::ops::Range<usize>,
        skip_separator: bool,
        trailing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut lines = div().flex().flex_col().flex_shrink_0();
        let whitespace = source.get(range.clone()).unwrap_or("");
        for (index, _) in whitespace
            .char_indices()
            .filter(|(_, ch)| *ch == '\n')
            .skip(usize::from(skip_separator))
        {
            let offset = range.start + index + usize::from(trailing);
            lines = lines.child(
                div()
                    .id(SharedString::from(format!("note-blank-{offset}")))
                    .h(px(self.library.font_size as f32 * 1.5))
                    .flex_shrink_0()
                    .cursor_text()
                    .when(
                        self.selection_range()
                            .is_some_and(|range| range.contains(&offset)),
                        |line| line.bg(rgb(self.colors.selected)),
                    )
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, _| {
                            this.text_drag = None;
                        }),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if !this.text_drag.is_some_and(|(_, dragged)| dragged) {
                            this.begin_edit(offset..offset, window, cx);
                        }
                    })),
            );
        }
        lines.into_any_element()
    }
    fn document(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.text_layouts.borrow_mut().clear();
        *self.editor_bounds.borrow_mut() = None;
        let source = self
            .library
            .notes
            .get(self.library.active)
            .map(|note| note.body.as_str())
            .unwrap_or("");
        let mut ranges = Vec::new();
        let mut inserted = false;
        for range in markdown_blocks(source) {
            if let Some(editing) = &self.editing {
                if !inserted && range.start >= editing.start {
                    ranges.push((editing.clone(), true));
                    inserted = true;
                }
                if range.start < editing.end && range.end > editing.start {
                    continue;
                }
            }
            ranges.push((range, false));
        }
        if let Some(editing) = &self.editing {
            if !inserted {
                ranges.push((editing.clone(), true));
            }
        }
        let mut document = div()
            .id("note-document-scroll")
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.document_scroll)
            .track_focus(&self.document_focus)
            .flex()
            .flex_col();
        let mut previous_end = 0;
        let mut has_block = false;
        for (range, active) in ranges {
            if range.start > previous_end {
                document = document.child(self.blank_lines(
                    source,
                    previous_end..range.start,
                    has_block,
                    false,
                    cx,
                ));
            }
            previous_end = previous_end.max(range.end);
            has_block = true;
            if active {
                let owner = cx.entity();
                document = document.child(
                    div()
                        .relative()
                        .flex_shrink_0()
                        .child(
                            Input::new(&self.body)
                                .appearance(false)
                                .bordered(false)
                                .focus_bordered(false)
                                .text_size(px(self.library.font_size as f32))
                                .p_0(),
                        )
                        .child(
                            canvas(
                                |_, _, _| (),
                                move |bounds, _, _, cx| {
                                    *owner.read(cx).editor_bounds.borrow_mut() = Some(bounds);
                                },
                            )
                            .absolute()
                            .inset_0(),
                        )
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, event: &MouseDownEvent, window, cx| {
                                let position = event.position;
                                cx.defer_in(window, move |this, _, cx| {
                                    if let Some(range) = &this.editing {
                                        let anchor = range.start + this.body.read(cx).cursor();
                                        this.text_selection = Some((anchor, anchor));
                                        this.text_drag = Some((position, false));
                                    }
                                });
                            }),
                        ),
                );
            } else if let Some(value) = source.get(range.clone()) {
                if value.trim().is_empty() {
                    document = document.child(
                        div()
                            .id(SharedString::from(format!(
                                "note-whitespace-{}",
                                range.start
                            )))
                            .h(px(
                                (value.bytes().filter(|byte| *byte == b'\n').count() + 1) as f32
                                    * self.library.font_size as f32
                                    * 1.5,
                            ))
                            .flex_shrink_0()
                            .cursor_text()
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.begin_edit(range.clone(), window, cx);
                            })),
                    );
                    continue;
                }
                let content = self.rendered_block(value, range.start, window, cx);
                document = document.child(
                    div()
                        .id(SharedString::from(format!("note-block-{}", range.start)))
                        .flex_shrink_0()
                        .min_h(px(self.library.font_size as f32 * 1.5))
                        .cursor_text()
                        .child(content)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if this.text_drag.is_some_and(|(_, dragged)| dragged)
                                || this.selection_range().is_some()
                            {
                                return;
                            }
                            let offset =
                                this.text_selection.map(|(_, end)| end).unwrap_or(range.end);
                            this.begin_edit(range.clone(), window, cx);
                            this.set_editor_cursor(offset.saturating_sub(range.start), window, cx);
                        })),
                );
            }
        }
        if previous_end < source.len() {
            document = document.child(self.blank_lines(
                source,
                previous_end..source.len(),
                false,
                true,
                cx,
            ));
        }
        let end = source.len();
        document = document.child(
            div()
                .id("note-document-end")
                .min_h(px(1.))
                .flex_1()
                .cursor_text()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, _| {
                        this.text_drag = None;
                    }),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if this.text_drag.is_some_and(|(_, dragged)| dragged) {
                        return;
                    }
                    let mut end = end;
                    if let Some(note) = this.library.notes.get_mut(this.library.active) {
                        if !note.body.is_empty() && !note.body.ends_with("\n\n") {
                            note.body.push_str("\n\n");
                            end = note.body.len();
                            this.persist(Vec::new(), cx);
                        }
                    }
                    this.begin_edit(end..end, window, cx);
                })),
        );
        div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(document)
            .child(gpui_component::scroll::Scrollbar::vertical(
                &self.document_scroll,
            ))
            .into_any_element()
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
        self.focus_editor(window, cx);
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
                                        this.focus_editor(window, cx);
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
                                        this.focus_editor(window, cx);
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
        if let Some(range) = self.selection_range() {
            self.library.notes[self.library.active]
                .body
                .replace_range(range.clone(), "");
            self.begin_edit(range.start..range.start, window, cx);
        }
        if self.editing.is_none() {
            let end = self
                .selected_image
                .as_ref()
                .map(|(_, position)| *position)
                .unwrap_or(self.library.notes[self.library.active].body.len());
            self.begin_edit(end..end, window, cx);
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
        self.changed(cx);
        self.editing = None;
        self.document_focus.focus(window);
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
            .flex_1()
            .min_w_0()
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
        div()
            .relative()
            .key_context("Notes")
            .size_full()
            .min_w_0()
            .flex()
            .flex_col()
            .p_6()
            .pb_0()
            .gap_4()
            .bg(rgb(colors.bg))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                this.resize_image(event, window, cx);
                this.move_text_selection(event, window, cx);
            }))
            .capture_action(cx.listener(
                |this, _: &gpui_component::input::SelectAll, window, cx| {
                    if this.document_has_focus(window, cx) {
                        this.select_document(window, cx);
                        cx.stop_propagation();
                    }
                },
            ))
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Copy, window, cx| {
                    if this.document_has_focus(window, cx) && this.copy_selection(cx) {
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Cut, window, cx| {
                    if this.document_has_focus(window, cx) && this.copy_selection(cx) {
                        this.replace_selection("", window, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if this.document_focus.is_focused(window)
                    && (event.keystroke.key == "backspace" || event.keystroke.key == "delete")
                    && this.delete_selected_image(window, cx)
                {
                    cx.stop_propagation();
                    window.prevent_default();
                    return;
                }
                if this.document_focus.is_focused(window) && this.selection_range().is_some() {
                    if event.keystroke.key == "backspace" || event.keystroke.key == "delete" {
                        this.replace_selection("", window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if event.keystroke.key == "enter" {
                        this.replace_selection("\n", window, cx);
                        cx.stop_propagation();
                        window.prevent_default();
                    } else if !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.platform
                        && !event.keystroke.modifiers.alt
                    {
                        if let Some(value) = &event.keystroke.key_char {
                            this.replace_selection(value, window, cx);
                            cx.stop_propagation();
                            window.prevent_default();
                        }
                    }
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.finish_image_resize(cx);
                }),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.finish_image_resize(cx);
                }),
            )
            .capture_action(
                cx.listener(|this, _: &crate::ui::CloseOverlay, window, cx| {
                    if this.title_edit.is_some() || this.delete_note.is_some() {
                        this.title_edit = None;
                        this.delete_note = None;
                        this.focus_editor(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }
                }),
            )
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Paste, window, cx| {
                    if this.title_edit.is_none()
                        && this.delete_note.is_none()
                        && this.document_has_focus(window, cx)
                    {
                        if this.paste_image(window, cx) {
                            cx.stop_propagation();
                        } else if let Some(value) =
                            cx.read_from_clipboard().and_then(|item| item.text())
                        {
                            if this.replace_selection(&value, window, cx) {
                                cx.stop_propagation();
                            }
                        }
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .flex_shrink_0()
                    .child(tabs)
                    .when(self.selected_image.is_some(), |row| {
                        row.child(self.image_controls(cx))
                    }),
            )
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
                s.child(self.document(window, cx))
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
