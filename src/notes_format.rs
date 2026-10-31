//! Native inline formatting, independent of Markdown syntax and cursor positions.
use super::*;
use gpui_component::menu::{PopupMenu, PopupMenuItem};
use std::ops::Range;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct TextFormat {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bold: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    strike: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    underline: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    color: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    background: Option<u32>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct FormatSpan {
    pub range: Range<usize>,
    format: TextFormat,
}

#[derive(Clone, Copy)]
pub(super) enum StyleChange {
    Bold(bool),
    Strike(bool),
    Underline(bool),
    Color(Option<u32>),
    Background(Option<u32>),
}

pub(super) fn default_text_color() -> u32 {
    0x2563eb
}
pub(super) fn default_background_color() -> u32 {
    0xffe08a
}

impl TextFormat {
    pub fn render(self, mut style: HighlightStyle) -> HighlightStyle {
        if let Some(bold) = self.bold {
            style.font_weight = Some(if bold {
                FontWeight::BOLD
            } else {
                FontWeight::NORMAL
            });
        }
        if let Some(strike) = self.strike {
            style.strikethrough = strike.then(StrikethroughStyle::default);
        }
        if let Some(underline) = self.underline {
            style.underline = underline.then(|| UnderlineStyle {
                thickness: px(1.),
                ..Default::default()
            });
        }
        if let Some(color) = self.color {
            style.color = Some(rgb(color & 0xffffff).into());
        }
        if let Some(color) = self.background {
            style.background_color = Some(rgb(color & 0xffffff).into());
        }
        style
    }
    fn change(&mut self, change: StyleChange) {
        match change {
            StyleChange::Bold(value) => self.bold = Some(value),
            StyleChange::Strike(value) => self.strike = Some(value),
            StyleChange::Underline(value) => self.underline = Some(value),
            StyleChange::Color(value) => self.color = value,
            StyleChange::Background(value) => self.background = value,
        }
    }
}

impl StyleChange {
    pub fn matches(self, style: HighlightStyle) -> bool {
        match self {
            Self::Bold(value) => (style.font_weight == Some(FontWeight::BOLD)) == value,
            Self::Strike(value) => style.strikethrough.is_some() == value,
            Self::Underline(value) => style.underline.is_some() == value,
            Self::Color(value) => style.color == value.map(|color| rgb(color & 0xffffff).into()),
            Self::Background(value) => {
                style.background_color == value.map(|color| rgb(color & 0xffffff).into())
            }
        }
    }
}

pub(super) fn format_at(spans: &[FormatSpan], offset: usize) -> TextFormat {
    let index = spans.partition_point(|span| span.range.end <= offset);
    spans
        .get(index)
        .filter(|span| span.range.contains(&offset))
        .map_or(TextFormat::default(), |span| span.format)
}

fn push_span(spans: &mut Vec<FormatSpan>, range: Range<usize>, format: TextFormat) {
    if range.is_empty() || format == TextFormat::default() {
        return;
    }
    if let Some(previous) = spans.last_mut() {
        if previous.range.end == range.start && previous.format == format {
            previous.range.end = range.end;
            return;
        }
    }
    spans.push(FormatSpan { range, format });
}

fn apply(spans: &mut Vec<FormatSpan>, range: Range<usize>, change: StyleChange) {
    let mut edges = vec![range.start, range.end];
    edges.extend(
        spans
            .iter()
            .flat_map(|span| [span.range.start, span.range.end]),
    );
    edges.sort_unstable();
    edges.dedup();
    let mut updated = Vec::new();
    for pair in edges.windows(2) {
        let segment = pair[0]..pair[1];
        let mut format = format_at(spans, segment.start);
        if range.contains(&segment.start) {
            format.change(change);
        }
        push_span(&mut updated, segment, format);
    }
    *spans = updated;
}

// Keep styles attached to text through insertion, replacement, deletion and IME.
fn rebase(spans: &mut Vec<FormatSpan>, range: Range<usize>, inserted: usize) {
    let inherited = if range.is_empty() {
        spans
            .iter()
            .find(|span| span.range.start < range.start && span.range.end >= range.start)
            .map_or_else(|| format_at(spans, range.start), |span| span.format)
    } else {
        format_at(spans, range.start)
    };
    let mut updated = Vec::new();
    for span in spans.iter() {
        if span.range.start < range.start {
            push_span(
                &mut updated,
                span.range.start..span.range.end.min(range.start),
                span.format,
            );
        }
    }
    push_span(&mut updated, range.start..range.start + inserted, inherited);
    for span in spans.iter() {
        if span.range.end > range.end {
            let start = span.range.start.max(range.end) - range.end + range.start + inserted;
            let end = span.range.end - range.end + range.start + inserted;
            push_span(&mut updated, start..end, span.format);
        }
    }
    *spans = updated;
}

impl Notes {
    pub(super) fn replace_source(&mut self, range: Range<usize>, value: &str) {
        let note = &mut self.library.notes[self.library.active];
        rebase(&mut note.formats, range.clone(), value.len());
        note.body.replace_range(range, value);
    }

    fn selection_has_style(&self, range: &Range<usize>, change: StyleChange) -> bool {
        let Some(note) = self.library.notes.get(self.library.active) else {
            return false;
        };
        let (total, active) = self
            .text_layouts
            .borrow()
            .iter()
            .map(|layout| layout.format_counts(range, &note.formats, change))
            .fold((0, 0), |(total, active), (count, matching)| {
                (total + count, active + matching)
            });
        total > 0 && active == total
    }

    fn apply_selection_style(
        &mut self,
        note_id: &str,
        range: Range<usize>,
        change: StyleChange,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(note) = self.library.notes.get(self.library.active) else {
            return;
        };
        if note.id != note_id || range.is_empty() || note.body.get(range.clone()).is_none() {
            return;
        }
        self.checkpoint(false);
        apply(
            &mut self.library.notes[self.library.active].formats,
            range.clone(),
            change,
        );
        match change {
            StyleChange::Color(Some(color)) => self.library.last_text_color = color,
            StyleChange::Background(Some(color)) => self.library.last_background_color = color,
            _ => {}
        }
        self.text_selection = Some((range.start, range.end));
        self.editor.marked = None;
        self.editor_changed(false, cx);
        self.document_focus.focus(window);
        self.wake_caret(window, cx);
    }

    pub(super) fn format_menu(&self) -> Option<FormatMenu> {
        let range = self.selection();
        let note = self.library.notes.get(self.library.active)?;
        if range.is_empty() || self.selected_image.is_some() {
            return None;
        }
        let last_background = self.library.last_background_color & 0xffffff;
        Some(FormatMenu {
            id: note.id.clone(),
            focus: self.document_focus.clone(),
            bold: self.selection_has_style(&range, StyleChange::Bold(true)),
            strike: self.selection_has_style(&range, StyleChange::Strike(true)),
            underline: self.selection_has_style(&range, StyleChange::Underline(true)),
            highlighted: self
                .selection_has_style(&range, StyleChange::Background(Some(last_background))),
            range,
            last_text: self.library.last_text_color & 0xffffff,
            last_background,
        })
    }
}

pub(super) struct FormatMenu {
    id: String,
    range: Range<usize>,
    focus: FocusHandle,
    bold: bool,
    strike: bool,
    underline: bool,
    highlighted: bool,
    last_text: u32,
    last_background: u32,
}

impl FormatMenu {
    pub fn build(
        self,
        mut menu: PopupMenu,
        window: &mut Window,
        cx: &mut Context<PopupMenu>,
        owner: WeakEntity<Notes>,
    ) -> PopupMenu {
        let range = self.range;
        menu = menu.action_context(self.focus.clone()).min_w(px(210.));
        for (label, change, active) in [
            ("加粗", StyleChange::Bold(true), self.bold),
            ("删除线", StyleChange::Strike(true), self.strike),
            ("下划线", StyleChange::Underline(true), self.underline),
        ] {
            let change = match change {
                StyleChange::Bold(_) => StyleChange::Bold(!active),
                StyleChange::Strike(_) => StyleChange::Strike(!active),
                _ => StyleChange::Underline(!active),
            };
            menu = menu.item(
                style_item(label, owner.clone(), self.id.clone(), range.clone(), change)
                    .checked(active),
            );
        }
        let last_background = self.last_background;
        let highlighted = self.highlighted;
        menu = menu
            .item(
                color_item(
                    "高亮",
                    last_background,
                    owner.clone(),
                    self.id.clone(),
                    range.clone(),
                    StyleChange::Background((!highlighted).then_some(last_background)),
                )
                .checked(highlighted),
            )
            .separator();
        for (label, background, last_color) in [
            ("文字颜色", false, self.last_text),
            ("高亮背景", true, last_background),
        ] {
            let owner = owner.clone();
            let id = self.id.clone();
            let range = range.clone();
            let focus = self.focus.clone();
            menu = menu.submenu(tr(label), window, cx, move |mut menu, _, _| {
                let change = |color| {
                    if background {
                        StyleChange::Background(color)
                    } else {
                        StyleChange::Color(color)
                    }
                };
                menu = menu.item(color_item(
                    "上次使用的颜色",
                    last_color,
                    owner.clone(),
                    id.clone(),
                    range.clone(),
                    change(Some(last_color)),
                ));
                menu = menu
                    .item(style_item(
                        if background {
                            "无背景颜色"
                        } else {
                            "默认文字颜色"
                        },
                        owner.clone(),
                        id.clone(),
                        range.clone(),
                        change(None),
                    ))
                    .separator();
                let palette = if background {
                    BACKGROUND_COLORS
                } else {
                    TEXT_COLORS
                };
                for &(label, color) in palette {
                    menu = menu.item(
                        color_item(
                            label,
                            color,
                            owner.clone(),
                            id.clone(),
                            range.clone(),
                            change(Some(color)),
                        )
                        .checked(color == last_color),
                    );
                }
                menu.action_context(focus.clone())
            });
        }
        menu
    }
}

const TEXT_COLORS: &[(&str, u32)] = &[
    ("灰色", 0x64748b),
    ("红色", 0xdc2626),
    ("橙色", 0xc2410c),
    ("黄色", 0xa16207),
    ("绿色", 0x15803d),
    ("蓝色", 0x2563eb),
    ("紫色", 0x9333ea),
];
const BACKGROUND_COLORS: &[(&str, u32)] = &[
    ("黄色", 0xffe08a),
    ("橙色", 0xffcba4),
    ("粉色", 0xffc9da),
    ("绿色", 0xc8e6b2),
    ("蓝色", 0xbfdbfe),
    ("紫色", 0xddd0fa),
    ("灰色", 0xdde2e8),
];

fn style_item(
    label: &'static str,
    owner: WeakEntity<Notes>,
    id: String,
    range: Range<usize>,
    change: StyleChange,
) -> PopupMenuItem {
    PopupMenuItem::new(tr(label)).on_click(move |_, window, cx| {
        let _ = owner.update(cx, |notes, cx| {
            notes.apply_selection_style(&id, range.clone(), change, window, cx)
        });
    })
}

fn color_item(
    label: &'static str,
    color: u32,
    owner: WeakEntity<Notes>,
    id: String,
    range: Range<usize>,
    change: StyleChange,
) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .size(px(16.))
                    .flex_shrink_0()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(rgba(0x00000020))
                    .bg(rgb(color)),
            )
            .child(tr(label))
    })
    .on_click(move |_, window, cx| {
        let _ = owner.update(cx, |notes, cx| {
            notes.apply_selection_style(&id, range.clone(), change, window, cx)
        });
    })
}
