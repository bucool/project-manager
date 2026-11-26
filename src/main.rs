#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]
mod assets;
mod backend;
mod dev_servers;
mod i18n;
mod inspiration;
#[cfg(windows)]
mod media_player;
mod media_viewer;
mod notes;
mod preview_cache;
mod project_git;
#[cfg(windows)]
mod single_instance;
mod smooth_scroll;
mod startup;
mod storage;
mod theme;
#[cfg(windows)]
mod tray;
mod ui;
#[cfg(windows)]
mod webview_capture;
use gpui::*;
use gpui_component::{Root, Theme, ThemeMode};
use ui::{CloseOverlay, FocusSearch, Workbench};
fn main() {
    #[cfg(windows)]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--project-listeners")) {
        let result = (|| -> anyhow::Result<()> {
            let pid = std::env::args()
                .nth(2)
                .ok_or_else(|| anyhow::anyhow!("Missing project PID"))?
                .parse()?;
            let expected = std::env::args()
                .nth(3)
                .map(|value| value.parse())
                .transpose()?;
            println!("{}", dev_servers::project_listeners(pid, expected)?);
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--capture-webview2")) {
        if let Err(error) = webview_capture::run() {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(windows)]
    let Some(instance) = single_instance::SingleInstance::acquire()
        .expect("Unable to initialize single-instance handling")
    else {
        return;
    };
    i18n::set_language(storage::load_language());
    let (backend, events) = backend::Backend::start();
    Application::new()
        .with_assets(assets::Assets)
        .run(move |cx| {
            #[cfg(windows)]
            cx.set_global(instance);
            gpui_component::init(cx);
            gpui_component::set_locale(if i18n::english() { "en" } else { "zh-CN" });
            Theme::change(ThemeMode::Light, None, cx);
            theme::apply("default", cx);
            cx.bind_keys([
                KeyBinding::new("escape", CloseOverlay, Some("Workbench")),
                KeyBinding::new("ctrl-f", FocusSearch, Some("Workbench")),
            ]);
            let cleanup = backend.clone();
            cx.on_app_quit(move |_cx| {
                #[cfg(windows)]
                tray::shutdown(_cx);
                crate::notes::flush(_cx);
                crate::inspiration::flush(_cx);
                cleanup.shutdown();
                async {}
            })
            .detach();
            cx.on_window_closed(|cx| {
                if cx.windows().is_empty() {
                    cx.quit();
                }
            })
            .detach();
            let bounds = Bounds::centered(None, size(px(1440.), px(940.)), cx);
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    window_min_size: Some(size(px(1040.), px(740.))),
                    titlebar: Some(TitlebarOptions {
                        title: Some("Project Manager".into()),
                        appears_transparent: true,
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    #[cfg(windows)]
                    {
                        tray::init(window, cx);
                        single_instance::SingleInstance::connect(window, cx)
                            .expect("Unable to listen for application activation");
                    }
                    let view = cx.new(|cx| Workbench::new(backend, events, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )
            .expect("Unable to create native window");
            cx.activate(true);
        });
}
