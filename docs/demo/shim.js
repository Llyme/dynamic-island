// Stands in for Tauri inside the demo iframe: the island's real frontend (demo/ui, a copy of ../ui) calls
// `window.__TAURI__` exactly as it does in the app. Commands go to the page that hosts it (backend.js, a
// port of the Rust side) and events come back the same way. Nothing here changes how the island behaves.
(() => {
  const host = window.parent.NADI;
  const listeners = new Map();

  window.__nadiEmit = (name, payload) => {
    const set = listeners.get(name);
    if (set) for (const cb of [...set]) cb({ event: name, payload, id: 0 });
  };

  const listen = (name, cb) => {
    if (!listeners.has(name)) listeners.set(name, new Set());
    listeners.get(name).add(cb);
    return Promise.resolve(() => listeners.get(name)?.delete(cb));
  };

  window.__TAURI__ = {
    core: {
      invoke: (cmd, args) => host.invoke(cmd, args || {}),
      convertFileSrc: (p) => p,
    },
    event: {
      listen,
      emit: (name, payload) => {
        if (name === "js-error") console.error("island:", payload);
        return Promise.resolve();
      },
    },
  };

  // the real cursor and mouse button are what the app reads from Windows: while the pointer is over the
  // island's own frame the page above no longer sees it, so the frame passes it up
  for (const type of ["pointermove", "pointerdown", "pointerup", "pointercancel"]) {
    window.addEventListener(type, (e) => host.framePointer(type, e), true);
  }
  window.addEventListener("blur", () => host.frameBlur?.());
})();
