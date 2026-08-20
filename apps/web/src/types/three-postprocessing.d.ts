// `three` ships no bundled type declarations and is only present transitively
// (via react-force-graph-3d → 3d-force-graph). GraphView needs exactly one
// symbol from it — UnrealBloomPass, for the 3D glow — so declare that surface
// here rather than taking on @types/three and a direct three dependency, which
// would risk resolving a second copy alongside the one 3d-force-graph uses.
declare module 'three/examples/jsm/postprocessing/UnrealBloomPass.js' {
  /**
   * Only `.x` / `.y` are read off the resolution argument (it is copied into a
   * fresh Vector2 internally), so a plain object satisfies it.
   */
  export class UnrealBloomPass {
    constructor(
      resolution: { x: number; y: number },
      strength?: number,
      radius?: number,
      threshold?: number,
    );
    dispose(): void;
  }
}
