// syncDrive — the host's sync loop, joining a Drive's sync core to a
// DriveTransport.
//
// The protocol lives in Rust (crates/mille-drive/src/sync.rs) as a core with
// no I/O of its own: frames and events in, actions out. This loop does the
// I/O: it sends frames, runs pulls into the drive's staging and hands back
// where each landed, and says honestly where sync stands (DR-L8). Which
// holder to pull from, how many pulls at once, and when to retry are the
// core's decisions; the loop only reports what happened.

import { isDriveError } from './error.js';
import type { Drive } from './engine.js';
import type {
  Blake3Hex,
  DriveErrorCode,
  DrivePeerEvent,
  DriveSyncAction,
  DriveSyncCounts,
  DriveSyncError,
  DriveSyncState,
  DriveSyncStatus,
  DriveTransport,
} from './types.js';

/** How often the loop nudges the core (re-greets peers, retries failed pulls). */
export const DEFAULT_DRIVE_TICK_MS = 10_000;

export interface SyncDriveOptions {
  /** The tick period in ms; `0` turns the tick off. Default 10 s. */
  readonly tickMs?: number;
}

export interface DriveSync {
  readonly status: DriveSyncStatus;
  /** Hear each new status. Returns the unsubscriber. */
  onStatus(listener: (status: DriveSyncStatus) => void): () => void;
  /**
   * Resolves once nothing this loop started is in flight: every send handed
   * over and every pull admitted or failed. Not a promise that every want is
   * met — a want no peer holds stays `waiting`.
   */
  settled(): Promise<void>;
  /** Stop: unsubscribe, stop the tick, close every peer on the drive. */
  dispose(): void;
}

/** Run `drive`'s sync over `transport` until disposed. */
export function syncDrive(
  drive: Drive,
  transport: DriveTransport,
  options: SyncDriveOptions = {},
): DriveSync {
  return new DriveSyncLoop(drive, transport, options);
}

const IDLE: DriveSyncStatus = {
  state: 'idle',
  peers: [],
  pulling: 0,
  wants: 0,
  unsourced: 0,
  lastError: null,
};

function stateOf(counts: DriveSyncCounts): DriveSyncState {
  if (counts.pulling > 0) return 'copying';
  return counts.wants > 0 ? 'waiting' : 'idle';
}

function sameStatus(a: DriveSyncStatus, b: DriveSyncStatus): boolean {
  return JSON.stringify(a) === JSON.stringify(b);
}

class DriveSyncLoop implements DriveSync {
  readonly #drive: Drive;
  readonly #transport: DriveTransport;
  readonly #offs: Array<() => void> = [];
  readonly #listeners = new Set<(status: DriveSyncStatus) => void>();
  readonly #peers = new Set<string>();
  #timer: ReturnType<typeof setInterval> | null = null;
  #pending = 0;
  #waiters: Array<() => void> = [];
  #lastError: DriveSyncError | null = null;
  #status: DriveSyncStatus = IDLE;
  #disposed = false;

  constructor(drive: Drive, transport: DriveTransport, options: SyncDriveOptions) {
    this.#drive = drive;
    this.#transport = transport;
    this.#offs.push(drive.onChange((change) => this.#onChange(change.kind)));
    this.#offs.push(transport.onFrame((peer, frame) => this.#onFrame(peer, frame)));
    this.#offs.push(transport.onPeer((event) => this.#onPeer(event)));
    const tickMs = options.tickMs ?? DEFAULT_DRIVE_TICK_MS;
    if (tickMs > 0) {
      const timer = setInterval(() => this.#step(null, () => drive.syncTick()), tickMs);
      // A sync loop never keeps a process alive on its own.
      timer.unref();
      this.#timer = timer;
    }
    this.#refresh();
  }

  get status(): DriveSyncStatus {
    return this.#status;
  }

  onStatus(listener: (status: DriveSyncStatus) => void): () => void {
    this.#listeners.add(listener);
    return () => {
      this.#listeners.delete(listener);
    };
  }

  settled(): Promise<void> {
    if (this.#pending === 0) return Promise.resolve();
    return new Promise((resolve) => {
      this.#waiters.push(resolve);
    });
  }

  dispose(): void {
    if (this.#disposed) return;
    this.#disposed = true;
    if (this.#timer !== null) clearInterval(this.#timer);
    this.#timer = null;
    for (const off of this.#offs.splice(0)) off();
    for (const peer of this.#peers) {
      try {
        this.#drive.syncClose(peer);
      } catch {
        // The drive was disposed first; it has no peers left to close.
      }
    }
    this.#peers.clear();
    this.#listeners.clear();
  }

  #onChange(kind: 'local' | 'import' | 'held'): void {
    if (kind === 'local') {
      this.#step(null, () => this.#drive.syncLocalChanged());
    } else {
      this.#refresh();
    }
  }

  #onFrame(peer: string, frame: Uint8Array): void {
    this.#step(peer, () => this.#drive.syncHandle(peer, frame));
  }

  #onPeer(event: DrivePeerEvent): void {
    if (this.#disposed) return;
    if (event.state === 'up') {
      this.#peers.add(event.peer);
      this.#step(event.peer, () => this.#drive.syncOpen(event.peer));
      return;
    }
    this.#peers.delete(event.peer);
    try {
      this.#drive.syncClose(event.peer);
    } catch (e: unknown) {
      this.#fail(e, event.peer, 'EIO');
    }
    this.#refresh();
  }

  /** Ask the core, run what it says, and say where sync now stands. */
  #step(peer: string | null, ask: () => DriveSyncAction[]): void {
    if (this.#disposed) return;
    let actions: DriveSyncAction[];
    try {
      actions = ask();
    } catch (e: unknown) {
      this.#fail(e, peer, 'EIO');
      this.#refresh();
      return;
    }
    this.#run(actions);
    this.#refresh();
  }

  #run(actions: readonly DriveSyncAction[]): void {
    for (const action of actions) {
      if (action.kind === 'send') {
        this.#send(action.peer, action.frame);
      } else {
        this.#pull(action.peer, action.blake3, action.into);
      }
    }
  }

  /** Hand a frame over, in order: the transport keeps per-peer order. */
  #send(peer: string, frame: Uint8Array): void {
    let sent: void | Promise<void>;
    try {
      sent = this.#transport.send(peer, frame);
    } catch (e: unknown) {
      this.#fail(e, peer, 'ETRANSPORT');
      return;
    }
    this.#track(
      Promise.resolve(sent).catch((e: unknown) => {
        this.#fail(e, peer, 'ETRANSPORT');
      }),
    );
  }

  /**
   * Fetch an object and hand the core where it landed. Either failure — the
   * transport's, or the core refusing the copy — goes back as
   * `syncPullFailed`, and the core picks the next holder.
   */
  #pull(peer: string, blake3: Blake3Hex, into: string): void {
    this.#track(this.#pullOnce(peer, blake3, into));
  }

  async #pullOnce(peer: string, blake3: Blake3Hex, into: string): Promise<void> {
    let landed: string;
    try {
      landed = (await this.#transport.pull(peer, blake3, into)).path;
    } catch (e: unknown) {
      this.#fail(e, peer, 'ETRANSPORT');
      this.#step(peer, () => this.#drive.syncPullFailed(peer, blake3));
      return;
    }
    let actions: DriveSyncAction[];
    try {
      actions = await this.#drive.syncPulled(peer, blake3, landed);
    } catch (e: unknown) {
      this.#fail(e, peer, 'EIO');
      this.#step(peer, () => this.#drive.syncPullFailed(peer, blake3));
      return;
    }
    if (this.#disposed) return;
    this.#run(actions);
    this.#refresh();
  }

  #track(work: Promise<void>): void {
    this.#pending += 1;
    void work
      .catch((e: unknown) => {
        this.#fail(e, null, 'EIO');
      })
      .finally(() => {
        this.#pending -= 1;
        this.#refresh();
        if (this.#pending > 0) return;
        for (const resolve of this.#waiters.splice(0)) resolve();
      });
  }

  #fail(e: unknown, peer: string | null, fallback: DriveErrorCode): void {
    const code = isDriveError(e) ? e.code : fallback;
    const message = e instanceof Error ? e.message : String(e);
    this.#lastError = peer === null ? { code, message } : { code, message, peer };
  }

  #refresh(): void {
    if (this.#disposed) return;
    let counts: DriveSyncCounts;
    try {
      counts = this.#drive.syncStatus();
    } catch {
      return;
    }
    const next: DriveSyncStatus = {
      state: stateOf(counts),
      peers: counts.peers,
      pulling: counts.pulling,
      wants: counts.wants,
      unsourced: counts.unsourced,
      lastError: this.#lastError,
    };
    if (sameStatus(next, this.#status)) return;
    this.#status = next;
    for (const listener of [...this.#listeners]) {
      try {
        listener(next);
      } catch {
        // A listener bug must not stop the loop.
      }
    }
  }
}
