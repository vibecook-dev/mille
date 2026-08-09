import type { MeshDemoApi } from './types';

declare global {
  interface Window {
    readonly milleMesh: MeshDemoApi;
  }
}

export {};
