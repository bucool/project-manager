//! Project-card Git controls and the commit/push dialog.
use super::*;
use crate::project_git::{self, Status};
use std::{cell::RefCell, collections::HashMap};

pub(super) struct GitUi {
    monitor: project_git::Monitor,
    states: HashMap<String, Status>,
    visible: RefCell<HashSet<String>>,
    input: Entity<InputState>,
    dialog: Option<Project>,
    active: Option<String>,
    error: String,
}

impl GitUi {
    pub fn new(window: &mut Window, cx: &mut Context<Workbench>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(tr("填写提交说明…")));
        let (monitor, events) = project_git::Monitor::start();
        cx.spawn_in(window, async move |this, cx| {
            while let Ok(event) = events.recv().await {
                if this
                    .update_in(cx, |this, window, cx| {
                        match event {
                            project_git::Event::Status(path, status) => {
                                this.git.states.insert(path, status);
                            }
                            project_git::Event::Finished { path, error } => {
                                this.git.active = None;
                                if let Some(error) = error {
                                    this.git.error = error;
                                } else {
                                    this.git.error.clear();
                                    if this
                                        .git
                                        .dialog
                                        .as_ref()
                                        .is_some_and(|project| project.path == path)
                                    {
                                        this.git.dialog = None;
                                        this.focus.focus(window);
                                    }
                                    this.message = tr("提交并推送完成").into();
                                    this.message_at = Instant::now();
                                    this.error = false;
                                    let stamp = this.message_at;
                                    this.toast_timer = Some(cx.spawn(async move |this, cx| {
                                        Timer::after(Duration::from_secs(5)).await;
                                        let _ = this.update(cx, |this, cx| {
                                            if this.message_at == stamp && !this.busy && !this.error
                                            {
                                                this.message.clear();
                                                cx.notify();
                                            }
                                        });
                                    }));
                                }
                            }
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
        // Only poll cards that were rendered, while the project window is active.
        // The worker coalesces requests and throttles each path to once per 10 s.
        cx.spawn_in(window, async move |this, cx| {
            loop {
                Timer::after(Duration::from_secs(5)).await;
                if this
                    .update_in(cx, |this, window, _| {
                        if window.is_window_active() && this.view != View::Notes {
                            for path in this.git.visible.borrow().iter() {
                                this.git.monitor.check(path, false);
                            }
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        Self {
            monitor,
            states: HashMap::new(),
            visible: RefCell::new(HashSet::new()),
            input,
            dialog: None,
            active: None,
            error: String::new(),
        }
    }

    pub fn begin_frame(&self) {
        self.visible.borrow_mut().clear();
    }
    pub fn is_open(&self) -> bool {
        self.dialog.is_some()
    }
}

impl Workbench {
    fn open_git(&mut self, project: Project, window: &mut Window, cx: &mut Context<Self>) {
        if self.git.active.is_some() {
            return;
        }
        self.git.error.clear();
        self.git.monitor.check(&project.path, true);
        self.git.input.update(cx, |input, cx| {
            input.set_value(format!("Update {}", project.name), window, cx);
            input.focus(window, cx);
        });
        self.smooth_scroll.cancel();
        self.selected = None;
        self.git.dialog = Some(project);
        cx.notify();
    }

    pub(super) fn close_git(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.git.active.is_none() {
            self.git.dialog = None;
            self.git.error.clear();
            self.focus.focus(window);
            cx.notify();
        }
    }

    fn submit_git(&mut self, cx: &mut Context<Self>) {
        if self.git.active.is_some() {
            return;
        }
        let Some(project) = self.git.dialog.as_ref() else {
            return;
        };
        let Some(status) = self.git.states.get(&project.path) else {
            return;
        };
        if status.error.is_some() || status.branch.is_empty() {
            return;
        }
        let message = self.git.input.read(cx).value().to_string();
        if !status.changes.is_empty() && message.trim().is_empty() {
            self.git.error = tr("请填写提交说明。").into();
        } else if self
            .git
            .monitor
            .publish(project.path.clone(), status.branch.clone(), message)
        {
            self.git.error.clear();
            self.git.active = Some(project.path.clone());
        } else {
            self.git.error = tr("Git 后台繁忙，请稍后重试。").into();
        }
        cx.notify();
    }

    pub(super) fn git_controls(&self, project: &Project, cx: &Context<Self>) -> AnyElement {
        self.git.visible.borrow_mut().insert(project.path.clone());
        self.git.monitor.check(&project.path, false);
        let state = self.git.states.get(&project.path);
        if state.is_some_and(|state| !state.repository) {
            return div().into_any_element();
        }
        let colors = self.colors();
        let active = self.git.active.as_deref() == Some(project.path.as_str());
        let enabled = state.is_some_and(Status::needs_action);
        let project = project.clone();
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .mt_3()
            .pt_3()
            .border_t_1()
            .border_color(rgb(colors.line))
            .flex_wrap()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        text(
                            format!(
                                "+{}{}",
                                state.map_or(0, |state| state.added),
                                if state.is_some_and(|state| state.partial_stats) {
                                    "*"
                                } else {
                                    ""
                                }
                            ),
                            12.,
                            0x23834b,
                        )
                        .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        text(
                            format!("-{}", state.map_or(0, |state| state.removed)),
                            12.,
                            0xc44b4b,
                        )
                        .font_weight(FontWeight::SEMIBOLD),
                    ),
            )
            .child(
                Button::new(SharedString::from(format!("git-publish-{}", project.id)))
                    .icon(if active {
                        icon(IconName::LoaderCircle, 18.)
                    } else {
                        icon(ActionIcon::Git, 18.)
                    })
                    .label(if active {
                        tr("正在提交并推送…")
                    } else {
                        tr("提交并推送")
                    })
                    .h(px(36.))
                    .px_3()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(rgb(if enabled {
                        colors.accent_border
                    } else {
                        colors.line
                    }))
                    .bg(rgb(if enabled { colors.soft } else { colors.surface }))
                    .text_color(rgb(if enabled { colors.accent } else { colors.muted }))
                    .disabled(!enabled || self.git.active.is_some())
                    .tooltip(if state.is_none() {
                        tr("正在检测 Git 状态…")
                    } else if enabled {
                        tr("查看改动并提交推送当前分支")
                    } else {
                        tr("没有待提交或待推送的改动")
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_git(project.clone(), window, cx)
                    })),
            )
            .into_any_element()
    }

    pub(super) fn git_modal(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(project) = &self.git.dialog else {
            return div().into_any_element();
        };
        let colors = self.colors();
        let state = self.git.states.get(&project.path);
        let working = self.git.active.is_some();
        let has_changes = state.is_some_and(|state| !state.changes.is_empty());
        let diagnostic = if !self.git.error.is_empty() {
            Some(self.git.error.clone())
        } else {
            state.and_then(|state| state.error.clone())
        };
        let path = project.path.clone();
        let ready = state
            .is_some_and(|state| state.repository && state.error.is_none() && state.needs_action());
        let panel = div()
            .w(px((f32::from(window.viewport_size().width) - 80.).min(660.)))
            .max_h(px(f32::from(window.viewport_size().height)
                - TITLE_BAR_HEIGHT
                - 64.))
            .p_6()
            .rounded(px(16.))
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
                        text(tr("提交并推送"), 22., colors.ink).font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        Button::new("close-git")
                            .ghost()
                            .icon(IconName::Close)
                            .disabled(working)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_git(window, cx)),
                            ),
                    ),
            )
            .child(text(project.name.clone(), 16., colors.ink).font_weight(FontWeight::SEMIBOLD))
            .child(text(
                format!(
                    "{}  ·  {}",
                    state.map_or("", |state| state.branch.as_str()),
                    state
                        .filter(|state| !state.upstream.is_empty())
                        .map_or(tr("尚未设置上游分支"), |state| state
                            .upstream
                            .as_str())
                ),
                12.,
                colors.muted,
            ))
            .child(text(
                tr("提交所有未忽略的改动，并推送当前分支。"),
                12.,
                colors.muted,
            ))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        text(
                            format!(
                                "+{}{}",
                                state.map_or(0, |state| state.added),
                                if state.is_some_and(|state| state.partial_stats) {
                                    "*"
                                } else {
                                    ""
                                }
                            ),
                            14.,
                            0x23834b,
                        )
                        .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(
                        text(
                            format!("-{}", state.map_or(0, |state| state.removed)),
                            14.,
                            0xc44b4b,
                        )
                        .font_weight(FontWeight::SEMIBOLD),
                    )
                    .child(text(
                        format!(
                            "{} {}",
                            state.map_or(0, |state| state.changes.len()),
                            tr("个改动文件")
                        ),
                        12.,
                        colors.muted,
                    )),
            )
            .child(
                div()
                    .id("git-changes")
                    .max_h(px(170.))
                    .overflow_y_scroll()
                    .p_3()
                    .rounded(px(8.))
                    .bg(rgb(colors.panel))
                    .children(
                        state
                            .into_iter()
                            .flat_map(|state| state.changes.iter().take(100))
                            .map(|file| {
                                text(file.clone(), 12., colors.ink)
                                    .font_family("Consolas")
                                    .py_1()
                            }),
                    )
                    .when(state.is_some_and(|state| state.changes.len() > 100), |s| {
                        s.child(text(
                            tr("仅显示前 100 个文件，提交将包含全部改动。"),
                            12.,
                            colors.muted,
                        ))
                    })
                    .when(!has_changes, |s| {
                        s.child(text(
                            tr("没有文件改动，将推送已有的本地提交。"),
                            12.,
                            colors.muted,
                        ))
                    }),
            )
            .when(state.is_some_and(|state| state.partial_stats), |s| {
                s.child(text(
                    tr("* 部分未跟踪文件较大，新增行数未全部统计；提交仍包含全部改动。"),
                    12.,
                    colors.muted,
                ))
            })
            .when(has_changes, |s| {
                s.child(Input::new(&self.git.input).h(px(40.)).disabled(working))
            })
            .when_some(diagnostic, |s, error| {
                s.child(
                    div()
                        .id("git-error")
                        .max_h(px(120.))
                        .overflow_y_scroll()
                        .child(text(
                            error
                                .split(": ")
                                .map(i18n::message)
                                .collect::<Vec<_>>()
                                .join(": "),
                            12.,
                            0xbf4b45,
                        )),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("refresh-git")
                            .ghost()
                            .icon(ActionIcon::Refresh)
                            .label(tr("刷新"))
                            .disabled(working)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.git.error.clear();
                                this.git.monitor.check(&path, true);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("cancel-git")
                            .ghost()
                            .label(tr("取消"))
                            .disabled(working)
                            .on_click(
                                cx.listener(|this, _, window, cx| this.close_git(window, cx)),
                            ),
                    )
                    .child(
                        Button::new("confirm-git")
                            .primary()
                            .icon(ActionIcon::Git)
                            .label(if working {
                                tr("正在提交并推送…")
                            } else {
                                tr("提交并推送")
                            })
                            .disabled(working || !ready)
                            .on_click(cx.listener(|this, _, _, cx| this.submit_git(cx))),
                    ),
            );
        self.overlay(panel).into_any_element()
    }
}
