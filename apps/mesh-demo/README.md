# Mille Mesh Demo

The same Electron app runs on both computers. Each instance can explicitly
share one folder with selected tailnet peers and can browse another peer's
`shared` export. Sharing and browsing are read-only by default.

## Run on macOS and Windows

Truffle currently ships a Windows x64 sidecar, so the Windows device must be
x64 (macOS supports Apple Silicon and Intel).

On each computer, from the repository root:

```bash
pnpm install
pnpm --filter @vibecook/mille build
pnpm --filter @vibecook/mille-ui build
pnpm --filter @vibecook/mille-truffle build
pnpm --filter mesh-demo dev
```

Then:

1. Run the commands above on both computers.
2. Use the sign-in link shown by each app to add its embedded Truffle node to
   the same tailnet. The identity is persisted under Electron's user-data
   directory, so this is normally one-time setup.
3. On the computer hosting files, select one or more peers, click **Choose
   folder and share**, and select a folder.
4. On the other computer, click **Browse read-only** beside the host.

The tailnet policy must allow TCP port `9451` between these app nodes. A share
also enforces its explicit peer allow-list in-process. No filesystem or mesh
object is exposed to the renderer; it receives only an authorized Mille
protocol `MessagePort`.

Read-write mode is intentionally a separate, explicit choice on both the host
and client. Individual reads and writes are capped at 16 MiB by default.
