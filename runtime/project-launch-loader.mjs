let launchPort;
export function initialize({ port }) { launchPort = port; }

export async function resolve(specifier, context, nextResolve) {
  const result = await nextResolve(specifier, context);
  if (!launchPort || context.parentURL?.endsWith('#project-manager-vite')) return result;
  if (!/\/vite\/dist\/node\/index\.js(?:[?#]|$)/.test(result.url)) return result;
  const original = JSON.stringify(result.url);
  const source = `
    import * as vite from ${original};
    export * from ${original};
    export function createServer(config = {}) {
      return vite.createServer({ ...config, server: {
        ...config.server, ...(process.env.GPM_LEVEL_EDITOR === '1' ? { host: '127.0.0.1' } : {}),
        port: ${launchPort}, strictPort: false, open: false
      }});
    }
    export function preview(config = {}) {
      return vite.preview({ ...config, preview: {
        ...config.preview, port: ${launchPort}, strictPort: false, open: false
      }});
    }
  `;
  return { url: `data:text/javascript,${encodeURIComponent(source)}#project-manager-vite`, shortCircuit: true };
}
