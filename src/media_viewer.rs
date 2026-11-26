//! Reusable application window for native image viewing and video playback.
use crate::{i18n::tr, theme::Colors};
#[path = "media_window.rs"]
mod placement;
use anyhow::Result;
use gpui::{prelude::*, *};
use gpui_component::{
    Disableable, IconName, Root,
    button::{Button, ButtonVariants},
    slider::{Slider, SliderEvent, SliderState},
};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

actions!(
    media_viewer,
    [
        CloseViewer,
        TogglePlayback,
        FitImage,
        ActualSize,
        SeekBack,
        SeekForward
    ]
);

#[derive(Clone, Default)]
struct ViewerSlot {
    window: Option<AnyWindowHandle>,
    view: Option<WeakEntity<Viewer>>,
}
impl Global for ViewerSlot {}

pub fn open(
    path: String,
    video: bool,
    thumbnail: Option<PathBuf>,
    colors: Colors,
    source_window: &Window,
    cx: &mut App,
) -> Result<()> {
    let placement = placement::Placement::new(source_window, cx);
    if let Some(slot) = cx.try_global::<ViewerSlot>().cloned() {
        if let (Some(handle), Some(view)) = (slot.window, slot.view.and_then(|view| view.upgrade()))
        {
            let source = path.clone();
            let thumb = thumbnail.clone();
            if handle
                .update(cx, |_, window, cx| {
                    placement.apply(window, false);
                    view.update(cx, |view, cx| {
                        view.load(source, video, thumb, colors, window, cx)
                    });
                    #[cfg(windows)]
                    crate::tray::show(window, cx);
                    window.activate_window();
                })
                .is_ok()
            {
                return Ok(());
            }
        }
    }
    cx.bind_keys([
        KeyBinding::new("escape", CloseViewer, Some("MediaViewer")),
        KeyBinding::new("space", TogglePlayback, Some("MediaViewer")),
        KeyBinding::new("r", FitImage, Some("MediaViewer")),
        KeyBinding::new("1", ActualSize, Some("MediaViewer")),
        KeyBinding::new("left", SeekBack, Some("MediaViewer")),
        KeyBinding::new("right", SeekForward, Some("MediaViewer")),
    ]);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(placement.bounds)),
            display_id: placement.display_id,
            window_min_size: Some(size(px(540.), px(400.))),
            titlebar: Some(TitlebarOptions {
                title: Some(tr("媒体查看器").into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        move |window, cx| {
            placement.apply(window, true);
            let view = cx.new(|cx| Viewer::new(window, cx));
            cx.set_global(ViewerSlot {
                window: Some(window.window_handle()),
                view: Some(view.downgrade()),
            });
            let weak = view.downgrade();
            window.on_window_should_close(cx, move |window, cx| {
                if let Some(view) = weak.upgrade() {
                    view.update(cx, |view, cx| view.release(window, cx));
                }
                cx.set_global(ViewerSlot::default());
                true
            });
            view.update(cx, |view, cx| {
                view.load(path, video, thumbnail, colors, window, cx)
            });
            cx.new(|cx| Root::new(view, window, cx))
        },
    )?;
    Ok(())
}

struct ImageData {
    image: Arc<RenderImage>,
    width: f32,
    height: f32,
}
fn decode_image(path: &str) -> Result<ImageData> {
    let mut reader = image::ImageReader::open(path)?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(16384);
    limits.max_image_height = Some(16384);
    reader.limits(limits);
    let mut pixels = reader.decode()?.into_rgba8();
    let width = pixels.width() as f32;
    let height = pixels.height() as f32;
    // GPUI render images contain premultiplied BGRA pixels.
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = pixel[3] as u16;
        for channel in &mut pixel[..3] {
            *channel = ((*channel as u16 * alpha + 127) / 255) as u8;
        }
        pixel.swap(0, 2);
    }
    Ok(ImageData {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(pixels)])),
        width,
        height,
    })
}

struct Viewer {
    path: String,
    video: bool,
    colors: Colors,
    image: Option<ImageData>,
    thumbnail: Option<PathBuf>,
    viewport: Rc<Cell<Bounds<Pixels>>>,
    fit: bool,
    zoom: f32,
    pan: Point<f32>,
    drag: Option<(Point<Pixels>, Point<f32>)>,
    loading: bool,
    opened: Instant,
    error: String,
    focus: FocusHandle,
    decode: Option<Task<()>>,
    poll: Option<Task<()>>,
    #[cfg(windows)]
    player: Option<Rc<crate::media_player::Player>>,
    position: f64,
    duration: f64,
    playing: bool,
    ended: bool,
    pending_seek: Option<f64>,
    scrubbing_until: Instant,
    progress: Entity<SliderState>,
    _subscription: Subscription,
}
impl Viewer {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let progress = cx.new(|_| SliderState::new().min(0.).max(1000.).step(1.));
        let subscription = cx.subscribe_in(&progress, window, |this, _, event, _, cx| {
            let SliderEvent::Change(value) = event;
            if this.duration > 0. {
                let position = this.duration * value.start() as f64 / 1000.;
                this.pending_seek = Some(position);
                this.position = position;
                this.ended = false;
                this.scrubbing_until = Instant::now() + Duration::from_millis(300);
                cx.notify();
            }
        });
        Self {
            path: String::new(),
            video: false,
            colors: crate::theme::find("default").colors(),
            image: None,
            thumbnail: None,
            viewport: Rc::new(Cell::new(Bounds::default())),
            fit: true,
            zoom: 1.,
            pan: point(0., 0.),
            drag: None,
            loading: true,
            opened: Instant::now(),
            error: String::new(),
            focus: cx.focus_handle(),
            decode: None,
            poll: None,
            #[cfg(windows)]
            player: None,
            position: 0.,
            duration: 0.,
            playing: false,
            ended: false,
            pending_seek: None,
            scrubbing_until: Instant::now(),
            progress,
            _subscription: subscription,
        }
    }
    fn release(&mut self, window: &mut Window, _: &mut Context<Self>) {
        self.decode = None;
        self.poll = None;
        #[cfg(windows)]
        if let Some(player) = self.player.take() {
            player.shutdown();
        }
        if let Some(image) = self.image.take() {
            let _ = window.drop_image(image.image);
        }
        self.thumbnail = None;
        self.drag = None;
    }
    fn load(
        &mut self,
        path: String,
        video: bool,
        thumbnail: Option<PathBuf>,
        colors: Colors,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.path == path && self.video == video && self.error.is_empty() {
            self.colors = colors;
            self.focus.focus(window);
            cx.notify();
            return;
        }
        self.release(window, cx);
        self.path = path.clone();
        self.video = video;
        self.colors = colors;
        self.thumbnail = thumbnail;
        self.error.clear();
        self.loading = true;
        self.opened = Instant::now();
        self.fit = true;
        self.zoom = 1.;
        self.pan = point(0., 0.);
        self.position = 0.;
        self.duration = 0.;
        self.playing = false;
        self.ended = false;
        self.pending_seek = None;
        self.scrubbing_until = Instant::now();
        self.progress
            .update(cx, |slider, cx| slider.set_value(0., window, cx));
        let name = Path::new(&path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        window.set_window_title(&format!("{name} — Project Manager"));
        self.focus.focus(window);
        if video {
            #[cfg(windows)]
            match crate::media_player::Player::new(&path, window) {
                Ok(player) => {
                    self.player = Some(Rc::new(player));
                    self.poll = Some(cx.spawn_in(window, async move |this, cx| {
                        loop {
                            Timer::after(Duration::from_millis(100)).await;
                            let keep = this.update_in(cx, |this, window, cx| this.tick(window, cx));
                            if !matches!(keep, Ok(true)) {
                                break;
                            }
                        }
                    }));
                }
                Err(error) => {
                    self.error = format!("{}：{error:#}", tr("无法播放此视频"));
                    self.loading = false;
                }
            }
            #[cfg(not(windows))]
            {
                self.error = tr("无法播放此视频").into();
                self.loading = false;
            }
        } else {
            let job = cx
                .background_executor()
                .spawn(async move { decode_image(&path) });
            self.decode = Some(cx.spawn_in(window, async move |this, cx| {
                let result = job.await;
                let _ = this.update_in(cx, |this, _, cx| {
                    this.loading = false;
                    match result {
                        Ok(image) => {
                            this.image = Some(image);
                            this.thumbnail = None;
                        }
                        Err(error) => this.error = format!("{}：{error:#}", tr("无法读取此图片")),
                    }
                    cx.notify();
                });
            }));
        }
        cx.notify();
    }
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        #[cfg(windows)]
        if let Some(player) = self.player.clone() {
            let before = (
                self.position,
                self.duration,
                self.playing,
                self.loading,
                self.ended,
                self.error.len(),
            );
            let state = player.poll();
            if let Some(error) = state.error {
                player.shutdown();
                self.error = format!("{}：{error}", tr("无法播放此视频"));
                self.loading = false;
                self.player = None;
                cx.notify();
                return false;
            }
            self.duration = state.duration;
            self.playing = state.playing;
            if state.ended {
                self.ended = true;
                self.playing = false;
            }
            if self.duration > 0. || self.playing || self.ended {
                self.loading = false;
            }
            if self.loading && self.opened.elapsed() > Duration::from_secs(30) {
                player.shutdown();
                self.player = None;
                self.loading = false;
                self.error = tr("视频无法加载，请检查文件及系统解码支持。").into();
                cx.notify();
                return false;
            }
            if let Some(position) = self.pending_seek.take() {
                if let Err(error) = player.seek(position.min(self.duration.max(0.))) {
                    self.error = format!("{}：{error}", tr("无法调整播放进度"));
                }
            } else if Instant::now() >= self.scrubbing_until {
                self.position = if self.ended {
                    self.duration
                } else {
                    state.position
                };
                let value = if self.duration > 0. {
                    (self.position / self.duration * 1000.) as f32
                } else {
                    0.
                };
                let value = value.clamp(0., 1000.);
                if (self.progress.read(cx).value().start() - value).abs() > 0.001 {
                    self.progress
                        .update(cx, |slider, cx| slider.set_value(value, window, cx));
                }
            }
            if before
                != (
                    self.position,
                    self.duration,
                    self.playing,
                    self.loading,
                    self.ended,
                    self.error.len(),
                )
            {
                cx.notify();
            }
            return true;
        }
        false
    }
    fn toggle(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        if let Some(player) = &self.player {
            let result = (|| -> Result<()> {
                if self.ended {
                    player.seek(0.)?;
                    self.ended = false;
                }
                player.toggle()
            })();
            if let Err(error) = result {
                self.error = format!("{}：{error}", tr("无法播放此视频"));
            }
        }
        cx.notify();
    }
    fn seek_by(&mut self, seconds: f64, window: &mut Window, cx: &mut Context<Self>) {
        if self.duration > 0. {
            self.position = (self.position + seconds).clamp(0., self.duration);
            self.pending_seek = Some(self.position);
            self.ended = false;
            self.scrubbing_until = Instant::now() + Duration::from_millis(300);
            self.progress.update(cx, |slider, cx| {
                slider.set_value((self.position / self.duration * 1000.) as f32, window, cx)
            });
            cx.notify();
        }
    }
    fn scale(&self) -> f32 {
        if !self.fit {
            return self.zoom;
        }
        if let Some(image) = &self.image {
            let bounds = self.viewport.get();
            (f32::from(bounds.size.width) / image.width)
                .min(f32::from(bounds.size.height) / image.height)
                .min(1.)
                .max(0.01)
        } else {
            1.
        }
    }
    fn zoom_at(&mut self, factor: f32, cursor: Point<Pixels>, cx: &mut Context<Self>) {
        if self.image.is_none() {
            return;
        }
        let bounds = self.viewport.get();
        let old = self.scale();
        let next = (old * factor).clamp(0.001, 16.);
        let x = f32::from(cursor.x - bounds.origin.x - bounds.size.width / 2.);
        let y = f32::from(cursor.y - bounds.origin.y - bounds.size.height / 2.);
        self.pan = point(
            x - (x - self.pan.x) * next / old,
            y - (y - self.pan.y) * next / old,
        );
        self.fit = false;
        self.zoom = next;
        cx.notify();
    }
    fn reset(&mut self, fit: bool, cx: &mut Context<Self>) {
        self.fit = fit;
        self.zoom = 1.;
        self.pan = point(0., 0.);
        self.drag = None;
        cx.notify();
    }
}

fn time_label(seconds: f64) -> String {
    let seconds = seconds.max(0.) as u64;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

impl Render for Viewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let colors = self.colors;
        let title = Path::new(&self.path)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut viewport = div()
            .id("media-viewport")
            .relative()
            .w_full()
            .flex_1()
            .min_h_0()
            .overflow_hidden()
            .bg(rgb(0x101214));
        if !self.video {
            let bounds = self.viewport.get();
            let scale = self.scale();
            if let Some(image) = &self.image {
                let width = image.width * scale;
                let height = image.height * scale;
                viewport = viewport.child(
                    img(image.image.clone())
                        .absolute()
                        .left(px((f32::from(bounds.size.width) - width) / 2. + self.pan.x))
                        .top(px(
                            (f32::from(bounds.size.height) - height) / 2. + self.pan.y
                        ))
                        .w(px(width))
                        .h(px(height))
                        .object_fit(ObjectFit::Fill),
                );
            } else if let Some(thumbnail) = &self.thumbnail {
                viewport = viewport.child(
                    img(thumbnail.clone())
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                );
            }
            let state = self.viewport.clone();
            let owner = cx.entity_id();
            viewport = viewport
                .child(
                    canvas(
                        move |bounds, _, cx| {
                            if state.replace(bounds) != bounds {
                                cx.notify(owner);
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .inset_0(),
                )
                .cursor(if self.drag.is_some() {
                    CursorStyle::ClosedHand
                } else {
                    CursorStyle::OpenHand
                })
                .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                    cx.stop_propagation();
                    let delta = f32::from(event.delta.pixel_delta(px(40.)).y);
                    this.zoom_at((delta * 0.003).clamp(-1., 1.).exp(), event.position, cx);
                }))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, event: &MouseDownEvent, window, cx| {
                        this.focus.focus(window);
                        if event.click_count == 2 {
                            this.reset(true, cx);
                        } else {
                            this.drag = Some((event.position, this.pan));
                            cx.notify();
                        }
                    }),
                );
        }
        #[cfg(windows)]
        if self.video {
            if let Some(player) = self.player.clone() {
                viewport = viewport.child(
                    canvas(
                        move |bounds, window, _| {
                            player.layout(bounds, window.scale_factor());
                        },
                        |_, _, _, _| {},
                    )
                    .size_full(),
                );
            }
        }
        if !self.error.is_empty() {
            viewport = viewport.child(
                div()
                    .absolute()
                    .inset_0()
                    .p_6()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0xf1b3a5))
                    .child(self.error.clone()),
            );
        }
        div()
            .size_full()
            .flex()
            .flex_col()
            .key_context("MediaViewer")
            .track_focus(&self.focus)
            .bg(rgb(colors.bg))
            .text_color(rgb(colors.ink))
            .on_action(cx.listener(|this, _: &CloseViewer, window, cx| {
                this.release(window, cx);
                window.remove_window();
            }))
            .on_action(cx.listener(|this, _: &TogglePlayback, _, cx| {
                if this.video {
                    this.toggle(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FitImage, _, cx| this.reset(true, cx)))
            .on_action(cx.listener(|this, _: &ActualSize, _, cx| this.reset(false, cx)))
            .on_action(cx.listener(|this, _: &SeekBack, window, cx| this.seek_by(-5., window, cx)))
            .on_action(
                cx.listener(|this, _: &SeekForward, window, cx| this.seek_by(5., window, cx)),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some((start, pan)) = this.drag {
                    if event.pressed_button == Some(MouseButton::Left) {
                        this.pan = point(
                            pan.x + f32::from(event.position.x - start.x),
                            pan.y + f32::from(event.position.y - start.y),
                        );
                    } else {
                        this.drag = None;
                    }
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.drag = None;
                    cx.notify();
                }),
            )
            .child(
                div()
                    .h(px(52.))
                    .px_4()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .when(self.loading, |s| {
                        s.child(
                            div()
                                .text_size(px(12.))
                                .text_color(rgb(colors.muted))
                                .child(tr("正在加载媒体…")),
                        )
                    })
                    .when(!self.video, |s| {
                        s.child(
                            Button::new("image-zoom-out")
                                .ghost()
                                .icon(IconName::Minus)
                                .tooltip(tr("缩小"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let bounds = this.viewport.get();
                                    this.zoom_at(1. / 1.25, bounds.center(), cx);
                                })),
                        )
                        .child(
                            div()
                                .w(px(58.))
                                .text_size(px(12.))
                                .child(format!("{:.0}%", self.scale() * 100.)),
                        )
                        .child(
                            Button::new("image-zoom-in")
                                .ghost()
                                .icon(IconName::Plus)
                                .tooltip(tr("放大"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    let bounds = this.viewport.get();
                                    this.zoom_at(1.25, bounds.center(), cx);
                                })),
                        )
                        .child(
                            Button::new("image-fit")
                                .ghost()
                                .icon(IconName::Maximize)
                                .tooltip(tr("适应窗口"))
                                .on_click(cx.listener(|this, _, _, cx| this.reset(true, cx))),
                        )
                        .child(
                            Button::new("image-actual")
                                .ghost()
                                .label("100%")
                                .tooltip(tr("原始大小"))
                                .on_click(cx.listener(|this, _, _, cx| this.reset(false, cx))),
                        )
                    }),
            )
            .child(viewport)
            .when(self.video, |s| {
                s.child(
                    div()
                        .h(px(76.))
                        .flex_shrink_0()
                        .px_4()
                        .flex()
                        .items_center()
                        .gap_4()
                        .child(
                            Button::new("video-play-pause")
                                .primary()
                                .icon(if self.playing {
                                    crate::assets::ActionIcon::Pause
                                } else {
                                    crate::assets::ActionIcon::Play
                                })
                                .label(if self.playing {
                                    tr("暂停")
                                } else {
                                    tr("播放")
                                })
                                .disabled(self.loading || !self.error.is_empty())
                                .on_click(cx.listener(|this, _, _, cx| this.toggle(cx))),
                        )
                        .child(div().text_size(px(12.)).child(time_label(self.position)))
                        .child(
                            div().flex_1().min_w_0().child(
                                Slider::new(&self.progress)
                                    .horizontal()
                                    .disabled(self.duration <= 0.),
                            ),
                        )
                        .child(div().text_size(px(12.)).child(time_label(self.duration))),
                )
            })
    }
}
