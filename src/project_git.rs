//! Local Git checks and explicit commit/push jobs, off the UI thread.
use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::HashMap,
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub repository: bool,
    pub branch: String,
    pub upstream: String,
    pub changes: Vec<String>,
    pub added: u64,
    pub removed: u64,
    pub ahead: u32,
    pub behind: u32,
    pub initial: bool,
    pub conflicts: bool,
    pub partial_stats: bool,
    pub error: Option<String>,
}

impl Status {
    pub fn needs_action(&self) -> bool {
        self.repository
            && (self.error.is_some()
                || !self.changes.is_empty()
                || self.ahead > 0
                || (self.upstream.is_empty() && !self.initial))
    }
}

pub enum Event {
    Status(String, Status),
    Finished {
        path: String,
        error: Option<Failure>,
    },
}

#[derive(Clone, Copy)]
pub enum FailureStage {
    Commit,
    Push,
}
impl FailureStage {
    pub fn label(self) -> &'static str {
        crate::i18n::tr(match self {
            Self::Commit => "提交失败",
            Self::Push => "推送失败",
        })
    }
}
#[derive(Clone)]
pub struct Failure {
    pub stage: FailureStage,
    pub message: String,
}
enum Job {
    Check(String),
    Publish {
        path: String,
        branch: String,
        message: String,
    },
    Quit,
}

pub struct Monitor {
    tx: mpsc::SyncSender<Job>,
    publish_tx: mpsc::SyncSender<Job>,
    requested: Mutex<HashMap<String, Instant>>,
    stopped: Arc<AtomicBool>,
}
impl Monitor {
    pub fn start() -> (Self, async_channel::Receiver<Event>) {
        let (tx, jobs) = mpsc::sync_channel(128);
        let (publish_tx, publish_jobs) = mpsc::sync_channel(1);
        let (events, receiver) = async_channel::unbounded();
        let stopped = Arc::new(AtomicBool::new(false));
        let cancel = stopped.clone();
        thread::spawn(move || {
            let mut previous = HashMap::new();
            loop {
                if cancel.load(Ordering::Relaxed) {
                    break;
                }
                // User actions take priority over queued background checks.
                let job = match publish_jobs.try_recv() {
                    Ok(job) => job,
                    Err(_) => match jobs.recv_timeout(Duration::from_millis(100)) {
                        Ok(job) => job,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    },
                };
                let path = match job {
                    Job::Quit => break,
                    Job::Check(path) => path,
                    Job::Publish {
                        path,
                        branch,
                        message,
                    } => {
                        let mut stage = FailureStage::Commit;
                        let error =
                            publish(Path::new(&path), &branch, &message, &cancel, &mut stage)
                                .err()
                                .map(|error| Failure {
                                    stage,
                                    message: format!("{error:#}"),
                                });
                        let next = status(Path::new(&path), &cancel);
                        previous.insert(path.clone(), next.clone());
                        if events
                            .send_blocking(Event::Status(path.clone(), next))
                            .is_err()
                        {
                            break;
                        }
                        if events
                            .send_blocking(Event::Finished { path, error })
                            .is_err()
                        {
                            break;
                        }
                        continue;
                    }
                };
                let next = status(Path::new(&path), &cancel);
                if previous.get(&path) != Some(&next) {
                    previous.insert(path.clone(), next.clone());
                    if events.send_blocking(Event::Status(path, next)).is_err() {
                        break;
                    }
                }
            }
        });
        (
            Self {
                tx,
                publish_tx,
                requested: Mutex::new(HashMap::new()),
                stopped,
            },
            receiver,
        )
    }
    pub fn check(&self, path: &str, force: bool) {
        let Ok(mut requested) = self.requested.lock() else {
            return;
        };
        if !force
            && requested
                .get(path)
                .is_some_and(|time| time.elapsed() < Duration::from_secs(10))
        {
            return;
        }
        if self.tx.try_send(Job::Check(path.to_owned())).is_ok() {
            requested.insert(path.to_owned(), Instant::now());
        }
    }
    pub fn publish(&self, path: String, branch: String, message: String) -> bool {
        self.publish_tx
            .try_send(Job::Publish {
                path,
                branch,
                message,
            })
            .is_ok()
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Relaxed);
        let _ = self.tx.try_send(Job::Quit);
    }
}

fn command(root: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .args(["--literal-pathspecs", "-C"])
        .arg(root)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .env("SSH_ASKPASS_REQUIRE", "never")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00004000);
    }
    command
}

fn read_output(reader: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(8 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn run(
    root: &Path,
    args: &[&str],
    input: Option<&str>,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<Output> {
    let mut command = command(root, args);
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    if timeout <= Duration::from_secs(10) {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    let mut child = command
        .spawn()
        .context("Git 无法启动，请确认已安装 Git 并加入 PATH。")?;
    let stdout = child.stdout.take().context("Git stdout unavailable")?;
    let stderr = child.stderr.take().context("Git stderr unavailable")?;
    let out = thread::spawn(move || read_output(stdout));
    let err = thread::spawn(move || read_output(stderr));
    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(input.as_bytes());
        }
    }
    let started = Instant::now();
    let mut interrupted = false;
    let exit = loop {
        if let Some(exit) = child.try_wait()? {
            break exit;
        }
        if cancel.load(Ordering::Relaxed) || started.elapsed() > timeout {
            interrupted = true;
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .creation_flags(0x08000000)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            let _ = child.kill();
            break child.wait()?;
        }
        thread::sleep(Duration::from_millis(25));
    };
    let stdout = out
        .join()
        .map_err(|_| anyhow::anyhow!("Git output reader stopped"))??;
    let stderr = err
        .join()
        .map_err(|_| anyhow::anyhow!("Git output reader stopped"))??;
    ensure!(
        !interrupted,
        "Git 操作超时或已取消，请检查网络和仓库状态后重试。"
    );
    ensure!(
        stdout.len() <= 8 * 1024 * 1024 && stderr.len() <= 8 * 1024 * 1024,
        "Git 输出过大，请在终端中处理此仓库。"
    );
    Ok(Output {
        status: exit,
        stdout,
        stderr,
    })
}
fn checked(
    root: &Path,
    args: &[&str],
    input: Option<&str>,
    timeout: u64,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    let output = run(root, args, input, Duration::from_secs(timeout), cancel)?;
    if !output.status.success() {
        let diagnostic = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        bail!("{}", String::from_utf8_lossy(diagnostic).trim());
    }
    Ok(output.stdout)
}

fn status(root: &Path, cancel: &AtomicBool) -> Status {
    // Only a repository owned by this project; never climb into a parent repo.
    if !root.join(".git").exists() {
        return Status::default();
    }
    match inspect(root, cancel) {
        Ok(status) => status,
        Err(error) => Status {
            repository: true,
            error: Some(format!("{error:#}")),
            ..Default::default()
        },
    }
}

fn inspect(root: &Path, cancel: &AtomicBool) -> Result<Status> {
    let bytes = checked(
        root,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
        None,
        5,
        cancel,
    )?;
    let mut state = Status {
        repository: true,
        ..Default::default()
    };
    let mut untracked = Vec::new();
    let mut files = Vec::new();
    let mut records = bytes.split(|byte| *byte == 0);
    while let Some(record) = records.next() {
        let line = String::from_utf8_lossy(record);
        if let Some(branch) = line.strip_prefix("# branch.head ") {
            state.branch = branch.into();
        } else if line == "# branch.oid (initial)" {
            state.initial = true;
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            state.upstream = upstream.into();
        } else if let Some(counts) = line.strip_prefix("# branch.ab ") {
            for count in counts.split_whitespace() {
                if let Some(value) = count.strip_prefix('+') {
                    state.ahead = value.parse().unwrap_or(0);
                }
                if let Some(value) = count.strip_prefix('-') {
                    state.behind = value.parse().unwrap_or(0);
                }
            }
        } else if let Some(path) = line.strip_prefix("? ") {
            state.changes.push(format!("+ {path}"));
            untracked.push(PathBuf::from(path));
            files.push(PathBuf::from(path));
        } else if matches!(record.first(), Some(b'1' | b'2' | b'u')) {
            let fields: Vec<_> = line
                .splitn(
                    if record[0] == b'u' {
                        11
                    } else if record[0] == b'2' {
                        10
                    } else {
                        9
                    },
                    ' ',
                )
                .collect();
            if let Some(path) = fields.last() {
                state
                    .changes
                    .push(format!("{} {}", fields.get(1).unwrap_or(&""), path));
                files.push(PathBuf::from(path));
            }
            if record[0] == b'2' {
                records.next();
            }
            if record[0] == b'u' {
                state.conflicts = true;
            }
        }
    }
    let mut remaining_bytes = 64 * 1024 * 1024;
    if state.initial {
        for file in files {
            if let Some(lines) = text_lines(&root.join(file), &mut remaining_bytes, cancel)? {
                state.added += lines;
            } else {
                state.partial_stats = true;
            }
        }
    } else if !state.changes.is_empty() {
        let diff = checked(
            root,
            &[
                "diff",
                "--numstat",
                "-z",
                "--no-ext-diff",
                "--no-textconv",
                "HEAD",
                "--",
            ],
            None,
            5,
            cancel,
        )?;
        let mut records = diff.split(|byte| *byte == 0);
        while let Some(record) = records.next() {
            if record.is_empty() {
                continue;
            }
            let mut fields = record.splitn(3, |byte| *byte == b'\t');
            let added = fields.next().unwrap_or_default();
            let removed = fields.next().unwrap_or_default();
            let path = fields.next().unwrap_or_default();
            state.added += String::from_utf8_lossy(added).parse::<u64>().unwrap_or(0);
            state.removed += String::from_utf8_lossy(removed).parse::<u64>().unwrap_or(0);
            if path.is_empty() {
                records.next();
                records.next();
            }
        }
        for file in untracked {
            if let Some(lines) = text_lines(&root.join(file), &mut remaining_bytes, cancel)? {
                state.added += lines;
            } else {
                state.partial_stats = true;
            }
        }
    }
    Ok(state)
}

fn text_lines(path: &Path, remaining: &mut u64, cancel: &AtomicBool) -> Result<Option<u64>> {
    let Ok(mut file) = File::open(path) else {
        return Ok(Some(0));
    };
    let bytes = file.metadata()?.len();
    if bytes > 32 * 1024 * 1024 || bytes > *remaining {
        return Ok(None);
    }
    *remaining -= bytes;
    let mut buffer = [0u8; 32768];
    let mut lines = 0;
    let mut last = None;
    loop {
        ensure!(
            !cancel.load(Ordering::Relaxed),
            "Git 操作超时或已取消，请检查网络和仓库状态后重试。"
        );
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if buffer[..count].contains(&0) {
            return Ok(Some(0));
        }
        lines += buffer[..count]
            .iter()
            .filter(|byte| **byte == b'\n')
            .count() as u64;
        last = buffer.get(count - 1).copied();
    }
    Ok(Some(
        lines + u64::from(last.is_some_and(|byte| byte != b'\n')),
    ))
}

fn config(root: &Path, key: &str, cancel: &AtomicBool) -> Result<Option<String>> {
    let output = run(
        root,
        &["config", "--get", key],
        None,
        Duration::from_secs(5),
        cancel,
    )?;
    if output.status.code() == Some(1) {
        return Ok(None);
    }
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(Some(String::from_utf8(output.stdout)?.trim().to_owned()))
}

fn publish(
    root: &Path,
    expected_branch: &str,
    message: &str,
    cancel: &AtomicBool,
    stage: &mut FailureStage,
) -> Result<()> {
    ensure!(root.join(".git").exists(), "项目文件夹不是 Git 仓库。");
    let top = checked(root, &["rev-parse", "--show-toplevel"], None, 5, cancel)?;
    ensure!(
        std::fs::canonicalize(root)? == std::fs::canonicalize(String::from_utf8(top)?.trim())?,
        "Git 仓库根目录与项目目录不一致，请在终端中处理。"
    );
    let state = inspect(root, cancel)?;
    ensure!(
        state.branch != "(detached)" && !state.branch.is_empty(),
        "当前不在分支上，请先在 Git 中切换到分支。"
    );
    ensure!(
        state.branch == expected_branch,
        "Git 分支已变化，请关闭窗口后重试。"
    );
    ensure!(!state.conflicts, "仓库存在冲突，请先解决冲突后再提交。");
    for name in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "rebase-merge",
        "rebase-apply",
    ] {
        let path = checked(root, &["rev-parse", "--git-path", name], None, 5, cancel)?;
        let path = PathBuf::from(String::from_utf8(path)?.trim());
        ensure!(
            !root.join(path).exists(),
            "仓库正在合并或变基，请先在 Git 中完成操作。"
        );
    }
    *stage = FailureStage::Push;
    let upstream_remote = config(root, &format!("branch.{}.remote", state.branch), cancel)?;
    let remote = config(root, &format!("branch.{}.pushRemote", state.branch), cancel)?
        .or(config(root, "remote.pushDefault", cancel)?)
        .or(upstream_remote.clone())
        .unwrap_or_else(|| "origin".into());
    ensure!(
        remote != "." && !remote.starts_with('-'),
        "请先为当前分支配置远程仓库。"
    );
    checked(
        root,
        &["remote", "get-url", "--push", &remote],
        None,
        5,
        cancel,
    )
    .context("请先配置远程仓库和本机 Git 登录凭据。")?;
    let merge = config(root, &format!("branch.{}.merge", state.branch), cancel)?
        .filter(|_| upstream_remote.as_ref() == Some(&remote))
        .filter(|merge| merge.starts_with("refs/heads/"))
        .unwrap_or_else(|| format!("refs/heads/{}", state.branch));
    let mut committed = false;
    if !state.changes.is_empty() {
        *stage = FailureStage::Commit;
        ensure!(!message.trim().is_empty(), "请填写提交说明。");
        checked(root, &["add", "--all", "--", "."], None, 30, cancel)?;
        checked(root, &["commit", "--file=-"], Some(message), 120, cancel)?;
        committed = true;
    }
    let target = format!("HEAD:{merge}");
    *stage = FailureStage::Push;
    let mut push_args = vec!["push", "--porcelain"];
    if state.upstream.is_empty() {
        push_args.push("--set-upstream");
    }
    push_args.extend(["--", remote.as_str(), target.as_str()]);
    checked(root, &push_args, None, 120, cancel).with_context(|| {
        if committed {
            "提交成功，但推送失败。提交已保留，可重试推送。"
        } else {
            "推送失败，请检查远程仓库、网络或登录凭据。"
        }
    })?;
    Ok(())
}
