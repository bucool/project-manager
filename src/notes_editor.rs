//! One native input handler, source buffer, selection and undo history per note.
//! Rendered Markdown blocks are layout only; they never own editable text.
use super::*;
use crate::smooth_scroll::SmoothScroll;
use std::{ops::Range, time::Instant};
use unicode_segmentation::UnicodeSegmentation;
actions!(
    note_editor,
    [ExtendLeft, ExtendRight, ExtendUp, ExtendDown, InsertTab]
);

#[derive(Clone)]
struct Revision {
    body: String,
    sizes: HashMap<String, ImageSize>,
    selection: (usize, usize),
}
#[derive(Default)]
struct History {
    undo: Vec<Revision>,
    redo: Vec<Revision>,
    group: Option<(Instant, usize, bool)>,
}
pub(super) struct EditorState {
    pub scroll: SmoothScroll,
    histories: HashMap<String, History>,
    pub marked: Option<Range<usize>>,
    pub goal_x: Option<Pixels>,
    pub viewport: RefCell<Option<Bounds<Pixels>>>,
    pub image_bounds: RefCell<HashMap<usize, (Range<usize>, Bounds<Pixels>)>>,
    pub content: RefCell<HashMap<usize, (String, text::Content)>>,
    pub pending_sources: RefCell<HashMap<String, Arc<Image>>>,
    blocks: RefCell<(String, Vec<Range<usize>>)>,
    pub blink_on: bool,
    pub reveal: bool,
    blink_task: Option<Task<()>>,
    pub save_task: Option<Task<()>>,
    pub save_pending: bool,
    pub selections: HashMap<String, (usize, usize)>,
}
impl Default for EditorState {
    fn default() -> Self {
        Self {
            scroll: SmoothScroll::default(),
            histories: HashMap::new(),
            marked: None,
            goal_x: None,
            viewport: RefCell::new(None),
            image_bounds: RefCell::new(HashMap::new()),
            content: RefCell::new(HashMap::new()),
            pending_sources: RefCell::new(HashMap::new()),
            blocks: RefCell::new(Default::default()),
            blink_on: true,
            reveal: false,
            blink_task: None,
            save_task: None,
            save_pending: false,
            selections: HashMap::new(),
        }
    }
}

impl Notes {
    pub(super) fn source(&self) -> &str {
        self.library
            .notes
            .get(self.library.active)
            .map(|note| note.body.as_str())
            .unwrap_or("")
    }
    pub(super) fn caret(&self) -> usize {
        self.text_selection
            .map(|(_, end)| end)
            .unwrap_or(0)
            .min(self.source().len())
    }
    pub(super) fn selection(&self) -> Range<usize> {
        let (anchor, end) = self.text_selection.unwrap_or((0, 0));
        anchor.min(end).min(self.source().len())..anchor.max(end).min(self.source().len())
    }
    pub(super) fn selection_range(&self) -> Option<Range<usize>> {
        let range = self.selection();
        (!range.is_empty()).then_some(range)
    }
    pub(super) fn editor_reset(&mut self) {
        self.editor.scroll.cancel();
        self.text_selection = Some((0, 0));
        self.text_drag = None;
        self.selected_image = None;
        self.editor.marked = None;
        self.editor.goal_x = None;
        self.editor.reveal = false;
        self.editor.content.borrow_mut().clear();
        self.text_layouts.borrow_mut().clear();
        self.editor.image_bounds.borrow_mut().clear();
    }
    pub(super) fn wake_caret(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.editor.scroll.cancel();
        self.editor.blink_on = true;
        self.editor.reveal = true;
        if self.editor.blink_task.is_none() {
            self.editor.blink_task = Some(cx.spawn_in(window, async move |this, cx| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(530))
                        .await;
                    let active = this
                        .update_in(cx, |this, window, cx| {
                            if !this.document_focus.is_focused(window) {
                                this.editor.blink_task = None;
                                return false;
                            }
                            this.editor.blink_on = !this.editor.blink_on;
                            cx.notify();
                            true
                        })
                        .unwrap_or(false);
                    if !active {
                        break;
                    }
                }
            }));
        }
        cx.notify();
    }
    fn current_revision(&self) -> Revision {
        let note = &self.library.notes[self.library.active];
        Revision {
            body: note.body.clone(),
            sizes: note.image_sizes.clone(),
            selection: self.text_selection.unwrap_or((0, 0)),
        }
    }
    pub(super) fn checkpoint(&mut self, typing: bool) {
        if self.library.notes.is_empty() {
            return;
        }
        let note_id = self.library.notes[self.library.active].id.clone();
        let caret = self.caret();
        let coalesce = typing && self.selection().is_empty();
        let same_group = coalesce
            && self
                .editor
                .histories
                .get(&note_id)
                .and_then(|history| history.group)
                .is_some_and(|(time, end, insert)| {
                    insert && end == caret && time.elapsed() < Duration::from_millis(700)
                });
        let revision = if same_group {
            None
        } else {
            Some(self.current_revision())
        };
        let history = self.editor.histories.entry(note_id).or_default();
        if let Some(revision) = revision {
            history.undo.push(revision);
            while history.undo.len() > 100
                || (history.undo.len() > 1
                    && history.undo.iter().map(|r| r.body.len()).sum::<usize>() > 8 * 1024 * 1024)
            {
                history.undo.remove(0);
            }
        }
        history.redo.clear();
        history.group = None;
    }
    pub(super) fn break_edit_group(&mut self) {
        if let Some(note) = self.library.notes.get(self.library.active) {
            if let Some(history) = self.editor.histories.get_mut(&note.id) {
                history.group = None;
            }
        }
    }
    pub(super) fn editor_changed(&mut self, typing: bool, cx: &mut Context<Self>) {
        self.editor.content.borrow_mut().clear();
        self.text_layouts.borrow_mut().clear();
        self.editor.image_bounds.borrow_mut().clear();
        self.editor.goal_x = None;
        self.selected_image = None;
        self.text_drag = None;
        if typing {
            let caret = self.caret();
            let id = self.library.notes[self.library.active].id.clone();
            self.editor.histories.entry(id).or_default().group =
                Some((Instant::now(), caret, true));
        }
        self.queue_editor_save(cx);
    }
    pub(super) fn queue_editor_save(&mut self, cx: &mut Context<Self>) {
        self.editor.save_pending = true;
        self.status = tr("正在保存…").into();
        self.editor.save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let _ = this.update(cx, |this, cx| this.persist(Vec::new(), cx));
        }));
        cx.notify();
    }
    pub(super) fn edit_range(
        &mut self,
        range: Range<usize>,
        value: &str,
        typing: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.library.notes.is_empty() || self.source().get(range.clone()).is_none() {
            return;
        }
        let range = if value.is_empty() && !range.is_empty() {
            empty_inline_span(self.source(), range)
        } else {
            range
        };
        self.checkpoint(typing);
        let value = value.replace("\r\n", "\n").replace('\r', "\n");
        self.library.notes[self.library.active]
            .body
            .replace_range(range.clone(), &value);
        let caret = range.start + value.len();
        self.text_selection = Some((caret, caret));
        self.editor.marked = None;
        self.editor_changed(typing, cx);
        self.wake_caret(window, cx);
    }
    pub(super) fn replace_selection(
        &mut self,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.edit_range(self.selection(), value, false, window, cx);
        true
    }
    pub(super) fn undo_editor(&mut self, redo: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.library.notes.is_empty() {
            return;
        }
        let current = self.current_revision();
        let id = self.library.notes[self.library.active].id.clone();
        let history = self.editor.histories.entry(id).or_default();
        let previous = if redo {
            history.redo.pop()
        } else {
            history.undo.pop()
        };
        if let Some(previous) = previous {
            if redo {
                history.undo.push(current);
            } else {
                history.redo.push(current);
            }
            history.group = None;
            let note = &mut self.library.notes[self.library.active];
            note.body = previous.body;
            note.image_sizes = previous.sizes;
            self.text_selection = Some(previous.selection);
            self.editor.marked = None;
            self.editor_changed(false, cx);
            self.wake_caret(window, cx);
        }
    }
    pub(super) fn select_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.break_edit_group();
        self.text_selection = Some((0, self.source().len()));
        self.selected_image = None;
        self.document_focus.focus(window);
        self.wake_caret(window, cx);
    }
    pub(super) fn copy_selection(&self, cx: &mut Context<Self>) -> bool {
        let range = self.selection();
        if range.is_empty() {
            return false;
        }
        // Markdown preserves structure and images when pasting into another note.
        cx.write_to_clipboard(ClipboardItem::new_string(self.source()[range].to_owned()));
        true
    }
    pub(super) fn text_bounds(&self, source: usize) -> Option<Bounds<Pixels>> {
        for layout in self.text_layouts.borrow().iter() {
            if let Some(position) = layout.position_for_source(source) {
                return Some(Bounds::new(position, size(px(1.5), layout.line_height())));
            }
        }
        for (range, bounds) in self.editor.image_bounds.borrow().values() {
            if source == range.start {
                return Some(Bounds::new(
                    bounds.origin,
                    size(px(1.5), px(self.library.font_size as f32 * 1.5)),
                ));
            }
            if source == range.end {
                return Some(Bounds::new(
                    point(bounds.left(), bounds.bottom() - px(2.)),
                    size(px(1.5), px(2.)),
                ));
            }
        }
        None
    }
    pub(super) fn offset_at_point(&self, position: Point<Pixels>) -> usize {
        let mut candidates: Vec<(f32, usize)> = self
            .text_layouts
            .borrow()
            .iter()
            .map(|layout| {
                let bounds = layout.bounds();
                let distance = if position.y < bounds.top() {
                    f32::from(bounds.top() - position.y)
                } else if position.y > bounds.bottom() {
                    f32::from(position.y - bounds.bottom())
                } else {
                    0.
                };
                (distance, layout.source_at(position))
            })
            .collect();
        for (range, bounds) in self.editor.image_bounds.borrow().values() {
            let top = (f32::from(position.y - bounds.top())).abs();
            let bottom = (f32::from(position.y - bounds.bottom())).abs();
            candidates.push((
                top.min(bottom),
                if top < bottom { range.start } else { range.end },
            ));
        }
        candidates
            .into_iter()
            .min_by(|a, b| a.0.total_cmp(&b.0))
            .map(|(_, offset)| offset)
            .unwrap_or(self.source().len())
    }
    pub(super) fn set_caret(
        &mut self,
        offset: usize,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut offset = offset.min(self.source().len());
        while !self.source().is_char_boundary(offset) {
            offset = offset.saturating_sub(1);
        }
        let anchor = if extend {
            self.text_selection
                .map(|(anchor, _)| anchor)
                .unwrap_or(self.caret())
        } else {
            offset
        };
        self.text_selection = Some((anchor, offset));
        self.selected_image = None;
        if self.editor.marked.take().is_some() {
            self.persist(Vec::new(), cx);
        }
        self.break_edit_group();
        self.document_focus.focus(window);
        self.wake_caret(window, cx);
    }
    pub(super) fn start_text_selection(
        &mut self,
        offset: usize,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.goal_x = None;
        self.set_caret(offset, event.modifiers.shift, window, cx);
        if event.click_count == 2 {
            if let Some((start, word)) = self
                .source()
                .unicode_word_indices()
                .find(|(start, word)| *start <= offset && offset <= *start + word.len())
            {
                self.text_selection = Some((start, start + word.len()));
            }
        } else if event.click_count >= 3 {
            let start = self.source()[..offset]
                .rfind('\n')
                .map(|i| i + 1)
                .unwrap_or(0);
            let end = self.source()[offset..]
                .find('\n')
                .map(|i| offset + i)
                .unwrap_or(self.source().len());
            self.text_selection = Some((start, end));
        }
        self.text_drag = Some((event.position, false));
        cx.notify();
    }
    pub(super) fn move_text_selection(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((origin, dragged)) = self.text_drag else {
            return;
        };
        if !event.dragging() || self.image_resize.is_some() {
            return;
        }
        if !dragged
            && f32::from(event.position.x - origin.x).abs()
                + f32::from(event.position.y - origin.y).abs()
                < 3.
        {
            return;
        }
        let offset = self.offset_at_point(event.position);
        let anchor = self
            .text_selection
            .map(|(anchor, _)| anchor)
            .unwrap_or(offset);
        self.text_selection = Some((anchor, offset));
        self.text_drag = Some((origin, true));
        if let Some(viewport) = *self.editor.viewport.borrow() {
            let delta = if event.position.y < viewport.top() + px(20.) {
                16.
            } else if event.position.y > viewport.bottom() - px(20.) {
                -16.
            } else {
                0.
            };
            let current = self.document_scroll.offset();
            let max = self.document_scroll.max_offset().height;
            self.document_scroll.set_offset(point(
                current.x,
                (current.y + px(delta)).max(-max).min(px(0.)),
            ));
        }
        self.editor.blink_on = true;
        cx.notify();
        let _ = window;
    }
    pub(super) fn editor_move(
        &mut self,
        horizontal: i32,
        vertical: i32,
        extend: bool,
        word: bool,
        edge: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.marked.is_some() {
            cx.propagate();
            return;
        }
        let caret = self.caret();
        let target = if vertical != 0 {
            if edge {
                if vertical < 0 { 0 } else { self.source().len() }
            } else if let Some(bounds) = self.text_bounds(caret) {
                let x = self.editor.goal_x.unwrap_or(bounds.left());
                self.editor.goal_x = Some(x);
                self.offset_at_point(point(
                    x,
                    bounds.top() + bounds.size.height * vertical as f32 + bounds.size.height / 2.,
                ))
            } else {
                caret
            }
        } else if edge {
            if horizontal < 0 {
                self.source()[..caret]
                    .rfind('\n')
                    .map(|i| i + 1)
                    .unwrap_or(0)
            } else {
                self.source()[caret..]
                    .find('\n')
                    .map(|i| caret + i)
                    .unwrap_or(self.source().len())
            }
        } else if !extend && !self.selection().is_empty() {
            if horizontal < 0 {
                self.selection().start
            } else {
                self.selection().end
            }
        } else if word {
            if horizontal < 0 {
                self.source()[..caret]
                    .unicode_word_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            } else {
                self.source()[caret..]
                    .unicode_word_indices()
                    .next()
                    .map(|(i, word)| caret + i + word.len())
                    .unwrap_or(self.source().len())
            }
        } else if let Some(target) = self
            .text_layouts
            .borrow()
            .iter()
            .find_map(|layout| layout.move_source(caret, horizontal < 0))
        {
            target
        } else {
            let images = local_images(self.source());
            if horizontal < 0 {
                images
                    .iter()
                    .find(|(range, _)| range.start < caret && caret <= range.end)
                    .map(|(range, _)| range.start)
                    .unwrap_or_else(|| {
                        self.source()[..caret]
                            .grapheme_indices(true)
                            .last()
                            .map(|(i, _)| i)
                            .unwrap_or(0)
                    })
            } else {
                images
                    .iter()
                    .find(|(range, _)| range.start <= caret && caret < range.end)
                    .map(|(range, _)| range.end)
                    .unwrap_or_else(|| {
                        self.source()[caret..]
                            .graphemes(true)
                            .next()
                            .map(|ch| caret + ch.len())
                            .unwrap_or(caret)
                    })
            }
        };
        if vertical == 0 {
            self.editor.goal_x = None;
        }
        self.set_caret(target, extend, window, cx);
    }
    pub(super) fn editor_delete(
        &mut self,
        backward: bool,
        word: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.editor.marked.is_some() {
            cx.propagate();
            return;
        }
        if !self.selection().is_empty() {
            self.replace_selection("", window, cx);
            return;
        }
        let caret = self.caret();
        if let Some((range, name)) = local_images(self.source()).into_iter().find(|(range, _)| {
            if backward {
                range.end == caret
            } else {
                range.start == caret
            }
        }) {
            self.select_image(name, range.end, window, cx);
            self.text_selection = Some((range.start, range.end));
            return;
        }
        if !word {
            let visible = self
                .text_layouts
                .borrow()
                .iter()
                .find_map(|layout| layout.deletion_range(caret, backward));
            if let Some(range) = visible {
                self.edit_range(range, "", false, window, cx);
                return;
            }
        }
        let boundary = if word {
            if backward {
                self.source()[..caret]
                    .unicode_word_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0)
            } else {
                self.source()[caret..]
                    .unicode_word_indices()
                    .next()
                    .map(|(i, word)| caret + i + word.len())
                    .unwrap_or(self.source().len())
            }
        } else if backward {
            self.source()[..caret]
                .grapheme_indices(true)
                .last()
                .map(|(i, _)| i)
                .unwrap_or(caret)
        } else {
            self.source()[caret..]
                .graphemes(true)
                .next()
                .map(|ch| caret + ch.len())
                .unwrap_or(caret)
        };
        if boundary != caret {
            self.edit_range(
                boundary.min(caret)..boundary.max(caret),
                "",
                false,
                window,
                cx,
            );
        }
    }
    pub(super) fn editor_enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editor.marked.is_some() {
            cx.propagate();
            return;
        }
        let caret = self.selection().start;
        let start = self.source()[..caret]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let line = &self.source()[start..caret];
        let indent: String = line
            .chars()
            .take_while(|ch| *ch == ' ' || *ch == '\t')
            .collect();
        let trimmed = line.trim_start();
        let marker = ["- [ ] ", "- [x] ", "* ", "- ", "+ ", "> "]
            .into_iter()
            .find(|marker| trimmed.starts_with(marker));
        let value = if let Some(marker) = marker {
            if trimmed.trim_end() == marker.trim_end() {
                self.edit_range(start..caret, "", false, window, cx);
                return;
            }
            format!(
                "\n{indent}{}",
                if marker == "- [x] " { "- [ ] " } else { marker }
            )
        } else {
            format!("\n{indent}")
        };
        self.edit_range(self.selection(), &value, false, window, cx);
    }
    pub(super) fn editor_paste(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.paste_image(window, cx) {
            if let Some(value) = cx.read_from_clipboard().and_then(|item| item.text()) {
                self.replace_selection(&value, window, cx);
            }
        }
    }
    pub(super) fn document(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.text_layouts.borrow_mut().clear();
        self.editor.image_bounds.borrow_mut().clear();
        let source = self.source();
        let whitespace_only = source.trim().is_empty();
        if self.editor.blocks.borrow().0 != source {
            *self.editor.blocks.borrow_mut() = (source.to_owned(), markdown_blocks(source));
        }
        let ranges = self.editor.blocks.borrow().1.clone();
        let mut document = div()
            .id("native-note-document")
            .key_context("NoteEditor")
            .track_focus(&self.document_focus)
            .size_full()
            .overflow_y_scroll()
            .track_scroll(&self.document_scroll)
            .flex()
            .flex_col()
            .cursor_text()
            .text_color(rgb(self.colors.ink));
        document = bind_editor_actions(document, cx);
        let mut previous_end = 0;
        let mut has_block = false;
        if whitespace_only {
            document = document.child(self.render_text(source, 0, cx));
        }
        for range in ranges.into_iter().filter(|_| !whitespace_only) {
            if range.start > previous_end {
                document = document.child(self.blank_lines(
                    source,
                    previous_end..range.start,
                    has_block,
                    false,
                    cx,
                ));
            }
            previous_end = range.end;
            has_block = true;
            let value = &source[range.clone()];
            if value.trim().is_empty() {
                document = document.child(self.blank_lines(source, range.clone(), false, true, cx));
            } else {
                document = document.child(div().flex_shrink_0().child(self.rendered_block(
                    value,
                    range.start,
                    window,
                    cx,
                )));
            }
        }
        if !whitespace_only && previous_end < source.len() {
            document = document.child(self.blank_lines(
                source,
                previous_end..source.len(),
                false,
                true,
                cx,
            ));
        }
        // A final editable line is part of the document, not a temporary input box.
        if !whitespace_only && source.ends_with('\n') {
            document = document.child(self.editor_blank(source.len(), cx));
        }
        document = document.child(
            div()
                .id("note-end-hit-target")
                .min_h(px(1.))
                .flex_1()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.start_text_selection(this.source().len(), event, window, cx);
                    }),
                ),
        );
        let owner = cx.entity();
        div()
            .relative()
            .flex_1()
            .min_h_0()
            .child(document)
            .child(
                canvas(
                    |bounds, _, _| bounds,
                    move |bounds, _, window, cx| {
                        let notes = owner.read(cx);
                        *notes.editor.viewport.borrow_mut() = Some(bounds);
                        window.handle_input(
                            &notes.document_focus,
                            ElementInputHandler::new(bounds, owner.clone()),
                            cx,
                        );
                        if notes.text_drag.is_some() {
                            let moving = owner.clone();
                            window.on_mouse_event(
                                move |event: &MouseMoveEvent, phase, window, cx| {
                                    if phase.bubble() {
                                        moving.update(cx, |notes, cx| {
                                            notes.move_text_selection(event, window, cx)
                                        });
                                    }
                                },
                            );
                            let released = owner.clone();
                            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                                if phase.bubble() && event.button == MouseButton::Left {
                                    released.update(cx, |notes, _| notes.text_drag = None);
                                }
                            });
                        }
                        if notes.document_focus.is_focused(window)
                            && notes.selection_range().is_none()
                            && notes.editor.blink_on
                        {
                            if let Some(caret) = notes.text_bounds(notes.caret()) {
                                let top = caret.top().max(bounds.top());
                                let bottom = caret.bottom().min(bounds.bottom());
                                if top < bottom {
                                    window.paint_quad(fill(
                                        Bounds::new(
                                            point(caret.left(), top),
                                            size(px(1.5), bottom - top),
                                        ),
                                        rgb(notes.colors.ink),
                                    ));
                                }
                            }
                        }
                        if notes.editor.reveal {
                            let caret = notes.text_bounds(notes.caret());
                            let scroll = notes.document_scroll.clone();
                            let owner = owner.clone();
                            window.defer(cx, move |_, cx| {
                                owner.update(cx, |notes, cx| {
                                    notes.editor.reveal = false;
                                    if let Some(caret) = caret {
                                        let delta = if caret.top() < bounds.top() + px(8.) {
                                            bounds.top() + px(8.) - caret.top()
                                        } else if caret.bottom() > bounds.bottom() - px(8.) {
                                            bounds.bottom() - px(8.) - caret.bottom()
                                        } else {
                                            px(0.)
                                        };
                                        if delta != px(0.) {
                                            let offset = scroll.offset();
                                            scroll.set_offset(point(
                                                offset.x,
                                                (offset.y + delta)
                                                    .max(-scroll.max_offset().height)
                                                    .min(px(0.)),
                                            ));
                                            cx.notify();
                                        }
                                    }
                                });
                            });
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
            .child(
                self.editor
                    .scroll
                    .layer_with_line_height(self.document_scroll.clone(), px(80.)),
            )
            .child(gpui_component::scroll::Scrollbar::vertical(
                &self.editor.scroll.handle(&self.document_scroll),
            ))
            .into_any_element()
    }
    pub(super) fn editor_blank(&self, offset: usize, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex_shrink_0()
            .h(px(self.library.font_size as f32 * 1.5))
            .text_size(px(self.library.font_size as f32))
            .line_height(relative(1.5))
            .child(text::SelectableText::new(
                SharedString::from(format!("note-blank-{offset}")),
                text::Content::blank(offset),
                cx.entity(),
                self.selection_range(),
            ))
            .into_any_element()
    }
    pub(super) fn blank_lines(
        &self,
        source: &str,
        range: Range<usize>,
        skip_separator: bool,
        trailing: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut lines = div().flex().flex_col().flex_shrink_0();
        for (index, _) in source[range.clone()]
            .char_indices()
            .filter(|(_, ch)| *ch == '\n')
            .skip(usize::from(skip_separator))
        {
            let offset = range.start + index + usize::from(trailing);
            // The final document line is added once by document().
            if trailing && offset == source.len() {
                continue;
            }
            lines = lines.child(self.editor_blank(offset, cx));
        }
        lines.into_any_element()
    }
}

fn empty_inline_span(source: &str, removal: Range<usize>) -> Range<usize> {
    fn visit(
        node: &markdown::mdast::Node,
        source: &str,
        removal: &Range<usize>,
        result: &mut Range<usize>,
    ) {
        use markdown::mdast::Node;
        let Some(position) = node.position() else {
            return;
        };
        let span = position.start.offset..position.end.offset;
        if span.start > removal.start || span.end < removal.end {
            return;
        }
        if matches!(
            node,
            Node::Strong(_)
                | Node::Emphasis(_)
                | Node::Delete(_)
                | Node::Link(_)
                | Node::InlineCode(_)
        ) {
            if let Some(value) = source.get(span.clone()) {
                if text::Content::parse(value, span.start).all_text_covered(removal)
                    && span.len() > result.len()
                {
                    *result = span;
                }
            }
        }
        if let Some(children) = node.children() {
            for child in children {
                visit(child, source, removal, result);
            }
        }
    }
    let mut result = removal.clone();
    if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
        visit(&root, source, &removal, &mut result);
    }
    result
}
fn byte_from_utf16(text: &str, offset: usize) -> usize {
    let mut units = 0;
    for (index, ch) in text.char_indices() {
        if units >= offset {
            return index;
        }
        units += ch.len_utf16();
    }
    text.len()
}
impl EntityInputHandler for Notes {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let source = self.source();
        let bytes = byte_from_utf16(source, range.start)..byte_from_utf16(source, range.end);
        *adjusted = Some(
            source[..bytes.start].encode_utf16().count()
                ..source[..bytes.end].encode_utf16().count(),
        );
        Some(source[bytes].to_owned())
    }
    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let range = self.selection();
        Some(UTF16Selection {
            range: self.source()[..range.start].encode_utf16().count()
                ..self.source()[..range.end].encode_utf16().count(),
            reversed: self.text_selection.is_some_and(|(a, b)| a > b),
        })
    }
    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.editor.marked.as_ref().map(|range| {
            self.source()[..range.start].encode_utf16().count()
                ..self.source()[..range.end].encode_utf16().count()
        })
    }
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.editor.marked = None;
        self.persist(Vec::new(), cx);
    }
    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| {
                byte_from_utf16(self.source(), range.start)
                    ..byte_from_utf16(self.source(), range.end)
            })
            .or_else(|| self.editor.marked.clone())
            .unwrap_or_else(|| self.selection());
        if self.editor.marked.take().is_some() {
            let start = range.start;
            self.library.notes[self.library.active]
                .body
                .replace_range(range, value);
            let caret = start + value.len();
            self.text_selection = Some((caret, caret));
            self.editor_changed(false, cx);
            self.wake_caret(window, cx);
        } else {
            self.edit_range(range, value, !value.contains('\n'), window, cx);
        }
    }
    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        value: &str,
        selection: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let range = range
            .map(|range| {
                byte_from_utf16(self.source(), range.start)
                    ..byte_from_utf16(self.source(), range.end)
            })
            .or_else(|| self.editor.marked.clone())
            .unwrap_or_else(|| self.selection());
        let start = range.start;
        if self.editor.marked.is_none() {
            if self.editor.save_pending {
                self.persist(Vec::new(), cx);
            }
            self.checkpoint(false);
        }
        self.library.notes[self.library.active]
            .body
            .replace_range(range, value);
        self.editor.marked = Some(start..start + value.len());
        let selection =
            selection.unwrap_or(value.encode_utf16().count()..value.encode_utf16().count());
        self.text_selection = Some((
            start + byte_from_utf16(value, selection.start),
            start + byte_from_utf16(value, selection.end),
        ));
        self.editor.content.borrow_mut().clear();
        self.wake_caret(window, cx);
    }
    fn bounds_for_range(
        &mut self,
        range: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        self.text_bounds(byte_from_utf16(self.source(), range.start))
            .or(Some(Bounds::new(
                bounds.origin,
                size(px(1.), px(self.library.font_size as f32 * 1.5)),
            )))
    }
    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        Some(
            self.source()[..self.offset_at_point(point)]
                .encode_utf16()
                .count(),
        )
    }
}

// Editor shortcuts are scoped to the document; title and numeric inputs retain
// their own native key handling.
fn bind_editor_actions(document: Stateful<Div>, cx: &mut Context<Notes>) -> Stateful<Div> {
    use gpui_component::input as key;
    document
        .on_action(cx.listener(|this, _: &key::Backspace, window, cx| {
            this.editor_delete(true, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::Delete, window, cx| {
            this.editor_delete(false, false, window, cx)
        }))
        .on_action(
            cx.listener(|this, _: &key::DeleteToPreviousWordStart, window, cx| {
                this.editor_delete(true, true, window, cx)
            }),
        )
        .on_action(
            cx.listener(|this, _: &key::DeleteToNextWordEnd, window, cx| {
                this.editor_delete(false, true, window, cx)
            }),
        )
        .on_action(cx.listener(|this, _: &key::Enter, window, cx| this.editor_enter(window, cx)))
        .on_action(
            cx.listener(|this, _: &key::SelectAll, window, cx| this.select_document(window, cx)),
        )
        .on_action(cx.listener(|this, _: &key::Copy, _, cx| {
            this.copy_selection(cx);
        }))
        .on_action(cx.listener(|this, _: &key::Cut, window, cx| {
            if this.copy_selection(cx) {
                this.replace_selection("", window, cx);
            }
        }))
        .on_action(cx.listener(|this, _: &key::Paste, window, cx| this.editor_paste(window, cx)))
        .on_action(
            cx.listener(|this, _: &key::Undo, window, cx| this.undo_editor(false, window, cx)),
        )
        .on_action(
            cx.listener(|this, _: &key::Redo, window, cx| this.undo_editor(true, window, cx)),
        )
        .on_action(cx.listener(|this, _: &InsertTab, window, cx| {
            this.replace_selection("    ", window, cx);
        }))
        .on_action(cx.listener(|this, _: &key::MoveLeft, window, cx| {
            this.editor_move(-1, 0, false, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveRight, window, cx| {
            this.editor_move(1, 0, false, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveUp, window, cx| {
            this.editor_move(0, -1, false, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveDown, window, cx| {
            this.editor_move(0, 1, false, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ExtendLeft, window, cx| {
            this.editor_move(-1, 0, true, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ExtendRight, window, cx| {
            this.editor_move(1, 0, true, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ExtendUp, window, cx| {
            this.editor_move(0, -1, true, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &ExtendDown, window, cx| {
            this.editor_move(0, 1, true, false, false, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveHome, window, cx| {
            this.editor_move(-1, 0, false, false, true, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveEnd, window, cx| {
            this.editor_move(1, 0, false, false, true, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveToStart, window, cx| {
            this.editor_move(0, -1, false, false, true, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::MoveToEnd, window, cx| {
            this.editor_move(0, 1, false, false, true, window, cx)
        }))
        .on_action(
            cx.listener(|this, _: &key::SelectToStartOfLine, window, cx| {
                this.editor_move(-1, 0, true, false, true, window, cx)
            }),
        )
        .on_action(cx.listener(|this, _: &key::SelectToEndOfLine, window, cx| {
            this.editor_move(1, 0, true, false, true, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::SelectToStart, window, cx| {
            this.editor_move(0, -1, true, false, true, window, cx)
        }))
        .on_action(cx.listener(|this, _: &key::SelectToEnd, window, cx| {
            this.editor_move(0, 1, true, false, true, window, cx)
        }))
        .on_action(
            cx.listener(|this, _: &key::MoveToPreviousWord, window, cx| {
                this.editor_move(-1, 0, false, true, false, window, cx)
            }),
        )
        .on_action(cx.listener(|this, _: &key::MoveToNextWord, window, cx| {
            this.editor_move(1, 0, false, true, false, window, cx)
        }))
        .on_action(
            cx.listener(|this, _: &key::SelectToPreviousWordStart, window, cx| {
                this.editor_move(-1, 0, true, true, false, window, cx)
            }),
        )
        .on_action(
            cx.listener(|this, _: &key::SelectToNextWordEnd, window, cx| {
                this.editor_move(1, 0, true, true, false, window, cx)
            }),
        )
}

pub(super) fn bindings(cx: &mut App) {
    use gpui_component::input as k;
    cx.bind_keys([
        KeyBinding::new("backspace", k::Backspace, Some("NoteEditor")),
        KeyBinding::new("delete", k::Delete, Some("NoteEditor")),
        KeyBinding::new("enter", k::Enter { secondary: false }, Some("NoteEditor")),
        KeyBinding::new("ctrl-a", k::SelectAll, Some("NoteEditor")),
        KeyBinding::new("ctrl-c", k::Copy, Some("NoteEditor")),
        KeyBinding::new("ctrl-x", k::Cut, Some("NoteEditor")),
        KeyBinding::new("ctrl-v", k::Paste, Some("NoteEditor")),
        KeyBinding::new("ctrl-z", k::Undo, Some("NoteEditor")),
        KeyBinding::new("ctrl-y", k::Redo, Some("NoteEditor")),
        KeyBinding::new("ctrl-shift-z", k::Redo, Some("NoteEditor")),
        KeyBinding::new("tab", InsertTab, Some("NoteEditor")),
        KeyBinding::new("left", k::MoveLeft, Some("NoteEditor")),
        KeyBinding::new("right", k::MoveRight, Some("NoteEditor")),
        KeyBinding::new("up", k::MoveUp, Some("NoteEditor")),
        KeyBinding::new("down", k::MoveDown, Some("NoteEditor")),
        KeyBinding::new("shift-left", ExtendLeft, Some("NoteEditor")),
        KeyBinding::new("shift-right", ExtendRight, Some("NoteEditor")),
        KeyBinding::new("shift-up", ExtendUp, Some("NoteEditor")),
        KeyBinding::new("shift-down", ExtendDown, Some("NoteEditor")),
        KeyBinding::new("home", k::MoveHome, Some("NoteEditor")),
        KeyBinding::new("end", k::MoveEnd, Some("NoteEditor")),
        KeyBinding::new("shift-home", k::SelectToStartOfLine, Some("NoteEditor")),
        KeyBinding::new("shift-end", k::SelectToEndOfLine, Some("NoteEditor")),
        KeyBinding::new("ctrl-home", k::MoveToStart, Some("NoteEditor")),
        KeyBinding::new("ctrl-end", k::MoveToEnd, Some("NoteEditor")),
        KeyBinding::new("ctrl-shift-home", k::SelectToStart, Some("NoteEditor")),
        KeyBinding::new("ctrl-shift-end", k::SelectToEnd, Some("NoteEditor")),
        KeyBinding::new("ctrl-left", k::MoveToPreviousWord, Some("NoteEditor")),
        KeyBinding::new("ctrl-right", k::MoveToNextWord, Some("NoteEditor")),
        KeyBinding::new(
            "ctrl-shift-left",
            k::SelectToPreviousWordStart,
            Some("NoteEditor"),
        ),
        KeyBinding::new(
            "ctrl-shift-right",
            k::SelectToNextWordEnd,
            Some("NoteEditor"),
        ),
        KeyBinding::new(
            "ctrl-backspace",
            k::DeleteToPreviousWordStart,
            Some("NoteEditor"),
        ),
        KeyBinding::new("ctrl-delete", k::DeleteToNextWordEnd, Some("NoteEditor")),
    ]);
}
