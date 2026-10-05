// Child half of `panic-boundary.test.mjs`. Calls a deliberately panicking
// native entry point and reports whether the panic arrived as a JS error.
//
// This has to be a separate process: when the guard regresses the panic aborts
// the runtime, and a caller in-process cannot observe its own SIGABRT. The
// parent reads the exit signal to tell "threw" from "died".
//
// argv: <nativePath> <"sync"|"async"|"git-sync"|"git-async">
//
// The `git-*` probes exist only in a `--features panic-probe,git` build: they
// panic inside `mille-git` itself (SPEC mille-git §18.4), the async one inside
// a task on the git engine's own runtime.

const [, , nativePath, which] = process.argv;
const native = require(nativePath);
const probes = {
  sync: () => native.__panicProbeSync(),
  async: () => native.__panicProbeAsync(),
  'git-sync': () => native.__gitPanicProbeSync(),
  'git-async': () => native.__gitPanicProbeAsync(),
};

function report(err) {
  console.log(`CAUGHT: ${err && err.message}`);
  process.exit(0);
}

if (which === 'sync' || which === 'git-sync') {
  try {
    probes[which]();
  } catch (err) {
    report(err);
  }
  console.log('UNCAUGHT: returned normally');
  process.exit(3);
} else {
  probes[which]().then(
    () => {
      console.log('UNCAUGHT: resolved normally');
      process.exit(3);
    },
    (err) => report(err),
  );
}
