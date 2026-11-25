use std::{path::PathBuf, sync::mpsc, thread, time::Duration};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevServer {
    pub pid: u32,
    pub port: u16,
    pub addresses: Vec<String>,
    pub url: String,
    pub name: String,
    pub directory: Option<String>,
    pub kind: String,
    // Exact Windows creation time distinguishes a discovered process from a reused PID.
    pub started: u64,
}

impl DevServer {
    pub fn matches_url(&self, address: &str) -> bool {
        let Ok(address) = url::Url::parse(address) else {
            return false;
        };
        if address.port_or_known_default() != Some(self.port) {
            return false;
        }
        let host = address.host_str().unwrap_or("").trim_matches(['[', ']']);
        self.addresses.iter().any(|bound| {
            bound == host
                || (host == "localhost" && matches!(bound.as_str(), "127.0.0.1" | "::1"))
                || (bound == "0.0.0.0"
                    && (host == "localhost" || host.parse::<std::net::Ipv4Addr>().is_ok()))
                || (bound == "::"
                    && (host == "localhost" || host.parse::<std::net::IpAddr>().is_ok()))
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Discovery {
    pub servers: Vec<DevServer>,
    pub warning: Option<String>,
}

enum Command {
    Watch(bool),
    Refresh,
    Quit,
}

pub struct Monitor(mpsc::Sender<Command>);

impl Monitor {
    pub fn start(workspace: PathBuf) -> (Self, async_channel::Receiver<Discovery>) {
        let (tx, commands) = mpsc::channel();
        let (events, receiver) = async_channel::unbounded();
        thread::spawn(move || {
            let mut watching = false;
            let mut previous = None;
            loop {
                let command = if watching {
                    commands.recv_timeout(Duration::from_secs(5))
                } else {
                    commands
                        .recv()
                        .map_err(|_| mpsc::RecvTimeoutError::Disconnected)
                };
                let requested = match command {
                    Ok(Command::Watch(active)) => {
                        watching = active;
                        if !active {
                            continue;
                        }
                        true
                    }
                    Ok(Command::Refresh) => true,
                    Err(mpsc::RecvTimeoutError::Timeout) => false,
                    Ok(Command::Quit) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let next = discover(&workspace);
                // No UI timer and no redraw when the process/port snapshot is unchanged.
                if requested || previous.as_ref() != Some(&next) {
                    if events.send_blocking(next.clone()).is_err() {
                        break;
                    }
                    previous = Some(next);
                }
            }
        });
        (Self(tx), receiver)
    }

    pub fn watch(&self, active: bool) {
        let _ = self.0.send(Command::Watch(active));
    }

    pub fn refresh(&self) {
        let _ = self.0.send(Command::Refresh);
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        let _ = self.0.send(Command::Quit);
    }
}

pub fn stop(server: &DevServer) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        windows::stop(server)
    }
    #[cfg(not(windows))]
    {
        let _ = server;
        anyhow::bail!(crate::i18n::tr("停止外部开发服务目前仅支持 Windows。"))
    }
}

#[cfg(not(windows))]
fn discover(_: &std::path::Path) -> Discovery {
    Discovery {
        servers: vec![],
        warning: Some("本机开发服务发现目前仅支持 Windows。".into()),
    }
}

#[cfg(windows)]
fn discover(workspace: &std::path::Path) -> Discovery {
    match windows::discover(workspace) {
        Ok(result) => result,
        Err(error) => Discovery {
            servers: vec![],
            warning: Some(format!("无法读取本机端口：{error:#}")),
        },
    }
}

#[cfg(windows)]
pub fn project_listeners(pid: u32, expected: Option<u64>) -> anyhow::Result<serde_json::Value> {
    windows::project_listeners(pid, expected)
}

#[cfg(windows)]
mod windows {
    use super::{DevServer, Discovery};
    use anyhow::{Context, Result, bail};
    use std::{
        collections::{BTreeMap, BTreeSet, HashMap, HashSet},
        mem::{offset_of, size_of},
        net::{Ipv4Addr, Ipv6Addr},
        os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
        path::Path,
        ptr,
    };
    use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
    use windows_sys::Win32::{
        Foundation::{
            CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_PARAMETER, FILETIME, HANDLE,
            INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        NetworkManagement::IpHelper::{
            GetExtendedTcpTable, MIB_TCP6ROW_OWNER_PID, MIB_TCP6TABLE_OWNER_PID,
            MIB_TCPROW_OWNER_PID, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
        },
        Networking::WinSock::{AF_INET, AF_INET6},
        System::{
            Diagnostics::ToolHelp::{
                CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
                TH32CS_SNAPPROCESS,
            },
            Threading::{
                GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
                PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, TerminateProcess, WaitForSingleObject,
            },
        },
    };

    fn creation_time(handle: HANDLE) -> Result<u64> {
        let mut created = FILETIME::default();
        let mut exited = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err(std::io::Error::last_os_error())
                .context(crate::i18n::tr("无法读取开发进程身份"));
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    fn process_identity(pid: u32) -> Result<u64> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error())
                .context(crate::i18n::tr("无法读取开发进程身份"));
        }
        let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
        creation_time(handle.as_raw_handle())
    }

    pub fn stop(server: &DevServer) -> Result<()> {
        if server.pid == 0 || server.pid == std::process::id() || server.started == 0 {
            bail!(crate::i18n::tr("无法确认开发进程身份，请刷新端口后重试。"));
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                server.pid,
            )
        };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_INVALID_PARAMETER as i32) {
                return Ok(()); // The process has already exited.
            }
            return Err(error).context(crate::i18n::tr("无法停止外部开发服务"));
        }
        let process = unsafe { OwnedHandle::from_raw_handle(handle) };
        let handle = process.as_raw_handle();
        if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
            return Ok(());
        }
        if creation_time(handle)? != server.started {
            bail!(crate::i18n::tr("进程已变化，请刷新端口后重试。"));
        }
        if !listeners()?.0.contains_key(&(server.pid, server.port)) {
            return Ok(()); // This server is no longer listening; leave the process alone.
        }
        // Use the validated handle directly; never stop a terminal, launcher or another PID.
        if unsafe { TerminateProcess(handle, 1) } == 0 {
            let error = std::io::Error::last_os_error();
            if unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0 {
                return Err(error).context(crate::i18n::tr("无法停止外部开发服务"));
            }
        }
        match unsafe { WaitForSingleObject(handle, 5000) } {
            WAIT_OBJECT_0 => Ok(()),
            WAIT_TIMEOUT => bail!(crate::i18n::tr("开发进程未退出，请刷新后重试。")),
            _ => Err(std::io::Error::last_os_error())
                .context(crate::i18n::tr("等待开发进程退出失败")),
        }
    }

    struct ProcessEntry {
        parent: u32,
        name: String,
    }

    fn processes() -> Result<HashMap<u32, ProcessEntry>> {
        // Toolhelp only enumerates names/PIDs. Command lines are read later for listener trees.
        unsafe {
            let handle = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if handle == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error()).context("无法读取进程列表");
            }
            let mut entry = PROCESSENTRY32W {
                dwSize: size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = HashMap::new();
            let mut ok = Process32FirstW(handle, &mut entry);
            if ok == 0 {
                let error = std::io::Error::last_os_error();
                CloseHandle(handle);
                return Err(error).context("无法读取进程列表");
            }
            while ok != 0 {
                let len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                found.insert(
                    entry.th32ProcessID,
                    ProcessEntry {
                        parent: entry.th32ParentProcessID,
                        name: String::from_utf16_lossy(&entry.szExeFile[..len]).to_lowercase(),
                    },
                );
                ok = Process32NextW(handle, &mut entry);
            }
            CloseHandle(handle);
            Ok(found)
        }
    }

    fn tcp_table(family: u32) -> Result<Vec<u32>> {
        let mut size = 0;
        unsafe {
            GetExtendedTcpTable(
                ptr::null_mut(),
                &mut size,
                0,
                family,
                TCP_TABLE_OWNER_PID_LISTENER,
                0,
            );
        }
        // The table can grow between calls. A u32 buffer provides the required row alignment.
        for _ in 0..4 {
            let mut buffer = vec![0u32; (size as usize).div_ceil(4).max(1)];
            let result = unsafe {
                GetExtendedTcpTable(
                    buffer.as_mut_ptr().cast(),
                    &mut size,
                    0,
                    family,
                    TCP_TABLE_OWNER_PID_LISTENER,
                    0,
                )
            };
            if result == 0 {
                return Ok(buffer);
            }
            if result != ERROR_INSUFFICIENT_BUFFER {
                bail!("{}", std::io::Error::from_raw_os_error(result as i32));
            }
        }
        bail!("端口列表变化过快，请刷新重试")
    }

    fn listeners() -> Result<(BTreeMap<(u32, u16), BTreeSet<String>>, Option<String>)> {
        let mut ports: BTreeMap<(u32, u16), BTreeSet<String>> = BTreeMap::new();
        let mut failures = Vec::new();
        for family in [AF_INET, AF_INET6] {
            let table = match tcp_table(family as u32) {
                Ok(table) => table,
                Err(error) => {
                    failures.push(error.to_string());
                    continue;
                }
            };
            let (offset, stride) = if family == AF_INET {
                (
                    offset_of!(MIB_TCPTABLE_OWNER_PID, table),
                    size_of::<MIB_TCPROW_OWNER_PID>(),
                )
            } else {
                (
                    offset_of!(MIB_TCP6TABLE_OWNER_PID, table),
                    size_of::<MIB_TCP6ROW_OWNER_PID>(),
                )
            };
            let count = table[0] as usize;
            if offset + count.saturating_mul(stride) > table.len() * 4 {
                bail!("系统返回的端口表不完整");
            }
            for index in 0..count {
                // Bounds checked above; read_unaligned also covers the variable table header.
                let row = unsafe { table.as_ptr().cast::<u8>().add(offset + index * stride) };
                let (pid, port, address) = unsafe {
                    if family == AF_INET {
                        let row = ptr::read_unaligned(row.cast::<MIB_TCPROW_OWNER_PID>());
                        (
                            row.dwOwningPid,
                            u16::from_be(row.dwLocalPort as u16),
                            Ipv4Addr::from(row.dwLocalAddr.to_ne_bytes()).to_string(),
                        )
                    } else {
                        let row = ptr::read_unaligned(row.cast::<MIB_TCP6ROW_OWNER_PID>());
                        let ip = Ipv6Addr::from(row.ucLocalAddr);
                        let address = if row.dwLocalScopeId > 0 {
                            format!("{ip}%{}", row.dwLocalScopeId)
                        } else {
                            ip.to_string()
                        };
                        (
                            row.dwOwningPid,
                            u16::from_be(row.dwLocalPort as u16),
                            address,
                        )
                    }
                };
                if port != 0 && pid != 0 {
                    ports.entry((pid, port)).or_default().insert(address);
                }
            }
        }
        if failures.len() == 2 {
            bail!("{}", failures.join("；"));
        }
        let warning =
            (!failures.is_empty()).then(|| "部分网络端口暂时无法读取，请刷新重试。".into());
        Ok((ports, warning))
    }

    fn lineage(pid: u32, processes: &HashMap<u32, ProcessEntry>) -> Vec<u32> {
        let mut chain = Vec::new();
        let mut current = pid;
        for _ in 0..20 {
            if current == 0 || chain.contains(&current) {
                break;
            }
            let Some(process) = processes.get(&current) else {
                break;
            };
            chain.push(current);
            // Do not cross an interactive shell/IDE boundary into unrelated launched tools.
            if matches!(
                process.name.as_str(),
                "powershell.exe" | "pwsh.exe" | "windowsterminal.exe" | "code.exe" | "explorer.exe"
            ) {
                break;
            }
            current = process.parent;
        }
        chain
    }

    pub fn project_listeners(root: u32, expected: Option<u64>) -> Result<serde_json::Value> {
        // This lightweight query only reads process ancestry and TCP tables.
        // It does not initialize GPUI, enumerate command lines or open projects.
        let identity = (root != 0)
            .then(|| process_identity(root))
            .transpose()
            .ok()
            .flatten();
        let Some(started) =
            identity.filter(|started| expected.is_none_or(|value| value == *started))
        else {
            return Ok(serde_json::json!({ "alive": false, "listeners": [] }));
        };
        let processes = processes()?;
        let (ports, _) = listeners()?;
        let mut identities = HashMap::from([(root, Some(started))]);
        let mut owned = Vec::new();
        let mut owned_listeners = HashSet::new();
        for (&(pid, port), addresses) in &ports {
            let mut current = pid;
            let mut chain = Vec::new();
            // Include shells spawned by npm, but never cross beyond our own root.
            for _ in 0..64 {
                if current == 0 || chain.contains(&current) {
                    break;
                }
                chain.push(current);
                if current == root {
                    break;
                }
                let Some(process) = processes.get(&current) else {
                    break;
                };
                current = process.parent;
            }
            if chain.last() != Some(&root) {
                continue;
            }
            let mut child_created = u64::MAX;
            let valid = chain.iter().all(|pid| {
                let identity = identities
                    .entry(*pid)
                    .or_insert_with(|| process_identity(*pid).ok());
                if let Some(created) = *identity {
                    // A reused parent PID is newer than its alleged child.
                    if created >= started && created <= child_created {
                        child_created = created;
                        return true;
                    }
                }
                false
            });
            if valid {
                owned_listeners.insert((pid, port));
                owned.push(serde_json::json!({ "pid": pid, "port": port, "addresses": addresses }));
            }
        }
        // Windows can allow one process on 127.0.0.1:PORT and another on
        // 0.0.0.0:PORT. The wildcard listener alone does not prove which project
        // a loopback request will reach. Never accept such a shared port.
        let owned_ports: HashSet<_> = owned_listeners.iter().map(|(_, port)| *port).collect();
        let blocked: BTreeSet<_> = ports
            .keys()
            .filter(|(pid, port)| {
                owned_ports.contains(port) && !owned_listeners.contains(&(*pid, *port))
            })
            .map(|(_, port)| *port)
            .collect();
        Ok(
            serde_json::json!({ "alive": true, "started": started.to_string(), "listeners": owned, "blockedPorts": blocked }),
        )
    }

    fn runtime(name: &str) -> Option<&'static str> {
        match name {
            "node.exe" | "node" => Some("Node.js"),
            "bun.exe" | "bun" => Some("Bun"),
            "deno.exe" | "deno" => Some("Deno"),
            "pnpm.exe" => Some("pnpm"),
            _ => None,
        }
    }

    fn dev_kind(commands: &[String]) -> Option<String> {
        for command in commands {
            let command = command.replace('\\', "/").to_lowercase();
            let words: Vec<_> = command
                .split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
                .filter(|s| !s.is_empty())
                .collect();
            let dev = words
                .iter()
                .any(|word| *word == "dev" || word.starts_with("dev:"));
            for manager in ["npm", "pnpm", "yarn", "bun"] {
                let manager_command = words.iter().any(|word| {
                    let name = word.rsplit('/').next().unwrap_or(word);
                    matches!(name, "npm-cli.js" | "npm.cmd" | "npm") && manager == "npm"
                        || (manager != "npm"
                            && (name == manager || name.starts_with(&format!("{manager}."))))
                });
                if dev && manager_command {
                    return Some(format!("{manager} · dev"));
                }
            }
            if command.contains("/vite/") || words.contains(&"vite") {
                return Some("Vite".into());
            }
            if command.contains("next-server") || (command.contains("/next/") && dev) {
                return Some("Next.js".into());
            }
            if command.contains("webpack-dev-server") {
                return Some("Webpack".into());
            }
            if command.contains("react-scripts") && words.contains(&"start") {
                return Some("React".into());
            }
            if command.contains("nuxt") && dev {
                return Some("Nuxt".into());
            }
            if command.contains("astro") && dev {
                return Some("Astro".into());
            }
        }
        None
    }

    fn path_key(path: &str) -> String {
        path.replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    }

    pub fn discover(workspace: &Path) -> Result<Discovery> {
        let (listeners, mut warning) = listeners()?;
        let processes = processes()?;
        let mut chains = HashMap::new();
        let mut requested = HashSet::new();
        for &(pid, _) in listeners.keys() {
            let chain = lineage(pid, &processes);
            if chain
                .iter()
                .any(|pid| runtime(&processes[pid].name).is_some())
            {
                requested.extend(chain.iter().copied());
                chains.insert(pid, chain);
            }
        }
        if requested.is_empty() {
            return Ok(Discovery {
                servers: vec![],
                warning,
            });
        }
        let pids: Vec<_> = requested.into_iter().map(Pid::from_u32).collect();
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            ProcessRefreshKind::new()
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always),
        );
        let excluded = path_key(&workspace.join("runtime/server.mjs").to_string_lossy());
        let mut servers = Vec::new();
        let mut unreadable = false;
        for ((pid, port), addresses) in listeners {
            let Some(chain) = chains.get(&pid) else {
                continue;
            };
            let Some(process) = system.process(Pid::from_u32(pid)) else {
                continue;
            };
            if process
                .cmd()
                .iter()
                .any(|arg| path_key(&arg.to_string_lossy()) == excluded)
            {
                continue;
            }
            // Parent PIDs can be reused; a parent newer than the listener is not its launcher.
            let relatives: Vec<_> = chain
                .iter()
                .filter_map(|pid| system.process(Pid::from_u32(*pid)))
                .take_while(|parent| parent.start_time() <= process.start_time())
                .collect();
            let commands: Vec<_> = relatives
                .iter()
                .map(|p| {
                    p.cmd()
                        .iter()
                        .map(|s| s.to_string_lossy())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect();
            let detected = dev_kind(&commands);
            let fallback = runtime(&processes[&pid].name);
            if detected.is_none() && fallback.is_none() {
                continue;
            }
            if process.cmd().is_empty() {
                unreadable = true;
            }
            let kind = detected.unwrap_or_else(|| fallback.unwrap_or("Node.js").into());
            let directory = relatives
                .iter()
                .find_map(|p| p.cwd())
                .map(|path| path.to_string_lossy().into_owned());
            let name = directory
                .as_deref()
                .and_then(|path| Path::new(path).file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{kind} {}", crate::i18n::tr("服务")));
            let addresses: Vec<_> = addresses.into_iter().collect();
            let bound = addresses
                .iter()
                .find(|address| address.as_str() == "127.0.0.1")
                .or_else(|| {
                    addresses
                        .iter()
                        .find(|address| address.as_str() == "0.0.0.0")
                })
                .unwrap_or(&addresses[0]);
            let host = match bound.as_str() {
                "0.0.0.0" => "127.0.0.1".into(),
                "::" => "[::1]".into(),
                address if address.contains(':') => format!("[{address}]"),
                address => address.into(),
            };
            let secure = commands.iter().any(|cmd| {
                cmd.split_whitespace()
                    .any(|arg| matches!(arg, "--https" | "--ssl" | "--experimental-https"))
            });
            servers.push(DevServer {
                pid,
                port,
                addresses,
                url: format!("{}://{host}:{port}", if secure { "https" } else { "http" }),
                name,
                directory,
                kind,
                started: process_identity(pid).unwrap_or_default(),
            });
        }
        if unreadable {
            warning = Some("部分进程信息无法读取，暂以 Node 服务显示。".into());
        }
        servers.sort_by_key(|server| (server.port, server.pid));
        Ok(Discovery { servers, warning })
    }
}
