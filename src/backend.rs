use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::{
    fs,
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default)]
    pub root: String,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default)]
    pub auto_capture: bool,
    #[serde(default)]
    pub game_engines_only: bool,
    #[serde(default)]
    pub close_to_tray: bool,
    #[serde(default)]
    pub language: crate::i18n::Language,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default)]
    pub sort_order: SortOrder,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum SortOrder {
    Created,
    Name,
    #[default]
    #[serde(other)]
    Modified,
}

fn default_theme() -> String {
    "default".into()
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Classification {
    #[serde(default)]
    pub game_engines: Vec<String>,
    #[serde(default)]
    pub frameworks: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default)]
    pub root: String,
    pub engine: String,
    #[serde(default)]
    pub classification: Option<Classification>,
    pub modified: f64,
    #[serde(default)]
    pub created_at: Option<f64>,
    pub favorite: bool,
    pub status: String,
    pub url: Option<String>,
    #[serde(default)]
    pub lan_urls: Vec<String>,
    #[serde(default)]
    pub lan_error: Option<String>,
    pub preview: Option<String>,
    pub captured_at: Option<u64>,
    pub capture: String,
    pub capture_error: Option<String>,
    pub error: Option<String>,
    pub logs: String,
    pub dependencies_installed: bool,
    #[serde(default)]
    pub install_state: String,
    #[serde(default)]
    pub install_error: Option<String>,
}

impl Project {
    pub fn categories(&self, games_only: bool) -> Vec<&str> {
        if let Some(classification) = &self.classification {
            let mut labels: Vec<&str> = classification
                .game_engines
                .iter()
                .map(String::as_str)
                .collect();
            if !games_only {
                labels.extend(classification.frameworks.iter().map(String::as_str));
            }
            if labels.is_empty() {
                labels.push("Web");
            }
            labels
        } else if !games_only
            || matches!(
                self.engine.as_str(),
                "Phaser" | "Babylon.js" | "Cocos" | "LayaAir" | "PixiJS" | "Three.js" | "Galacean"
            )
        {
            vec![self.engine.as_str()]
        } else {
            vec!["Web"]
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub settings: Settings,
    pub cache_path: String,
    pub queue_length: usize,
    pub projects: Vec<Project>,
    #[serde(skip)]
    pub service_active: bool,
}

impl Snapshot {
    fn normalize_roots(&mut self) {
        // Old caches had one root and no per-project directory membership.
        if self.settings.roots.is_empty() && !self.settings.root.is_empty() {
            self.settings.roots.push(self.settings.root.clone());
        }
        for project in &mut self.projects {
            project.engine = project.categories(self.settings.game_engines_only)[0].to_owned();
            if project.root.is_empty() {
                project.root = project
                    .path
                    .rsplit_once(['\\', '/'])
                    .map(|(parent, _)| parent.to_owned())
                    .unwrap_or_else(|| self.settings.root.clone());
            }
        }
    }

    fn needs_service(&self) -> bool {
        self.queue_length > 0
            || self.projects.iter().any(|p| {
                p.install_state == "installing"
                    || matches!(p.status.as_str(), "starting" | "running" | "stopping")
            })
    }

    pub(crate) fn cached(&self) -> Self {
        let mut snapshot = self.clone();
        snapshot.normalize_roots();
        snapshot.service_active = false;
        snapshot.queue_length = 0;
        for project in &mut snapshot.projects {
            project.status = "idle".into();
            project.url = None;
            project.lan_urls.clear();
            project.lan_error = None;
            if project.install_state == "installing" {
                project.install_state = "error".into();
                project.install_error = Some("上次依赖安装已中断，请重试。".into());
                project.dependencies_installed = false;
            }
            if matches!(project.capture.as_str(), "queued" | "capturing") {
                project.capture = "idle".into();
            }
        }
        snapshot
    }
}

pub struct Request {
    pub operations: Vec<(String, Value)>,
    pub launch: Option<String>,
    pub message: String,
}
pub enum Event {
    Storage {
        directory: Option<String>,
        previous: Option<String>,
        error: Option<String>,
    },
    Snapshot(Snapshot),
    Completed(Snapshot, String),
    Error(String, bool),
}

#[derive(Clone)]
pub struct Backend {
    pub tx: mpsc::Sender<Request>,
    child: Arc<Mutex<Option<Child>>>,
    closing: Arc<AtomicBool>,
    root: Arc<PathBuf>,
    data_dir: Arc<PathBuf>,
}

fn hidden(command: &mut Command) -> &mut Command {
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command
}

pub fn workspace() -> PathBuf {
    if let Some(root) = std::env::var_os("GPM_WORKSPACE") {
        return PathBuf::from(root);
    }
    if let Ok(exe) = std::env::current_exe() {
        for root in exe.ancestors().skip(1).take(5) {
            if root.join("runtime/server.mjs").is_file() {
                return root.to_owned();
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn request(
    agent: &ureq::Agent,
    base: &str,
    endpoint: &str,
    value: Option<Value>,
) -> Result<Snapshot> {
    let address = format!("{base}/api/{endpoint}");
    let response = match value {
        Some(value) => agent.post(&address).send_json(value),
        None => agent.get(&address).call(),
    };
    match response {
        Ok(response) => {
            let mut snapshot: Snapshot = response.into_json()?;
            snapshot.normalize_roots();
            Ok(snapshot)
        }
        Err(ureq::Error::Status(_, response)) => {
            let error: Value = response.into_json().unwrap_or_default();
            bail!("{}", error["error"].as_str().unwrap_or("操作失败，请重试"));
        }
        Err(error) => Err(error.into()),
    }
}

impl Backend {
    pub fn start() -> (Self, async_channel::Receiver<Event>) {
        let root = workspace();
        let data_dir = std::env::var_os("GPM_DATA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| root.join(".data"));
        Self::start_worker(root, data_dir, true)
    }

    #[cfg(test)]
    fn start_in(root: PathBuf, data_dir: PathBuf) -> (Self, async_channel::Receiver<Event>) {
        Self::start_worker(root, data_dir, false)
    }

    fn start_worker(
        root: PathBuf,
        data_dir: PathBuf,
        use_locator: bool,
    ) -> (Self, async_channel::Receiver<Event>) {
        let (tx, rx) = mpsc::channel::<Request>();
        let (events, receiver) = async_channel::unbounded();
        let backend = Self {
            tx,
            child: Arc::new(Mutex::new(None)),
            closing: Arc::new(AtomicBool::new(false)),
            root: Arc::new(root),
            data_dir: Arc::new(data_dir),
        };
        let mut worker = backend.clone();
        thread::spawn(move || {
            let agent = ureq::AgentBuilder::new()
                .timeout_connect(Duration::from_secs(3))
                .timeout_read(Duration::from_secs(75))
                .build();
            let mut base = None;
            let mut configured = !use_locator;
            let mut migration_source = worker
                .data_dir
                .join("settings.json")
                .is_file()
                .then(|| worker.data_dir.as_ref().clone());
            if use_locator {
                let initial = crate::storage::load_location().and_then(|location| {
                    location
                        .map(|path| {
                            crate::storage::load_snapshot(&path).map(|snapshot| (path, snapshot))
                        })
                        .transpose()
                });
                match initial {
                    Ok(Some((path, snapshot))) => {
                        worker.data_dir = Arc::new(path);
                        configured = true;
                        let _ = events.send_blocking(Event::Storage {
                            directory: Some(crate::storage::display_path(&worker.data_dir)),
                            previous: None,
                            error: None,
                        });
                        let _ = events.send_blocking(Event::Snapshot(snapshot));
                    }
                    other => {
                        let error = other.err().map(|error| format!("{error:#}"));
                        if error.is_some() {
                            migration_source = None;
                        }
                        let _ = events.send_blocking(Event::Storage {
                            directory: None,
                            previous: migration_source
                                .as_deref()
                                .map(crate::storage::display_path),
                            error,
                        });
                    }
                }
            } else if let Some(snapshot) = worker.read_cache() {
                let _ = events.send_blocking(Event::Snapshot(snapshot));
            } else {
                let initial = (|| -> Result<Snapshot> {
                    base = Some(worker.connect()?);
                    let address = base.as_deref().unwrap();
                    let mut snapshot = request(&agent, address, "projects", None)?;
                    if snapshot.settings.auto_capture
                        && snapshot.projects.iter().any(|p| {
                            p.preview.is_none()
                                && p.capture_error.is_none()
                                && p.dependencies_installed
                        })
                    {
                        snapshot =
                            request(&agent, address, "capture-all", Some(serde_json::json!({})))?;
                    }
                    worker.settle(&mut base, snapshot)
                })();
                match initial {
                    Ok(snapshot) => {
                        let _ = events.send_blocking(Event::Snapshot(snapshot));
                    }
                    Err(error) => {
                        if worker.stop_owned_service().is_ok() {
                            base = None;
                        }
                        let _ = events
                            .send_blocking(Event::Error(format!("无法加载项目：{error:#}"), true));
                    }
                }
            }
            while !worker.closing.load(Ordering::Relaxed) {
                let command = if base.is_some() {
                    rx.recv_timeout(Duration::from_millis(1500))
                } else {
                    rx.recv().map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                if worker.closing.load(Ordering::Relaxed) {
                    break;
                }
                match command {
                    Ok(command) => {
                        if command.operations.len() == 1 && command.operations[0].0 == "storage" {
                            let result = (|| -> Result<Snapshot> {
                                if base.is_some() {
                                    bail!(
                                        "请先停止管理器启动的项目，并等待扫描、安装和画面获取完成，再更换数据目录。"
                                    );
                                }
                                let path = command.operations[0].1["path"]
                                    .as_str()
                                    .context("请选择数据目录")?;
                                let source = if configured {
                                    Some(worker.data_dir.as_path())
                                } else {
                                    migration_source.as_deref()
                                };
                                let (path, snapshot) =
                                    crate::storage::configure(path, source, !configured)?;
                                worker.data_dir = Arc::new(path);
                                configured = true;
                                let _ = events.send_blocking(Event::Storage {
                                    directory: Some(crate::storage::display_path(&worker.data_dir)),
                                    previous: None,
                                    error: None,
                                });
                                Ok(snapshot)
                            })();
                            let event = match result {
                                Ok(snapshot) => Event::Completed(snapshot, command.message),
                                Err(error) => Event::Error(format!("{error:#}"), false),
                            };
                            let _ = events.send_blocking(event);
                            continue;
                        }
                        let result = (|| -> Result<Snapshot> {
                            if !configured {
                                bail!("请先设置数据存储目录");
                            }
                            if base.is_none()
                                && command.operations.len() == 1
                                && command.operations[0].0 == "settings"
                                && command.launch.is_none()
                            {
                                return worker.update_cached_settings(&command.operations[0].1);
                            }
                            if base.is_none() {
                                base = Some(worker.connect()?);
                            }
                            let address = base.as_deref().unwrap();
                            for (endpoint, value) in command.operations {
                                request(&agent, address, &endpoint, Some(value))?;
                            }
                            let snapshot = request(&agent, address, "projects", None)?;
                            if let Some(id) = command.launch {
                                if let Some(url) = snapshot
                                    .projects
                                    .iter()
                                    .find(|p| p.id == id)
                                    .and_then(|p| p.url.as_ref())
                                {
                                    open(url)?;
                                }
                            }
                            worker.settle(&mut base, snapshot)
                        })();
                        match result {
                            Ok(snapshot) => {
                                let _ = events
                                    .send_blocking(Event::Completed(snapshot, command.message));
                            }
                            Err(error) => {
                                // Failed operations must also release an otherwise idle service.
                                worker.refresh(&agent, &mut base, &events);
                                let _ =
                                    events.send_blocking(Event::Error(format!("{error:#}"), false));
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        worker.refresh(&agent, &mut base, &events);
                    }
                    Err(_) => break,
                }
            }
            let _ = worker.stop_owned_service();
        });
        (backend, receiver)
    }

    fn connect(&self) -> Result<String> {
        // A private worker can be released when idle, without terminating a user's web workbench.
        let root = self.root.as_path();
        let log_dir = self.data_dir.as_path();
        fs::create_dir_all(&log_dir)?;
        let log = fs::File::create(log_dir.join("native-service.log"))?;
        let mut owner = self.child.lock().unwrap();
        if self.closing.load(Ordering::Relaxed) {
            bail!("工作台已关闭");
        }
        let bundled_node = root.join("runtime/node/node.exe");
        let executable = if bundled_node.is_file() {
            bundled_node.clone()
        } else {
            PathBuf::from("node")
        };
        // Cargo's test harness cannot enter the native capture helper's main().
        let capture_executable = if cfg!(test) {
            root.join("target/release/project-manager.exe")
        } else {
            std::env::current_exe()?
        };
        let mut command = Command::new(executable);
        command
            .arg(root.join("runtime/server.mjs"))
            .current_dir(root)
            .env("PORT", "0")
            .env("GPM_DATA_DIR", self.data_dir.as_path())
            .env("GPM_EXECUTABLE", capture_executable)
            .stdout(Stdio::piped())
            .stderr(log);
        if bundled_node.is_file() {
            let mut paths = vec![root.join("runtime/node")];
            if let Some(path) = std::env::var_os("PATH") {
                paths.extend(std::env::split_paths(&path));
            }
            command.env("PATH", std::env::join_paths(paths)?);
        }
        let mut child = hidden(&mut command)
            .spawn()
            .context("未找到 Node.js，请安装 Node.js 20 或更新版本")?;
        let output = child.stdout.take().context("未能读取启动输出")?;
        *owner = Some(child);
        drop(owner);
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(output).lines().map_while(Result::ok) {
                if let Some(index) = line.find("http://127.0.0.1:") {
                    let _ = tx.send(line[index..].trim().to_string());
                }
            }
        });
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(base) => Ok(base),
            Err(_) => {
                self.stop_owned_service()?;
                bail!(
                    "本地服务未能就绪，请查看 {}",
                    crate::storage::display_path(&self.data_dir.join("native-service.log"))
                );
            }
        }
    }

    pub fn shutdown(&self) {
        self.closing.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Request {
            operations: vec![],
            launch: None,
            message: String::new(),
        });
        let _ = self.stop_owned_service();
    }

    fn stop_owned_service(&self) -> Result<()> {
        let mut owner = self.child.lock().unwrap();
        if let Some(child) = owner.as_mut() {
            if child.try_wait()?.is_some() {
                *owner = None;
                return Ok(());
            }
            #[cfg(windows)]
            {
                let output = hidden(Command::new("taskkill").args([
                    "/PID",
                    &child.id().to_string(),
                    "/T",
                    "/F",
                ]))
                .output()
                .context("无法关闭后台服务")?;
                if !output.status.success() && child.try_wait()?.is_none() {
                    bail!(
                        "关闭后台服务失败：{}",
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
            }
            #[cfg(not(windows))]
            {
                child.kill()?;
            }
            child.wait()?;
            *owner = None;
        }
        Ok(())
    }

    fn read_cache(&self) -> Option<Snapshot> {
        let bytes = fs::read(self.data_dir.join("native-snapshot.json")).ok()?;
        let mut snapshot: Snapshot = serde_json::from_slice(&bytes).ok()?;
        snapshot.cache_path = crate::storage::display_path(&self.data_dir.join("previews"));
        let mut upgraded = false;
        // Upgrade old caches on the backend thread, without starting the capture service.
        for project in &mut snapshot.projects {
            if project.created_at.is_none() {
                project.created_at = fs::metadata(&project.path)
                    .and_then(|metadata| metadata.created())
                    .ok()
                    .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|elapsed| elapsed.as_secs_f64() * 1000.);
                upgraded |= project.created_at.is_some();
            }
        }
        if upgraded {
            let _ = self.save_cache(&snapshot);
        }
        Some(snapshot.cached())
    }

    fn save_cache(&self, snapshot: &Snapshot) -> Result<()> {
        fs::create_dir_all(self.data_dir.as_path())?;
        let file = self.data_dir.join("native-snapshot.json");
        let bytes = serde_json::to_vec_pretty(&snapshot.cached())?;
        if fs::read(&file).ok().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, file).context("无法保存项目缓存")?;
        Ok(())
    }

    fn update_cached_settings(&self, value: &Value) -> Result<Snapshot> {
        let mut snapshot = self.read_cache().context("项目缓存不可用，请重新扫描")?;
        let file = self.data_dir.join("settings.json");
        let mut settings: Value = serde_json::from_slice(&fs::read(&file)?)?;
        if let Some(id) = value["favorite"].as_str() {
            let project = snapshot
                .projects
                .iter_mut()
                .find(|p| p.id == id)
                .context("项目不存在，请重新扫描")?;
            let mut favorites: Vec<String> =
                serde_json::from_value(settings["favorites"].clone()).unwrap_or_default();
            project.favorite = !favorites.iter().any(|favorite| favorite == id);
            if project.favorite {
                favorites.push(id.into());
            } else {
                favorites.retain(|favorite| favorite != id);
            }
            settings["favorites"] = serde_json::to_value(favorites)?;
        }
        if let Some(auto) = value["autoCapture"].as_bool() {
            snapshot.settings.auto_capture = auto;
            settings["autoCapture"] = Value::Bool(auto);
        }
        if let Some(games_only) = value["gameEnginesOnly"].as_bool() {
            snapshot.settings.game_engines_only = games_only;
            settings["gameEnginesOnly"] = Value::Bool(games_only);
            snapshot.normalize_roots();
        }
        if let Some(close_to_tray) = value["closeToTray"].as_bool() {
            snapshot.settings.close_to_tray = close_to_tray;
            settings["closeToTray"] = Value::Bool(close_to_tray);
        }
        if let Some(language) = value.get("language") {
            snapshot.settings.language = serde_json::from_value(language.clone())?;
            settings["language"] = serde_json::to_value(snapshot.settings.language)?;
        }
        if let Some(theme) = value["theme"].as_str() {
            let theme = crate::theme::find(theme).id;
            snapshot.settings.theme = theme.into();
            settings["theme"] = Value::String(theme.into());
        }
        if let Some(order) = value.get("sortOrder") {
            snapshot.settings.sort_order = serde_json::from_value(order.clone())?;
            settings["sortOrder"] = serde_json::to_value(snapshot.settings.sort_order)?;
        }
        let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
        fs::write(&temporary, serde_json::to_vec_pretty(&settings)?)?;
        fs::rename(temporary, file)?;
        self.save_cache(&snapshot)?;
        Ok(snapshot)
    }

    fn settle(&self, base: &mut Option<String>, mut snapshot: Snapshot) -> Result<Snapshot> {
        let saved = self.save_cache(&snapshot);
        if !snapshot.needs_service() {
            self.stop_owned_service()?;
            *base = None;
        }
        snapshot.service_active = base.is_some();
        saved?;
        Ok(snapshot)
    }

    fn refresh(
        &self,
        agent: &ureq::Agent,
        base: &mut Option<String>,
        events: &async_channel::Sender<Event>,
    ) {
        let Some(address) = base.as_deref() else {
            return;
        };
        match request(agent, address, "projects", None) {
            Ok(snapshot) => match self.settle(base, snapshot) {
                Ok(snapshot) => {
                    let _ = events.send_blocking(Event::Snapshot(snapshot));
                }
                Err(error) => {
                    let _ = events.send_blocking(Event::Error(format!("{error:#}"), false));
                }
            },
            Err(error) => {
                if self.stop_owned_service().is_ok() {
                    *base = None;
                }
                if let Some(snapshot) = self.read_cache() {
                    let _ = events.send_blocking(Event::Snapshot(snapshot));
                }
                let _ = events.send_blocking(Event::Error(
                    format!("后台服务中断，已保留缓存：{error}"),
                    true,
                ));
            }
        }
    }
}

pub fn open(target: &str) -> Result<()> {
    #[cfg(windows)]
    hidden(Command::new("explorer.exe").arg(target)).spawn()?;
    #[cfg(not(windows))]
    Command::new("xdg-open").arg(target).spawn()?;
    Ok(())
}

#[cfg(test)]
pub fn preview_path(cache: &str, project: &Project) -> Option<PathBuf> {
    if project.preview.is_none()
        || project.id.len() != 16
        || !project.id.chars().all(|c| c.is_ascii_hexdigit())
    {
        return None;
    }
    let path = PathBuf::from(cache).join(format!("{}.jpg", project.id));
    path.is_file().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    fn fixture_server(status: &str, body: &str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            // Drain headers and body before closing, otherwise Windows may reset the TCP connection.
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap();
                }
            }
            reader.read_exact(&mut vec![0; length]).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
        });
        address
    }

    #[test]
    fn reads_existing_service_protocol_without_losing_unicode_or_preview_errors() {
        let payload = serde_json::json!({
            "settings": {"root": "E:\\游戏项目", "autoCapture": true},
            "cachePath": "E:\\manager\\.data\\previews", "queueLength": 2,
            "projects": [{"id":"0123456789abcdef", "name":"与过去同行", "path":"E:\\游戏项目\\与过去同行", "engine":"PixiJS", "modified":123.5, "favorite":true, "status":"idle", "url":null, "preview":null, "capturedAt":null, "capture":"error", "captureError":"启动失败", "error":null, "logs":"日志", "dependenciesInstalled":true}]
        });
        let base = fixture_server("200 OK", &payload.to_string());
        let snapshot = request(&ureq::agent(), &base, "projects", None).unwrap();
        assert_eq!(snapshot.projects[0].name, "与过去同行");
        assert_eq!(
            snapshot.projects[0].capture_error.as_deref(),
            Some("启动失败")
        );
        assert_eq!(snapshot.queue_length, 2);
        assert!(snapshot.projects[0].favorite);
        assert_eq!(snapshot.settings.roots, vec!["E:\\游戏项目"]);
        assert_eq!(snapshot.projects[0].root, "E:\\游戏项目");
    }

    #[test]
    fn multi_directory_cache_restores_without_starting_a_service() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let first = fixture.0.join("games").to_string_lossy().into_owned();
        let second = fixture.0.join("other-games").to_string_lossy().into_owned();
        let snapshot = Snapshot {
            settings: Settings {
                root: first.clone(),
                roots: vec![first.clone(), second.clone()],
                auto_capture: false,
                ..Settings::default()
            },
            projects: [first, second]
                .into_iter()
                .enumerate()
                .map(|(i, root)| Project {
                    id: format!("{i:016x}"),
                    name: "同名游戏".into(),
                    path: format!("{root}/同名游戏"),
                    root,
                    favorite: i == 1,
                    preview: Some(format!("/previews/{i:016x}.jpg")),
                    captured_at: Some(123),
                    status: "running".into(),
                    ..Project::default()
                })
                .collect(),
            ..Snapshot::default()
        };
        backend.save_cache(&snapshot).unwrap();
        let (worker, events) = Backend::start_in(workspace(), fixture.0.join("data"));
        let _guard = StopOnDrop(worker.clone());
        let cached = next_snapshot(&events, |_| true);
        assert_eq!(cached.settings.roots, snapshot.settings.roots);
        assert_eq!(cached.projects, snapshot.cached().projects);
        assert!(!cached.service_active);
        assert!(worker.child.lock().unwrap().is_none());
        assert!(!fixture.0.join("data/native-service.log").exists());
        worker.shutdown();

        let empty = Snapshot {
            settings: Settings {
                auto_capture: false,
                ..Settings::default()
            },
            ..Snapshot::default()
        };
        backend.save_cache(&empty).unwrap();
        let (worker, events) = Backend::start_in(workspace(), fixture.0.join("data"));
        let _guard = StopOnDrop(worker.clone());
        assert!(next_snapshot(&events, |_| true).projects.is_empty());
        assert!(worker.child.lock().unwrap().is_none());
        assert!(!fixture.0.join("data/native-service.log").exists());
    }

    #[test]
    fn service_errors_are_reported_as_user_readable_messages() {
        let base = fixture_server("400 Bad Request", r#"{"error":"项目依赖未安装"}"#);
        let error = request(&ureq::agent(), &base, "scan", Some(json_value())).unwrap_err();
        assert_eq!(error.to_string(), "项目依赖未安装");
    }

    fn json_value() -> Value {
        serde_json::json!({})
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            // Windows clock ticks can coincide across parallel test threads.
            static NEXT_FIXTURE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(0);
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let directory = std::env::temp_dir().join(format!(
                "gpm-native-test-{}-{stamp}-{sequence}",
                std::process::id()
            ));
            let game = directory.join("games/画面缓存");
            fs::create_dir_all(&game).unwrap();
            fs::create_dir_all(directory.join("data")).unwrap();
            fs::write(game.join("index.html"), "<!doctype html><canvas width='800' height='500'></canvas><script>const c=document.querySelector('canvas').getContext('2d');c.fillStyle='#a1c787';c.fillRect(0,0,800,500);</script>").unwrap();
            fs::write(directory.join("data/settings.json"), serde_json::to_vec(&serde_json::json!({"root":directory.join("games"),"favorites":[],"autoCapture":false})).unwrap()).unwrap();
            Self(directory)
        }

        fn backend(&self) -> Backend {
            let (tx, _) = mpsc::channel();
            Backend {
                tx,
                child: Arc::new(Mutex::new(None)),
                closing: Arc::new(AtomicBool::new(false)),
                root: Arc::new(workspace()),
                data_dir: Arc::new(self.0.join("data")),
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // Only remove this test's explicitly created temporary directory.
            if self.0.parent() == Some(std::env::temp_dir().as_path())
                && self
                    .0
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("gpm-native-test-")
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }
    struct StopOnDrop(Backend);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.shutdown();
        }
    }
    fn next_snapshot(
        events: &async_channel::Receiver<Event>,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        let deadline = std::time::Instant::now() + Duration::from_secs(45);
        loop {
            match next_event(
                events,
                deadline.saturating_duration_since(std::time::Instant::now()),
            ) {
                Event::Snapshot(snapshot) | Event::Completed(snapshot, _)
                    if predicate(&snapshot) =>
                {
                    return snapshot;
                }
                Event::Error(error, _) => panic!("{error}"),
                _ => {}
            }
        }
    }
    fn next_event(events: &async_channel::Receiver<Event>, timeout: Duration) -> Event {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            match events.try_recv() {
                Ok(event) => return event,
                Err(async_channel::TryRecvError::Empty) if std::time::Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10))
                }
                error => panic!(
                    "Timed out waiting for native worker: {}",
                    error.err().unwrap()
                ),
            }
        }
    }
    fn send(backend: &Backend, endpoint: &str, value: Value) {
        backend
            .tx
            .send(Request {
                operations: vec![(endpoint.into(), value)],
                launch: None,
                message: endpoint.into(),
            })
            .unwrap();
    }
    fn tcp_address(url: &str) -> &str {
        url.strip_prefix("http://").unwrap().trim_end_matches('/')
    }

    #[test]
    fn idle_mode_closes_the_backend_port_and_restarts_only_for_work() {
        let fixture = Fixture::new();
        let backend = fixture.backend();
        let _guard = StopOnDrop(backend.clone());
        let address = backend.connect().unwrap();
        assert!(TcpStream::connect(tcp_address(&address)).is_ok());
        let snapshot = request(&ureq::agent(), &address, "projects", None).unwrap();
        let mut base = Some(address.clone());
        let idle = backend.settle(&mut base, snapshot).unwrap();
        assert!(base.is_none());
        assert!(!idle.service_active);
        assert!(TcpStream::connect(tcp_address(&address)).is_err());
        assert_eq!(backend.read_cache().unwrap().projects[0].name, "画面缓存");

        let log = fixture.0.join("data/native-service.log");
        fs::remove_file(&log).unwrap();
        let (worker, events) = Backend::start_in(workspace(), fixture.0.join("data"));
        let _worker_guard = StopOnDrop(worker.clone());
        let cached = next_snapshot(&events, |s| !s.service_active);
        assert!(worker.child.lock().unwrap().is_none());
        assert!(!log.exists(), "A cached startup must not spawn Node");
        let id = &cached.projects[0].id;

        send(&worker, "settings", serde_json::json!({"favorite":id}));
        next_snapshot(&events, |s| s.projects[0].favorite);
        assert!(
            !log.exists(),
            "Favorites must work without a background service"
        );

        send(&worker, &format!("projects/{id}/capture"), json_value());
        next_snapshot(&events, |s| s.service_active && s.queue_length > 0);
        assert!(worker.child.lock().unwrap().is_some());
        let finished = next_snapshot(&events, |s| {
            !s.service_active && s.projects[0].preview.is_some()
        });
        assert!(worker.child.lock().unwrap().is_none());
        let image = preview_path(&finished.cache_path, &finished.projects[0]).unwrap();
        assert!(fs::metadata(&image).unwrap().len() > 1000);
        assert_eq!(finished.projects[0].capture, "done");
        assert_eq!(fs::read_dir(&finished.projects[0].path).unwrap().count(), 1);

        send(&worker, &format!("projects/{id}/start"), json_value());
        let playing = next_snapshot(&events, |s| s.projects[0].status == "running");
        assert!(playing.service_active);
        let playing_url = playing.projects[0].url.as_ref().unwrap();
        let stored = worker.read_cache().unwrap();
        assert_eq!(stored.projects[0].status, "idle");
        assert!(stored.projects[0].url.is_none());
        send(&worker, &format!("projects/{id}/stop"), json_value());
        next_snapshot(&events, |s| !s.service_active);
        assert!(TcpStream::connect(tcp_address(playing_url)).is_err());
        assert!(image.is_file());

        send(
            &worker,
            "scan",
            serde_json::json!({"root":fixture.0.join("missing")}),
        );
        next_snapshot(&events, |s| !s.service_active);
        assert!(matches!(
            next_event(&events, Duration::from_secs(5)),
            Event::Error(_, false)
        ));
        assert!(
            worker.child.lock().unwrap().is_none(),
            "A failed scan must also release the service"
        );
        assert!(worker.read_cache().unwrap().projects[0].preview.is_some());

        worker.shutdown();
        fs::remove_file(&log).unwrap();
        let (reopened, reopened_events) = Backend::start_in(workspace(), fixture.0.join("data"));
        let _reopened_guard = StopOnDrop(reopened.clone());
        let restored = next_snapshot(&reopened_events, |s| !s.service_active);
        assert!(preview_path(&restored.cache_path, &restored.projects[0]).is_some());
        assert!(restored.projects[0].favorite);
        assert!(
            !log.exists(),
            "Reopening a completed library must only read the cache"
        );
    }

    #[test]
    fn closing_native_app_terminates_only_its_owned_service() {
        let unrelated = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut child = hidden(Command::new("node").args(["-e", "require('net').createServer().listen(0,'127.0.0.1',function(){console.log(this.address().port)})"]).stdout(Stdio::piped())).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let owned_address = format!("127.0.0.1:{}", line.trim());
        assert!(TcpStream::connect(&owned_address).is_ok());
        let (tx, _) = mpsc::channel();
        let backend = Backend {
            tx,
            child: Arc::new(Mutex::new(Some(child))),
            closing: Arc::new(AtomicBool::new(false)),
            root: Arc::new(workspace()),
            data_dir: Arc::new(workspace().join(".data")),
        };
        backend.shutdown();
        assert!(TcpStream::connect(&owned_address).is_err());
        assert!(TcpStream::connect(unrelated.local_addr().unwrap()).is_ok());
    }
}
