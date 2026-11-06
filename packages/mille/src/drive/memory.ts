// createMemoryDriveNetwork — an in-process DriveTransport for tests, mille's
// and its hosts'.
//
// Every drive in the network sees every other one, until `partition` cuts a
// link. Frames are copied and delivered on a later turn of the event loop, in
// the order sent; a frame over a cut link is dropped, as a real link drops
// one. A pull copies the holder's object to the path the drive names, found
// the way a real transport finds it: the holder's `objectsRoot()` joined with
// `objectRelpath(blake3)`.

import { copyFile } from 'node:fs/promises';
import { join } from 'node:path';

import { objectRelpath } from './layout.js';
import type { DrivePeerEvent, DriveTransport } from './types.js';

type FrameListener = (peer: string, frame: Uint8Array) => void;
type PeerListener = (event: DrivePeerEvent) => void;

interface Member {
  readonly objectsRoot: string;
  readonly frameListeners: Set<FrameListener>;
  readonly peerListeners: Set<PeerListener>;
}

export interface MemoryDriveNetwork {
  /**
   * Join `peer` to the network. `holder` is its drive (anything with
   * `objectsRoot()`), where the others pull its objects from.
   */
  transportFor(peer: string, holder: { objectsRoot(): string }): DriveTransport;
  /** Cut (`true`) or restore (`false`) the link between `a` and `b`. */
  partition(a: string, b: string, cut: boolean): void;
}

function linkKey(a: string, b: string): string {
  return a < b ? `${a}\u0000${b}` : `${b}\u0000${a}`;
}

/** Run a listener; its bug stays in it, as it would across a socket. */
function safely(run: () => void): void {
  try {
    run();
  } catch {
    // Swallowed on purpose.
  }
}

/** A network of drives in one process. */
export function createMemoryDriveNetwork(): MemoryDriveNetwork {
  const members = new Map<string, Member>();
  const cut = new Set<string>();

  const linked = (a: string, b: string): boolean =>
    a !== b && members.has(a) && members.has(b) && !cut.has(linkKey(a, b));

  /** Tell `to`'s listeners, as they are now, on a later turn. */
  const tell = (to: string, event: DrivePeerEvent): void => {
    const listeners = [...(members.get(to)?.peerListeners ?? [])];
    setImmediate(() => {
      for (const listener of listeners) safely(() => listener(event));
    });
  };

  const transportFor = (peer: string, holder: { objectsRoot(): string }): DriveTransport => {
    if (members.has(peer)) throw new Error(`memory drive network: ${peer} has already joined`);
    const member: Member = {
      objectsRoot: holder.objectsRoot(),
      frameListeners: new Set(),
      peerListeners: new Set(),
    };
    members.set(peer, member);
    for (const other of members.keys()) {
      if (linked(peer, other)) tell(other, { peer, state: 'up' });
    }
    return {
      send(to: string, frame: Uint8Array): void {
        if (!linked(peer, to)) return;
        const copy = new Uint8Array(frame);
        setImmediate(() => {
          if (!linked(peer, to)) return;
          const listeners = [...(members.get(to)?.frameListeners ?? [])];
          for (const listener of listeners) safely(() => listener(peer, copy));
        });
      },
      async pull(from: string, blake3: string, intoPath: string): Promise<{ path: string }> {
        const holderMember = members.get(from);
        if (holderMember === undefined || !linked(peer, from)) {
          throw new Error(`memory drive network: ${from} is not reachable from ${peer}`);
        }
        await copyFile(join(holderMember.objectsRoot, objectRelpath(blake3)), intoPath);
        return { path: intoPath };
      },
      onFrame(listener: FrameListener): () => void {
        member.frameListeners.add(listener);
        return () => {
          member.frameListeners.delete(listener);
        };
      },
      onPeer(listener: PeerListener): () => void {
        member.peerListeners.add(listener);
        const up = [...members.keys()].filter((other) => linked(peer, other));
        setImmediate(() => {
          for (const other of up) {
            if (!member.peerListeners.has(listener)) return;
            safely(() => listener({ peer: other, state: 'up' }));
          }
        });
        return () => {
          member.peerListeners.delete(listener);
        };
      },
    };
  };

  const partition = (a: string, b: string, isCut: boolean): void => {
    const key = linkKey(a, b);
    if (isCut === cut.has(key)) return;
    if (isCut) cut.add(key);
    else cut.delete(key);
    if (!members.has(a) || !members.has(b)) return;
    const state = isCut ? 'down' : 'up';
    tell(a, { peer: b, state });
    tell(b, { peer: a, state });
  };

  return { transportFor, partition };
}
