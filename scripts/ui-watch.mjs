// Explicit compatibility stopgap for silent FSEvents delivery on macOS Tahoe
// (fsevents/fsevents#406). Directory fs.watch also enters libuv's FSEvents path
// and can hang on close on the affected host. Use bounded polling there; keep
// event-driven backends selectable for diagnosis and eventual removal.
export function uiWatcher(env = process.env, platform = process.platform) {
  const backend = env.LAZYBOX_V2_UI_WATCHER ?? (platform === 'darwin' ? 'polling' : 'default');
  const choices = {
    default: {},
    native: { useFsEvents: false, usePolling: false },
    polling: { useFsEvents: false, usePolling: true, interval: 250, binaryInterval: 1000 },
  };
  if (!Object.hasOwn(choices, backend)) throw new Error('LAZYBOX_V2_UI_WATCHER must be default, native or polling');
  return { backend, options: choices[backend] };
}

