import http from 'node:http';
import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';
import { spawn, execFile } from 'node:child_process';
import { createCaptureWorker } from './capture.mjs';
import { detectProjectTypes, projectCategories } from './project-types.mjs';
import { startLanPreview } from './lan-preview.mjs';

const here = path.dirname(fileURLToPath(import.meta.url));
if (!process.env.GPM_DATA_DIR || !path.isAbsolute(process.env.GPM_DATA_DIR)) {
  throw new Error('请从 Project Manager 启动后台，并先设置数据存储目录。');
}
const dataDir = process.env.GPM_DATA_DIR;
const previewDir = path.join(dataDir, 'previews');
await fs.mkdir(previewDir, { recursive: true });
const readJson = async (file, fallback) => { try { return JSON.parse(await fs.readFile(file, 'utf8')); } catch { return fallback; } };
let settings = await readJson(path.join(dataDir, 'settings.json'), { root: '', roots: [], favorites: [], autoCapture: true });
const rootKey = root => process.platform === 'win32' ? root.toLowerCase() : root;
function normalizeRoots(roots) {
  if (!Array.isArray(roots) || roots.length > 64) throw new Error('项目目录必须为列表，最多支持 64 个目录');
  const unique = new Map();
  for (const root of roots) {
    if (typeof root !== 'string' || !root.trim()) throw new Error('项目目录不能为空');
    const absolute = path.resolve(root.trim());
    unique.set(rootKey(absolute), absolute);
  }
  return [...unique.values()];
}
settings.roots = normalizeRoots(settings.roots ?? (settings.root ? [settings.root] : []));
settings.root = settings.roots[0] || '';
settings.gameEnginesOnly = settings.gameEnginesOnly === true;
settings.closeToTray = settings.closeToTray === true;
const languages = new Set(['system', 'zh-CN', 'en']);
settings.language = languages.has(settings.language) ? settings.language : 'system';
const themeIds = new Set(['default', 'terracotta', 'rose', 'glacier', 'forest', 'olive', 'coffee', 'coast']);
settings.theme = themeIds.has(settings.theme) ? settings.theme : 'default';
const sortOrders = new Set(['modified', 'created', 'name']);
settings.sortOrder = sortOrders.has(settings.sortOrder) ? settings.sortOrder : 'modified';
let previews = await readJson(path.join(dataDir, 'previews.json'), {});
let projects = [];
let initialScanError = null;
const sessions = new Map();
const dependencyFile = path.join(dataDir, 'dependencies.json');
const dependencyState = new Map(Object.entries(await readJson(dependencyFile, {})).map(([id, state]) => [id,
  state.state === 'installing' ? { ...state, state: 'error', error: '上次依赖安装已中断，请重试。' } : state,
]));
const installJobs = new Map();
let dependencyWrite = Promise.resolve();
function saveDependencyState() {
  const contents = JSON.stringify(Object.fromEntries(dependencyState), null, 2);
  dependencyWrite = dependencyWrite.catch(() => {}).then(async () => {
    const temporary = `${dependencyFile}.tmp`;
    await fs.writeFile(temporary, contents);
    await fs.rename(temporary, dependencyFile);
  });
  return dependencyWrite;
}
const captureState = new Map();
const queue = [];
let working = false;
let captureWorker;
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const saveSettings = () => fs.writeFile(path.join(dataDir, 'settings.json'), JSON.stringify(settings, null, 2));
const hash = input => createHash('sha256').update(input).digest('hex').slice(0, 16);
const getProject = id => { const project = projects.find(p => p.id === id); if (!project) throw new Error('项目不存在，请重新扫描目录'); return project; };

async function dependenciesPresent(directory, pkg) {
  if (!pkg || !Object.keys({ ...pkg.dependencies, ...pkg.devDependencies, ...pkg.optionalDependencies }).length) return true;
  // Workspace installs and Yarn PnP may keep dependencies above the project directory.
  for (let current = directory; ; current = path.dirname(current)) {
    const manifest = current === directory ? pkg : await readJson(path.join(current, 'package.json'), null);
    const workspace = current === directory || manifest?.workspaces
      || await fs.access(path.join(current, 'pnpm-workspace.yaml')).then(() => true, () => false);
    if (workspace) {
      for (const name of ['node_modules', '.pnp.cjs', '.pnp.js']) {
        if (await fs.access(path.join(current, name)).then(() => true, () => false)) return true;
      }
    }
    if (path.dirname(current) === current) return false;
  }
}

async function dependencyCommand(project) {
  const pkg = JSON.parse(await fs.readFile(path.join(project.path, 'package.json'), 'utf8'));
  let manager;
  if (pkg.packageManager) {
    manager = /^(npm|pnpm|yarn|bun)@/.exec(pkg.packageManager)?.[1];
    if (!manager) throw new Error('package.json 中的包管理器暂不支持，请在项目目录手动安装依赖。');
  } else {
    const files = await fs.readdir(project.path);
    manager = files.includes('pnpm-lock.yaml') ? 'pnpm' : files.includes('yarn.lock') ? 'yarn'
      : files.some(file => ['bun.lock', 'bun.lockb'].includes(file)) ? 'bun' : 'npm';
  }
  const args = manager === 'npm' ? ['install', '--include=dev', '--no-audit', '--no-fund']
    : manager === 'pnpm' ? ['install', '--prod=false'] : ['install'];
  if (manager === 'npm') {
    const cli = process.env.npm_execpath?.endsWith('npm-cli.js') ? process.env.npm_execpath
      : path.join(path.dirname(process.execPath), 'node_modules/npm/bin/npm-cli.js');
    if (await fs.access(cli).then(() => true, () => false)) return { manager, pkg, executable: process.execPath, args: [cli, ...args] };
  }
  // Only fixed, allowlisted command strings enter cmd.exe; project paths are passed as cwd.
  const commands = { npm: 'npm install --include=dev --no-audit --no-fund', pnpm: 'pnpm install --prod=false', yarn: 'yarn install', bun: 'bun install' };
  return process.platform === 'win32'
    ? { manager, pkg, executable: process.env.ComSpec || 'cmd.exe', args: ['/d', '/s', '/c', commands[manager]] }
    : { manager, pkg, executable: manager, args };
}

async function installDependencies(project) {
  if (installJobs.has(project.id)) return;
  if (['starting', 'running', 'stopping'].includes(sessions.get(project.id)?.status)
      || ['queued', 'capturing'].includes(captureState.get(project.id))) {
    throw new Error('请先停止运行或等待预览获取完成，再安装依赖。');
  }
  // Reserve this project before awaiting filesystem reads so duplicate clicks cannot spawn twice.
  const job = { child: null, done: null };
  installJobs.set(project.id, job);
  try {
    const command = await dependencyCommand(project);
    const state = { state: 'installing', error: null, logs: `> ${command.manager} install\n` };
    dependencyState.set(project.id, state);
    await saveDependencyState();
    if (shuttingDown) throw new Error('工作台正在关闭，依赖安装已取消。');
    project.dependenciesInstalled = false;
    const session = sessions.get(project.id);
    if (session) { session.status = 'idle'; session.error = null; session.logs = ''; }
    const child = spawn(command.executable, command.args, {
      cwd: project.path,
      env: { ...process.env, NODE_ENV: 'development', FORCE_COLOR: '0', COREPACK_ENABLE_DOWNLOAD_PROMPT: '0' },
      windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'],
    });
    job.child = child;
    const append = chunk => { state.logs = (state.logs + chunk.toString().replace(/\x1b\[[0-9;]*m/g, '')).slice(-16000); };
    child.stdout.on('data', append);
    child.stderr.on('data', append);
    let spawnError;
    child.on('error', error => { spawnError = error; append(`${error.message}\n`); });
    job.done = new Promise(resolve => child.once('close', async code => {
      try {
        if (spawnError || code !== 0) throw new Error(spawnError?.message || `${command.manager} 安装失败（退出码 ${code}），请查看日志后重试。`);
        if (!await dependenciesPresent(project.path, command.pkg)) throw new Error('安装命令已结束，但未找到依赖文件，请查看安装日志。');
        const current = projects.find(p => p.id === project.id);
        if (current) current.dependenciesInstalled = true;
        state.state = 'done';
        state.error = null;
        if (previews[project.id]?.error === '项目依赖未安装') {
          previews[project.id].error = null;
          await fs.writeFile(path.join(dataDir, 'previews.json'), JSON.stringify(previews, null, 2));
        }
      } catch (error) {
        state.state = 'error';
        state.error = error.message;
        const current = projects.find(p => p.id === project.id);
        if (current) current.dependenciesInstalled = false;
      }
      try { await saveDependencyState(); }
      catch (error) { state.state = 'error'; state.error = `保存安装结果失败：${error.message}`; }
      finally { installJobs.delete(project.id); resolve(); }
    }));
  } catch (error) {
    installJobs.delete(project.id);
    const state = dependencyState.get(project.id);
    if (state?.state === 'installing') {
      state.state = 'error'; state.error = error.message;
      await saveDependencyState().catch(() => {});
    }
    throw error;
  }
}

async function scan(roots = settings.roots) {
  if (installJobs.size) throw new Error('正在安装项目依赖，请完成后再扫描或修改目录。');
  const directories = normalizeRoots(roots);
  const next = [];
  // Collect every directory before changing the library, sessions, or saved settings.
  for (const absolute of directories) {
    const entries = await fs.readdir(absolute, { withFileTypes: true });
    for (const entry of entries) {
      if (!entry.isDirectory() || entry.name.startsWith('.')) continue;
      const directory = path.join(absolute, entry.name);
      const pkg = await readJson(path.join(directory, 'package.json'), null);
      let files;
      try { files = await fs.readdir(directory); } catch { continue; }
      const isWeb = files.includes('index.html') || pkg?.scripts?.['dev:web'] || pkg?.scripts?.dev;
      if (!isWeb) continue;
      const stats = await fs.stat(directory);
      const classification = await detectProjectTypes(directory, pkg, files);
      let modified = stats.mtimeMs;
      const createdAt = Number.isFinite(stats.birthtimeMs) && stats.birthtimeMs > 0 ? stats.birthtimeMs : null;
      for (const name of ['src', 'index.html', 'package.json']) {
        try { modified = Math.max(modified, (await fs.stat(path.join(directory, name))).mtimeMs); } catch {}
      }
      next.push({ id: hash(directory.toLowerCase()), name: entry.name, path: directory, root: absolute, classification, modified, createdAt, script: pkg?.scripts?.['dev:web'] ? 'dev:web' : pkg?.scripts?.dev ? 'dev' : null, dependenciesInstalled: await dependenciesPresent(directory, pkg) });
    }
  }
  const nextIds = new Set(next.map(p => p.id));
  for (let i = queue.length - 1; i >= 0; i--) if (!nextIds.has(queue[i])) { captureState.delete(queue[i]); queue.splice(i, 1); }
  await Promise.all([...sessions.keys()].filter(id => !nextIds.has(id)).map(stopProject));
  projects = next.sort((a, b) => b.modified - a.modified);
  settings.roots = directories;
  settings.root = directories[0] || '';
  await saveSettings();
  initialScanError = null;
  return snapshot();
}

function snapshot() {
  return { settings, cachePath: previewDir, queueLength: queue.length + Number(working), projects: projects.map(p => {
    const session = sessions.get(p.id);
    const dependency = dependencyState.get(p.id);
    const installState = installJobs.has(p.id) ? 'installing' : dependency?.state || 'idle';
    const categories = projectCategories(p.classification, settings.gameEnginesOnly);
    return { ...p, engine: categories[0], categories, dependenciesInstalled: p.dependenciesInstalled && !['installing', 'error'].includes(installState), installState, installError: dependency?.error || null, favorite: settings.favorites.includes(p.id), status: session?.status || 'idle', url: session?.url || null, lanUrls: session?.status === 'running' ? session.lan?.urls() || [] : [], lanError: session?.lanError || null, error: session?.error || null, logs: installState === 'installing' || installState === 'error' ? dependency?.logs || '' : session?.logs || dependency?.logs || '', capture: captureState.get(p.id) || 'idle', preview: previews[p.id]?.time ? `/previews/${p.id}.jpg?v=${previews[p.id].time}` : null, capturedAt: previews[p.id]?.time || null, captureError: previews[p.id]?.error || null };
  }) };
}

function addLogs(session, chunk) {
  const line = chunk.toString().replace(/\x1b\[[0-9;]*m/g, '');
  session.logs = (session.logs + line).slice(-10000);
  const match = line.match(/https?:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0):\d+\/?[^\s]*/);
  if (match && !session.url) session.url = match[0].replace('0.0.0.0', '127.0.0.1').replace('localhost', '127.0.0.1');
}

async function startProject(project, owner = 'user') {
  let session = sessions.get(project.id);
  if (session?.stopping) await session.stopping;
  if (session && ['starting', 'running'].includes(session.status)) {
    if (owner === 'user') session.owner = 'user';
    await session.ready;
    await enableLanPreview(session);
    return session;
  }
  if (installJobs.has(project.id)) throw new Error('正在安装依赖，请稍候。');
  if (!project.dependenciesInstalled || dependencyState.get(project.id)?.state === 'error') throw new Error('项目尚未安装依赖，请先点击“安装依赖”。');
  if ([...sessions.values()].filter(s => ['starting', 'running'].includes(s.status)).length >= 4) throw new Error('最多同时运行 4 个项目，请先停止一个项目。');
  session = { status: 'starting', logs: '', url: null, error: null, child: null, owner };
  sessions.set(project.id, session);
  session.ready = (async () => {
    if (project.script) {
      const npmCli = process.env.npm_execpath || path.join(path.dirname(process.execPath), 'node_modules/npm/bin/npm-cli.js');
      session.child = spawn(process.execPath, [npmCli, 'run', project.script], { cwd: project.path, env: { ...process.env, BROWSER: 'none', FORCE_COLOR: '0' }, windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    } else {
      session.child = spawn(process.execPath, [path.join(here, 'static-server.mjs'), project.path], { windowsHide: true, stdio: ['ignore', 'pipe', 'pipe'] });
    }
    session.child.stdout.on('data', data => addLogs(session, data));
    session.child.stderr.on('data', data => addLogs(session, data));
    session.child.on('error', error => { session.lan?.close(); session.status = 'error'; session.error = error.message; });
    session.child.on('exit', code => {
      session.lan?.close();
      if (!['idle', 'stopping'].includes(session.status)) { session.status = 'error'; session.error = `启动进程已退出（${code}）`; }
    });
    const deadline = Date.now() + 60000;
    while (Date.now() < deadline) {
      if (session.status === 'error') throw new Error(session.error);
      if (['idle', 'stopping'].includes(session.status)) throw new Error('启动已取消');
      if (session.url) {
        try {
          const response = await fetch(session.url, { signal: AbortSignal.timeout(2000) });
          if (response.ok && session.status === 'starting') {
            session.status = 'running';
            await enableLanPreview(session);
            return session;
          }
        } catch {}
      }
      await sleep(350);
    }
    await stopProject(project.id);
    session.status = 'error';
    session.error = '60 秒内未能启动，请查看运行日志。';
    throw new Error(session.error);
  })();
  return session.ready;
}

async function enableLanPreview(session) {
  if (session.owner !== 'user' || session.status !== 'running' || session.lan) return;
  if (session.lanStarting) return session.lanStarting;
  session.lanStarting = (async () => {
    try {
      const preview = await startLanPreview(session.url);
      if (session.status !== 'running') { preview.close(); return; }
      session.lan = preview;
      session.lanError = null;
    } catch (error) {
      session.lanError = `局域网预览启动失败：${error.message}`;
    } finally { session.lanStarting = null; }
  })();
  return session.lanStarting;
}

async function stopProject(id) {
  const session = sessions.get(id);
  if (!session) return;
  if (session.stopping) return session.stopping;
  session.status = 'stopping';
  session.lan?.close();
  session.lan = null;
  session.lanError = null;
  session.stopping = (async () => {
    await terminateChild(session.child);
    session.status = 'idle';
    session.url = null;
    session.error = null;
  })();
  try { await session.stopping; }
  catch (error) { session.status = 'error'; session.error = error.message; throw error; }
  finally { session.stopping = null; }
}

async function terminateChild(child) {
  if (child?.pid && child.exitCode === null && child.signalCode === null) {
    if (process.platform === 'win32') {
      await new Promise((resolve, reject) => execFile('taskkill', ['/pid', String(child.pid), '/T', '/F'], { windowsHide: true }, error => {
        if (error && child.exitCode === null && child.signalCode === null) reject(new Error(`关闭项目服务失败：${error.message}`));
        else resolve();
      }));
    } else {
      child.kill('SIGTERM');
    }
    const deadline = Date.now() + 5000;
    while (child.exitCode === null && child.signalCode === null && Date.now() < deadline) await sleep(25);
    if (child.exitCode === null && child.signalCode === null) throw new Error('项目服务未退出，请重试停止');
  }
}

function enqueue(id, force = false) {
  const project = getProject(id);
  if ((!force && previews[id]?.time) || ['queued', 'capturing'].includes(captureState.get(id))) return;
  if (installJobs.has(id) || !project.dependenciesInstalled || dependencyState.get(id)?.state === 'error') { previews[id] = { ...previews[id], error: '项目依赖未安装' }; return; }
  captureState.set(id, 'queued');
  queue.push(id);
  void runQueue();
}

async function runQueue() {
  if (working) return;
  working = true;
  while (queue.length) {
    const id = queue.shift();
    captureState.set(id, 'capturing');
    try {
      const project = getProject(id);
      const session = await startProject(project, 'capture');
      captureWorker ??= createCaptureWorker();
      const file = path.join(previewDir, `${id}.jpg`);
      await captureWorker.capture(session.url, file);
      previews[id] = { time: Date.now(), error: null };
    } catch (error) {
      previews[id] = { ...previews[id], error: error.message };
      await captureWorker?.close().catch(() => {});
      captureWorker = null;
    } finally {
      try {
        if (sessions.get(id)?.owner === 'capture') await stopProject(id);
      } catch (error) {
        previews[id] = { ...previews[id], error: error.message };
      }
      try { await fs.writeFile(path.join(dataDir, 'previews.json'), JSON.stringify(previews, null, 2)); }
      catch (error) { previews[id] = { ...previews[id], error: `保存预览缓存失败：${error.message}` }; }
      // Completion includes process cleanup and persisting the preview, not just taking the image.
      captureState.set(id, previews[id]?.error ? 'error' : 'done');
    }
  }
  await captureWorker?.close().catch(() => {});
  captureWorker = null;
  working = false;
  if (queue.length) void runQueue();
}

const json = (res, status, data) => { res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Cache-Control': 'no-store' }); res.end(JSON.stringify(data)); };
async function body(req) {
  let value = '';
  for await (const chunk of req) { value += chunk; if (value.length > 16000) throw new Error('请求过大'); }
  return value ? JSON.parse(value) : {};
}

const server = http.createServer(async (req, res) => {
  try {
    const expectedHost = `127.0.0.1:${server.address().port}`;
    const hosts = [expectedHost, `localhost:${server.address().port}`];
    if (!hosts.includes(req.headers.host)) return json(res, 403, { error: '无效的访问地址' });
    const url = new URL(req.url, `http://${expectedHost}`);
    if (req.method === 'POST') {
      if (req.headers.origin && !hosts.some(host => req.headers.origin === `http://${host}`)) return json(res, 403, { error: '仅允许本地面板操作' });
      const data = await body(req);
      if (url.pathname === '/api/scan') return json(res, 200, await scan(data.roots ?? (data.root ? [data.root] : settings.roots)));
      if (url.pathname === '/api/settings') {
        if (typeof data.autoCapture === 'boolean') settings.autoCapture = data.autoCapture;
        if (typeof data.gameEnginesOnly === 'boolean') settings.gameEnginesOnly = data.gameEnginesOnly;
        if (typeof data.closeToTray === 'boolean') settings.closeToTray = data.closeToTray;
        if (languages.has(data.language)) settings.language = data.language;
        if (themeIds.has(data.theme)) settings.theme = data.theme;
        if (sortOrders.has(data.sortOrder)) settings.sortOrder = data.sortOrder;
        if (data.favorite) { getProject(data.favorite); settings.favorites = settings.favorites.includes(data.favorite) ? settings.favorites.filter(id => id !== data.favorite) : [...settings.favorites, data.favorite]; }
        await saveSettings();
        return json(res, 200, snapshot());
      }
      if (url.pathname === '/api/capture-all') {
        const root = data.root == null ? null : normalizeRoots([data.root])[0];
        if (root && !settings.roots.some(item => rootKey(item) === rootKey(root))) throw new Error('目录未加入工作台');
        projects.filter(p => (!root || rootKey(p.root) === rootKey(root)) && (data.force || !previews[p.id]?.time)).forEach(p => enqueue(p.id, !!data.force));
        return json(res, 202, snapshot());
      }
      const match = url.pathname.match(/^\/api\/projects\/([a-f0-9]{16})\/(start|stop|capture|folder|install)$/);
      if (match) {
        const project = getProject(match[1]);
        if (match[2] === 'install') await installDependencies(project);
        if (match[2] === 'start') {
          await startProject(project);
          // Refresh in the background after a manual launch, including existing cached previews.
          enqueue(project.id, true);
        }
        if (match[2] === 'stop') await stopProject(project.id);
        if (match[2] === 'capture') enqueue(project.id, true);
        if (match[2] === 'folder') {
          if (process.platform === 'win32') spawn('explorer.exe', [project.path], { windowsHide: true, detached: true, stdio: 'ignore' }).unref();
          else throw new Error('打开文件夹目前仅支持 Windows');
        }
        return json(res, 200, snapshot());
      }
      return json(res, 404, { error: '接口不存在' });
    }
    if (req.method !== 'GET') return json(res, 405, { error: '不支持的请求方法' });
    if (url.pathname === '/api/projects') {
      // An unavailable configured directory must not replace a native cache with an empty library.
      if (initialScanError) throw initialScanError;
      return json(res, 200, snapshot());
    }
    return json(res, 404, { error: '接口不存在' });
  } catch (error) { json(res, error.code === 'ENOENT' ? 404 : 400, { error: error.message }); }
});

try { await scan(); } catch (error) { initialScanError = error; console.error(`目录扫描失败: ${error.message}`); }
server.on('error', error => {
  console.error(error.code === 'EADDRINUSE' ? '工作台端口已被占用。请打开已有工作台，或设置 PORT 使用其他端口。' : error.message);
  void shutdown();
});
server.listen(Number(process.env.PORT || 0), '127.0.0.1', () => {
  const address = `http://127.0.0.1:${server.address().port}`;
  console.log(`Project Manager → ${address}`);
});
let shuttingDown = false;
async function shutdown() {
  if (shuttingDown) return;
  shuttingDown = true;
  queue.length = 0;
  await Promise.all([...sessions.keys()].map(stopProject));
  await Promise.all([...installJobs.values()].map(async job => {
    await terminateChild(job.child);
    await job.done;
  }));
  await captureWorker?.close().catch(() => {});
  server.close();
  process.exit(0);
}
process.on('SIGINT', shutdown);
process.on('SIGTERM', shutdown);
