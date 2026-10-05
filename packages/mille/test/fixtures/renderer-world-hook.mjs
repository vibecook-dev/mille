// Module resolve hook: modules under the guarded directories may not import
// a Node builtin. Registered inside a Worker before mille's renderer-facing
// entries are loaded, it turns "this graph is node-free" from a static claim
// into something the loader enforces at runtime.

import { builtinModules } from 'node:module';

const BUILTINS = new Set(builtinModules.flatMap((name) => [name, `node:${name}`]));
let guarded = [];

export async function initialize(data) {
  guarded = data.guarded;
}

export async function resolve(specifier, context, nextResolve) {
  const parent = context.parentURL ?? '';
  if (guarded.some((dir) => parent.startsWith(dir)) && BUILTINS.has(specifier)) {
    throw new Error(`renderer-world module ${parent} imported ${specifier}`);
  }
  return nextResolve(specifier, context);
}
