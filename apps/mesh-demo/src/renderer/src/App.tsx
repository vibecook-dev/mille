import { useEffect, useMemo, useRef, useState } from 'react';

import type { Entry, FileExplorer } from '@vibecook/mille';
import { connectFileExplorer, type PortFileExplorer } from '@vibecook/mille/port';
import { FileTree, FileTreeProvider } from '@vibecook/mille-ui';

import type { MeshDemoState, ShareAccess } from '../../shared/types';

const INITIAL_STATE: MeshDemoState = {
  mesh: 'starting',
  deviceName: 'Starting…',
  dnsName: null,
  authUrl: null,
  error: null,
  peers: [],
  share: null,
  remote: {
    status: 'idle',
    peerRef: null,
    peerName: null,
    access: null,
    workspaceInstanceId: null,
    maxFileBytes: null,
    error: null,
  },
};

interface Preview {
  readonly name: string;
  readonly text: string;
  readonly loading: boolean;
  readonly error: string | null;
}

function humanBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / 1024 ** 2).toFixed(1)} MiB`;
}

function messageOf(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

export function App(): React.JSX.Element {
  const [state, setState] = useState<MeshDemoState>(INITIAL_STATE);
  const [allowed, setAllowed] = useState<Set<string>>(() => new Set());
  const [shareAccess, setShareAccess] = useState<ShareAccess>('read-only');
  const [fx, setFx] = useState<PortFileExplorer | null>(null);
  const [preview, setPreview] = useState<Preview | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [actionError, setActionError] = useState<string | null>(null);
  const portTicket = useRef(0);

  useEffect(() => {
    const unsubscribe = window.milleMesh.onState(setState);
    void window.milleMesh.getState().then(setState, (error) => {
      setActionError(messageOf(error));
    });
    return unsubscribe;
  }, []);

  useEffect(() => {
    const receivePort = (event: MessageEvent): void => {
      if (event.source !== window || event.data?.type !== 'mesh-explorer-port') return;
      const port = event.ports[0];
      if (port === undefined) return;
      const ticket = ++portTicket.current;
      void connectFileExplorer(port).then(
        (next) => {
          if (ticket !== portTicket.current) {
            void next.dispose().catch(() => {});
            return;
          }
          setFx((previous) => {
            if (previous !== null) void previous.dispose().catch(() => {});
            return next;
          });
          setPreview(null);
          setActionError(null);
        },
        (error) => setActionError(`Explorer handshake failed: ${messageOf(error)}`),
      );
    };
    window.addEventListener('message', receivePort);
    return () => window.removeEventListener('message', receivePort);
  }, []);

  useEffect(() => {
    if (state.remote.status !== 'idle') return;
    portTicket.current += 1;
    setFx((previous) => {
      if (previous !== null) void previous.dispose().catch(() => {});
      return null;
    });
    setPreview(null);
  }, [state.remote.status]);

  useEffect(() => {
    const liveIds = new Set(state.peers.map((peer) => peer.tailscaleId));
    setAllowed((current) => new Set([...current].filter((id) => liveIds.has(id))));
  }, [state.peers]);

  const onlinePeers = useMemo(() => state.peers.filter((peer) => peer.online), [state.peers]);

  async function run(label: string, operation: () => Promise<unknown>): Promise<void> {
    setBusy(label);
    setActionError(null);
    try {
      await operation();
    } catch (error) {
      setActionError(messageOf(error));
    } finally {
      setBusy(null);
    }
  }

  function toggleAllowed(id: string): void {
    setAllowed((current) => {
      const next = new Set(current);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  async function openEntry(entry: Entry): Promise<void> {
    const current = fx;
    if (current === null) return;
    setPreview({ name: entry.name, text: '', loading: true, error: null });
    try {
      const value = await current.readText(entry.id);
      setPreview({ name: entry.name, text: String(value), loading: false, error: null });
    } catch (error) {
      setPreview({
        name: entry.name,
        text: '',
        loading: false,
        error: messageOf(error),
      });
    }
  }

  const meshLabel =
    state.mesh === 'online'
      ? 'Mesh online'
      : state.mesh === 'auth-required'
        ? 'Sign-in required'
        : state.mesh === 'error'
          ? 'Mesh failed'
          : 'Starting mesh…';

  return (
    <div className="app-shell">
      <header className="topbar">
        <div>
          <div className="brand">Mille Mesh</div>
          <div className="subtitle">Private, peer-authorized file browsing</div>
        </div>
        <div className={`status status--${state.mesh}`}>
          <span className="status-dot" />
          {meshLabel}
        </div>
      </header>

      {state.mesh === 'auth-required' ? (
        <section className="auth-banner">
          <div>
            <strong>Connect this app to your tailnet</strong>
            <p>The embedded mesh identity is stored locally and reused on the next launch.</p>
          </div>
          <button type="button" onClick={() => void run('auth', window.milleMesh.openAuth)}>
            Open secure sign-in
          </button>
        </section>
      ) : null}

      {(state.error !== null || actionError !== null) && (
        <div className="error-banner" role="alert">
          {actionError ?? state.error}
        </div>
      )}

      <main className="workspace">
        <aside className="control-panel">
          <section className="card identity-card">
            <div className="eyebrow">This device</div>
            <h2>{state.deviceName}</h2>
            <p>{state.dnsName ?? 'Waiting for a mesh address'}</p>
          </section>

          <section className="card">
            <div className="section-heading">
              <div>
                <div className="eyebrow">Share</div>
                <h2>Local folder</h2>
              </div>
              {state.share !== null ? <span className="pill">Live</span> : null}
            </div>

            {state.share === null ? (
              <>
                <p className="hint">Select exactly which peers may open the folder.</p>
                <div className="peer-checks">
                  {onlinePeers.length === 0 ? (
                    <div className="empty-small">No online peers discovered yet.</div>
                  ) : (
                    onlinePeers.map((peer) => (
                      <label className="peer-check" key={peer.ref}>
                        <input
                          type="checkbox"
                          checked={allowed.has(peer.tailscaleId)}
                          onChange={() => toggleAllowed(peer.tailscaleId)}
                        />
                        <span>{peer.displayName}</span>
                      </label>
                    ))
                  )}
                </div>
                <label className="field-label" htmlFor="share-access">
                  Host access
                </label>
                <select
                  id="share-access"
                  value={shareAccess}
                  onChange={(event) => setShareAccess(event.target.value as ShareAccess)}
                >
                  <option value="read-only">Read-only (recommended)</option>
                  <option value="read-write">Read-write</option>
                </select>
                <button
                  className="primary full"
                  type="button"
                  disabled={state.mesh !== 'online' || allowed.size === 0 || busy !== null}
                  onClick={() =>
                    void run('share', () =>
                      window.milleMesh.startSharing({
                        access: shareAccess,
                        allowedPeerIds: [...allowed],
                      }),
                    )
                  }
                >
                  Choose folder and share
                </button>
              </>
            ) : (
              <div className="share-live">
                <strong>{state.share.label}</strong>
                <code title={state.share.root}>{state.share.root}</code>
                <div className="facts">
                  <span>{state.share.access}</span>
                  <span>{state.share.allowedPeerIds.length} allowed peer(s)</span>
                  <span>{humanBytes(state.share.maxFileBytes)} limit</span>
                </div>
                <button
                  className="secondary full"
                  type="button"
                  disabled={busy !== null}
                  onClick={() => void run('stop-share', window.milleMesh.stopSharing)}
                >
                  Stop sharing
                </button>
              </div>
            )}
          </section>

          <section className="card peers-card">
            <div className="eyebrow">Browse</div>
            <h2>Mesh devices</h2>
            <div className="peer-list">
              {state.peers.length === 0 ? (
                <div className="empty-small">Peers appear here when both apps are online.</div>
              ) : (
                state.peers.map((peer) => (
                  <div className={`peer ${peer.online ? '' : 'peer--offline'}`} key={peer.ref}>
                    <div className="peer-copy">
                      <strong>{peer.displayName}</strong>
                      <span>
                        {peer.os ?? 'unknown OS'} · {peer.ip}
                      </span>
                    </div>
                    <div className="peer-actions">
                      <button
                        type="button"
                        disabled={!peer.online || busy !== null}
                        onClick={() =>
                          void run('connect', () =>
                            window.milleMesh.connectPeer({
                              peerRef: peer.ref,
                              access: 'read-only',
                            }),
                          )
                        }
                      >
                        Browse read-only
                      </button>
                      <button
                        className="text-button"
                        type="button"
                        disabled={!peer.online || busy !== null}
                        title="The host must have explicitly enabled read-write access."
                        onClick={() =>
                          void run('connect-write', () =>
                            window.milleMesh.connectPeer({
                              peerRef: peer.ref,
                              access: 'read-write',
                            }),
                          )
                        }
                      >
                        Request read-write
                      </button>
                    </div>
                  </div>
                ))
              )}
            </div>
          </section>
        </aside>

        <section className="browser-panel">
          <div className="browser-header">
            <div>
              <div className="eyebrow">Remote workspace</div>
              <h1>{state.remote.peerName ?? 'Choose a peer to browse'}</h1>
            </div>
            <div className="browser-actions">
              {state.remote.status !== 'idle' ? (
                <span className={`connection connection--${state.remote.status}`}>
                  {state.remote.status}
                </span>
              ) : null}
              {state.remote.status !== 'idle' ? (
                <button
                  className="secondary"
                  type="button"
                  onClick={() => void run('disconnect', window.milleMesh.disconnectPeer)}
                >
                  Disconnect
                </button>
              ) : null}
            </div>
          </div>

          {state.remote.error !== null ? (
            <div className="inline-warning">{state.remote.error}</div>
          ) : null}

          <div className="browser-body">
            <div className="tree-pane">
              {fx === null ? (
                <div className="empty-state">
                  <div className="empty-orbit">⌁</div>
                  <h3>No remote folder open</h3>
                  <p>Share a folder on the other device, then browse that peer read-only.</p>
                </div>
              ) : (
                <FileTreeProvider fx={fx as unknown as FileExplorer}>
                  <FileTree
                    fx={fx}
                    ariaLabel="Remote files"
                    rowHeight={24}
                    overscan={24}
                    stickyRoots
                    disableDragDrop
                    onOpen={(entry) => void openEntry(entry)}
                  />
                </FileTreeProvider>
              )}
            </div>
            <div className="preview-pane">
              {preview === null ? (
                <div className="empty-preview">Open a text file to preview it.</div>
              ) : (
                <>
                  <div className="preview-title">{preview.name}</div>
                  {preview.loading ? (
                    <div className="empty-preview">Loading…</div>
                  ) : preview.error !== null ? (
                    <div className="preview-error">{preview.error}</div>
                  ) : (
                    <pre>{preview.text}</pre>
                  )}
                </>
              )}
            </div>
          </div>
        </section>
      </main>
    </div>
  );
}
