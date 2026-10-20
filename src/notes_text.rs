//! Markdown text with source positions shared by the entire notes document.
use super::Notes;
use gpui::{prelude::*, *};
use markdown::mdast::Node;
use std::ops::Range;

#[derive(Clone)]
struct Glyph {
    display: Range<usize>,
    source: Range<usize>,
    style: HighlightStyle,
}

#[derive(Clone, Default)]
pub(super) struct Content {
    text: String,
    glyphs: Vec<Glyph>,
    pub heading: Option<u8>,
    pub code: bool,
    links: Vec<(Range<usize>, String)>,
}

impl Content {
    pub fn parse(source: &str, base: usize) -> Self {
        let mut content = Self::default();
        if let Ok(root) = markdown::to_mdast(source, &markdown::ParseOptions::gfm()) {
            content.visit(&root, source, base, HighlightStyle::default());
        } else {
            content.append(source, base..base + source.len(), HighlightStyle::default());
        }
        while content.text.ends_with('\n') {
            content.text.pop();
        }
        content
            .glyphs
            .retain(|glyph| glyph.display.end <= content.text.len());
        content
    }

    fn append(&mut self, value: &str, source: Range<usize>, style: HighlightStyle) {
        let exact = source.len() == value.len();
        for (index, ch) in value.char_indices() {
            let display = self.text.len();
            self.text.push(ch);
            self.glyphs.push(Glyph {
                display: display..self.text.len(),
                source: if exact {
                    source.start + index..source.start + index + ch.len_utf8()
                } else {
                    source.clone()
                },
                style,
            });
        }
    }

    fn newline(&mut self, at: usize) {
        if !self.text.is_empty() && !self.text.ends_with('\n') {
            self.append("\n", at..at, HighlightStyle::default());
        }
    }

    fn visit(&mut self, node: &Node, source: &str, base: usize, mut style: HighlightStyle) {
        let range = node
            .position()
            .map(|p| base + p.start.offset..base + p.end.offset)
            .unwrap_or(base..base);
        let literal = match node {
            Node::Text(n) => Some(n.value.as_str()),
            Node::InlineCode(n) => Some(n.value.as_str()),
            Node::Code(n) => Some(n.value.as_str()),
            Node::InlineMath(n) => Some(n.value.as_str()),
            Node::Math(n) => Some(n.value.as_str()),
            _ => None,
        };
        match node {
            Node::Strong(_) => style.font_weight = Some(FontWeight::BOLD),
            Node::Emphasis(_) => style.font_style = Some(FontStyle::Italic),
            Node::Delete(_) => style.strikethrough = Some(StrikethroughStyle::default()),
            Node::Link(n) => {
                self.links.push((range.clone(), n.url.clone()));
                style.underline = Some(UnderlineStyle::default());
            }
            Node::LinkReference(_) => style.underline = Some(UnderlineStyle::default()),
            Node::InlineCode(_) => style.background_color = Some(rgba(0x80808018).into()),
            Node::Heading(n) => {
                self.heading = Some(n.depth);
                style.font_weight = Some(FontWeight::BOLD);
            }
            Node::Code(_) => self.code = true,
            Node::List(n) if n.ordered => {
                for (index, item) in n.children.iter().enumerate() {
                    let at = item
                        .position()
                        .map(|p| base + p.start.offset)
                        .unwrap_or(range.start);
                    self.newline(at);
                    self.append(
                        &format!("{}. ", n.start.unwrap_or(1) as usize + index),
                        at..at,
                        style,
                    );
                    if let Some(children) = item.children() {
                        for child in children {
                            self.visit(child, source, base, style);
                        }
                    }
                }
                return;
            }
            Node::Break(_) => {
                self.append("\n", range, style);
                return;
            }
            Node::ThematicBreak(_) => {
                self.append("────────────────", range, style);
                return;
            }
            Node::Definition(_) => return,
            Node::Image(n) => {
                self.append(&format!("![{}]({})", n.alt, n.url), range, style);
                return;
            }
            Node::ListItem(n) => {
                self.newline(range.start);
                let marker = match n.checked {
                    Some(true) => "☑ ",
                    Some(false) => "☐ ",
                    None => "• ",
                };
                self.append(marker, range.start..range.start, style);
            }
            Node::TableRow(_) => self.newline(range.start),
            Node::TableCell(_) if !self.text.is_empty() && !self.text.ends_with('\n') => {
                self.append("  │  ", range.start..range.start, style);
            }
            _ => {}
        }
        if let Some(value) = literal {
            let raw = source
                .get(range.start.saturating_sub(base)..range.end.saturating_sub(base))
                .unwrap_or("");
            if let Some(offset) = raw.find(value) {
                self.append(
                    value,
                    range.start + offset..range.start + offset + value.len(),
                    style,
                );
            } else {
                // Markdown decodes escapes/entities and normalizes CRLF. Retain the
                // original byte span of each visible character for editing and selection.
                let mut cursor = 0;
                for ch in value.chars() {
                    let tail = &raw[cursor..];
                    let consumed = if tail.starts_with(ch) {
                        ch.len_utf8()
                    } else if tail.starts_with('\\') && tail[1..].starts_with(ch) {
                        1 + ch.len_utf8()
                    } else if ch == '\n' && tail.starts_with("\r\n") {
                        2
                    } else if tail.starts_with('&') {
                        tail.find(';').map(|end| end + 1).unwrap_or(1)
                    } else {
                        tail.chars().next().map(char::len_utf8).unwrap_or(0)
                    };
                    self.append(
                        &ch.to_string(),
                        range.start + cursor..range.start + cursor + consumed,
                        style,
                    );
                    cursor += consumed;
                }
            }
        } else if let Some(children) = node.children() {
            for child in children {
                self.visit(child, source, base, style);
            }
        }
        if matches!(
            node,
            Node::Paragraph(_) | Node::Heading(_) | Node::Code(_) | Node::Blockquote(_)
        ) {
            self.newline(range.end);
        }
    }

    fn source_at(&self, offset: usize) -> usize {
        self.glyphs
            .iter()
            .find(|glyph| glyph.display.end > offset)
            .map(|glyph| glyph.source.start)
            .or_else(|| self.glyphs.last().map(|glyph| glyph.source.end))
            .unwrap_or(0)
    }

    pub fn selected_text(&self, selection: &Range<usize>) -> String {
        self.glyphs
            .iter()
            .filter(|glyph| {
                glyph.source.end > selection.start && glyph.source.start < selection.end
                    || glyph.source.is_empty() && selection.contains(&glyph.source.start)
            })
            .map(|glyph| &self.text[glyph.display.clone()])
            .collect()
    }
}

#[derive(Clone)]
pub(super) struct FragmentLayout {
    id: ElementId,
    layout: TextLayout,
    content: Content,
}

impl FragmentLayout {
    pub fn bounds(&self) -> Bounds<Pixels> {
        self.layout.bounds()
    }
    pub fn source_at(&self, position: Point<Pixels>) -> usize {
        let index = self
            .layout
            .index_for_position(position)
            .unwrap_or_else(|index| index);
        self.content.source_at(index)
    }
}

pub(super) struct SelectableText {
    id: ElementId,
    content: Content,
    text: StyledText,
    notes: Entity<Notes>,
    selection: Option<Range<usize>>,
}

impl SelectableText {
    pub fn new(
        id: impl Into<ElementId>,
        content: Content,
        notes: Entity<Notes>,
        selection: Option<Range<usize>>,
    ) -> Self {
        Self {
            id: id.into(),
            text: StyledText::new(content.text.clone()),
            content,
            notes,
            selection,
        }
    }
}
impl IntoElement for SelectableText {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}
impl Element for SelectableText {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;
    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }
    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut highlights: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
        for glyph in &self.content.glyphs {
            let mut style = glyph.style;
            if self.selection.as_ref().is_some_and(|selection| {
                glyph.source.end > selection.start && glyph.source.start < selection.end
                    || glyph.source.is_empty() && selection.contains(&glyph.source.start)
            }) {
                style.background_color = Some(rgba(0x6b9edc60).into());
            }
            if let Some((range, previous_style)) = highlights.last_mut() {
                if *previous_style == style && range.end == glyph.display.start {
                    range.end = glyph.display.end;
                    continue;
                }
            }
            highlights.push((glyph.display.clone(), style));
        }
        self.text = StyledText::new(self.content.text.clone()).with_highlights(highlights);
        self.text.request_layout(id, inspector, window, cx)
    }
    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Hitbox {
        self.text.prepaint(id, inspector, bounds, state, window, cx);
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }
    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut (),
        hitbox: &mut Hitbox,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.text
            .paint(id, inspector, bounds, state, &mut (), window, cx);
        let fragment = FragmentLayout {
            id: self.id.clone(),
            layout: self.text.layout().clone(),
            content: self.content.clone(),
        };
        {
            let mut layouts = self.notes.read(cx).text_layouts.borrow_mut();
            layouts.retain(|layout| layout.id != self.id);
            layouts.push(fragment.clone());
        }
        window.set_cursor_style(CursorStyle::IBeam, hitbox);
        let notes = self.notes.clone();
        let hitbox = hitbox.clone();
        window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
            if event.button != MouseButton::Left || !phase.bubble() || !hitbox.is_hovered(window) {
                return;
            }
            let offset = fragment.source_at(event.position);
            if event.modifiers.control {
                if let Some((_, url)) = fragment
                    .content
                    .links
                    .iter()
                    .find(|(range, _)| range.contains(&offset))
                {
                    if url.starts_with("https://")
                        || url.starts_with("http://")
                        || url.starts_with("mailto:")
                    {
                        cx.open_url(url);
                        cx.stop_propagation();
                        return;
                    }
                }
            }
            notes.update(cx, |notes, cx| {
                notes.start_text_selection(offset, event, window, cx)
            });
        });
    }
}
