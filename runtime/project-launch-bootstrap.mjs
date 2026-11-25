import { register } from 'node:module';

const port = Number(process.env.GPM_PROJECT_PORT);
if (Number.isInteger(port) && port > 0 && port <= 65535) {
  // npm scripts often call Vite's JS API rather than its CLI. Override only
  // this launch's inline options, without editing the project's configuration.
  register(new URL('./project-launch-loader.mjs', import.meta.url), { data: { port } });

  const entry = (process.argv[1] || '').replaceAll('\\', '/');
  const vite = /\/vite\/(?:bin\/vite\.js|dist\/node\/cli\.js)$/.test(entry);
  const next = /\/next\/dist\/bin\/next(?:\.js)?$/.test(entry);
  const astro = /\/astro\/(?:astro\.js|bin\/astro\.mjs)$/.test(entry);
  const webpack = /\/(?:webpack-cli|webpack-dev-server)\/bin\//.test(entry);
  const parcel = /\/(?:parcel|parcel-bundler)\/.*(?:cli|bin)/.test(entry);
  if ((vite || next || astro || webpack || parcel) && !process.argv.slice(2).includes('build')) {
    const args = [];
    for (let index = 2; index < process.argv.length; index++) {
      const arg = process.argv[index];
      if (arg === '--port' || (next && arg === '-p')) { index++; continue; }
      if (arg.startsWith('--port=') || (next && /^-p\d+$/.test(arg))) continue;
      if ((vite || parcel) && /^--strictPort(?:=|$)/.test(arg)) {
        if (['true', 'false'].includes(process.argv[index + 1])) index++;
        continue;
      }
      args.push(arg);
    }
    process.argv.splice(2, process.argv.length - 2, ...args, '--port', String(port));
  }
}
