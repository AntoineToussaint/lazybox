// Metro for an app inside the V2 npm workspaces. Expo finds the workspace root
// and watches the shared packages (client-core, protocol, surfaces/models).
//
// One React: Expo SDK 58 pins react 19.3.0, which npm installs in this app's
// node_modules, while the web packages hoist their own React to the workspace
// root, beside react-native. Two copies in one bundle break hooks, so every
// `react` import, from this app or from a hoisted library, resolves to the
// app's copy.
const path = require('node:path');
const { getDefaultConfig } = require('expo/metro-config');

const config = getDefaultConfig(__dirname);
const fromApp = path.join(__dirname, 'package.json');
const upstream = config.resolver.resolveRequest;
config.resolver.resolveRequest = (context, name, platform) => {
  const resolve = upstream || context.resolveRequest;
  if (name === 'react' || name.startsWith('react/')) return resolve({ ...context, originModulePath: fromApp }, name, platform);
  return resolve(context, name, platform);
};

module.exports = config;
