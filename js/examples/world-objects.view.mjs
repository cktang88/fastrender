// The camera view the golden and the measurements use, pinned once the game's world has started (same code in the
// browser and natively). params.view = { pos: [x, y, z], yaw, pitch } (player feet position; the game puts the
// camera 1.65 m above); without it the game's own spawn (the 5 Av sidewalk, seed 20261105) is kept.
const G = globalThis;
let pinned = false;
/** true once the world is running (the level is built and the start screen gone) */
export function pinView() {
  const W = G.__world, v = G.__worldParams && G.__worldParams.view;
  if (!W || !W.started) return false;
  if (!pinned) {
    pinned = true;
    if (v && W.player) {
      W.player.place(W.player.pos.clone().set(v.pos[0], v.pos[1], v.pos[2]), v.yaw ?? 0);
      W.player.pitch = v.pitch ?? 0;
    }
  }
  return true;
}
G.__worldPin = pinView;
