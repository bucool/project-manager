import { execFile } from 'node:child_process';

// Log URLs are hints, never evidence that this project owns a port.
export function recordLaunchOutput(session, chunk, stream) {
  const text = chunk.toString().replace(/\x1b\[[0-9;]*m/g, '');
  session.logs = (session.logs + text).slice(-10000);
  session.outputBuffers ??= {};
  session.urlCandidates ??= new Map();
  const combined = (session.outputBuffers[stream] || '') + text;
  const lines = combined.split(/[\r\n]+/);
  session.outputBuffers[stream] = lines.pop().slice(-4096);
  // Also accept logs without a final newline. Ownership checks reject partial URLs.
  for (const line of [...lines, session.outputBuffers[stream]]) {
    for (const match of line.matchAll(/https?:\/\/(?:localhost|127\.0\.0\.1|0\.0\.0\.0|\[::1?\]):\d+(?:\/[^\s"'<>)]*)?/g)) {
      try {
        const url = new URL(match[0]);
        if (!url.port || Number(url.port) > 65535) continue;
        session.urlCandidates.delete(url.href);
        session.urlCandidates.set(url.href, url.href);
        while (session.urlCandidates.size > 24) session.urlCandidates.delete(session.urlCandidates.keys().next().value);
      } catch { /* A URL can be split across output chunks. */ }
    }
  }
}

export async function projectListeners(session) {
  const child = session.child;
  if (!child?.pid || child.exitCode !== null || child.signalCode !== null) return { alive: false, listeners: [] };
  const executable = process.env.GPM_EXECUTABLE;
  if (!executable || process.platform !== 'win32') throw new Error('无法验证项目端口，请从 Windows 版 Project Manager 启动。');
  const args = ['--project-listeners', String(child.pid)];
  if (session.processIdentity) args.push(session.processIdentity);
  const result = await new Promise((resolve, reject) => {
    execFile(executable, args, { windowsHide: true, timeout: 4000, maxBuffer: 256 * 1024 }, (error, stdout, stderr) => {
      if (error) return reject(new Error(`无法验证项目端口：${stderr.trim() || error.message}`));
      try { resolve(JSON.parse(stdout)); } catch { reject(new Error('无法验证项目端口：系统返回无效结果。')); }
    });
  });
  if (result.alive && typeof result.started === 'string') session.processIdentity ??= result.started;
  if (child.exitCode !== null || child.signalCode !== null) return { alive: false, listeners: [] };
  return result;
}

function owns(url, ownership) {
  if (!ownership.alive) return false;
  const host = url.hostname.replace(/^\[|\]$/g, '');
  const port = Number(url.port || (url.protocol === 'https:' ? 443 : 80));
  return ownership.listeners.some(listener => listener.port === port && listener.addresses.some(address =>
    address === host || (['localhost', '127.0.0.1'].includes(host) && ['127.0.0.1', '0.0.0.0'].includes(address))
      || (host === '::1' && ['::1', '::'].includes(address))));
}

export async function stillOwnsProjectUrl(session) {
  if (!session.url) return false;
  return owns(new URL(session.url), await projectListeners(session));
}

export async function readyProjectUrl(session) {
  const ownership = await projectListeners(session);
  if (!ownership.alive) throw new Error('项目启动进程已退出，请查看运行日志。');
  const candidates = new Map();
  for (const address of [...(session.urlCandidates?.values() || [])].reverse()) {
    const url = new URL(address);
    if (['localhost', '0.0.0.0'].includes(url.hostname)) url.hostname = '127.0.0.1';
    if (url.hostname === '[::]') url.hostname = '[::1]';
    if (owns(url, ownership)) candidates.set(url.href, url);
  }
  // Discover the actual listener even when a script prints no URL or chooses a
  // different port after EADDRINUSE. Never try a port belonging to another process.
  for (const listener of ownership.listeners) {
    for (const address of listener.addresses) {
      const host = ['0.0.0.0', '127.0.0.1'].includes(address) ? '127.0.0.1'
        : ['::', '::1'].includes(address) ? '[::1]' : address.includes(':') ? `[${address}]` : address;
      try {
        const url = new URL(`http://${host}:${listener.port}/`);
        if (!candidates.has(url.href)) candidates.set(url.href, url);
      } catch { /* Ignore unsupported scoped addresses. */ }
    }
  }
  for (const url of [...candidates.values()].slice(0, 12)) {
    if (session.status !== 'starting') return null;
    try {
      const response = await fetch(url, { signal: AbortSignal.timeout(1200), redirect: 'manual' });
      let ready = response.ok;
      if (response.status >= 300 && response.status < 400 && response.headers.get('location')) {
        const destination = new URL(response.headers.get('location'), url);
        ready = destination.origin === url.origin || owns(destination, ownership);
      }
      await response.body?.cancel();
      if (!ready) continue;
      // Re-check after the HTTP request: a project may exit while another server
      // acquires the same port. A successful HTTP response alone proves nothing.
      if (owns(url, await projectListeners(session)) && session.status === 'starting') return url.href;
    } catch (error) {
      if (session.child.exitCode !== null || session.child.signalCode !== null) throw error;
    }
  }
  return null;
}
