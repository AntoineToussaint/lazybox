import type { ServerOptions } from 'vite';

export function uiWatcher(env?: NodeJS.ProcessEnv, platform?: NodeJS.Platform): {
  backend: 'default' | 'native' | 'polling';
  options: NonNullable<ServerOptions['watch']>;
};
