// Minimal browser surface for running a DOM-using three.js game WITHOUT a browser (headless node, or the native
// JavaScriptCore host). Imported before the game, so its module-level `document.getElementById`, `location.search`,
// `matchMedia`, ... see it. Everything the game reads exists; nothing renders (the HUD is inert).
//
// The host supplies what can't be faked in JS, through globalThis.__worldHost (all optional):
//   readFile(path)            -> ArrayBuffer | null   (the game's public/ dir; fetch() and <img src> use it)
//   decodeImage(bytes, url)   -> { width, height, data: Uint8Array RGBA } | null
//   createCanvas(w, h)        -> a real 2D canvas (node: @napi-rs/canvas), or none: canvases are inert stubs whose
//                                pixels FastRenderer can't read (counted as 'texture source without pixels')
// Remote URLs (CDNs: Poly Haven, Hugging Face) are refused, so the run is reproducible and offline.
const G = globalThis;
const H = (G.__worldHost ||= {});
const stats = (G.__worldStats ||= { rtRenders: {}, frames: 0 });
stats.fetch = { local: 0, missing: 0, remote: 0, bytes: 0 };
stats.images = { decoded: 0, failed: 0 };
stats.canvases = { real: 0, stub: 0 };
stats.workers = 0;
const params = (G.__worldParams ||= {});

// ------------------------------------------------------------------ events
class Target {
  addEventListener(t, f) { ((this.__l ||= {})[t] ||= []).push(f); }
  removeEventListener(t, f) { const a = this.__l?.[t]; if (a) { const i = a.indexOf(f); if (i >= 0) a.splice(i, 1); } }
  dispatchEvent(e) { e.target ||= this; for (const f of [...(this.__l?.[e.type] || [])]) f.call(this, e); const h = this['on' + e.type]; if (typeof h === 'function') h.call(this, e); return true; }
}
class Ev { constructor(type, o = {}) { Object.assign(this, o); this.type = type; } preventDefault() {} stopPropagation() {} }
G.Event ||= Ev; G.CustomEvent ||= Ev;

// ------------------------------------------------------------------ elements
const noop = () => {};
class ClassList { constructor() { this.s = new Set(); } add(...c) { c.forEach((x) => this.s.add(x)); } remove(...c) { c.forEach((x) => this.s.delete(x)); } toggle(c, f) { const on = f ?? !this.s.has(c); on ? this.s.add(c) : this.s.delete(c); return on; } contains(c) { return this.s.has(c); } }
class Element extends Target {
  constructor(tag = 'div') {
    super();
    this.tagName = tag.toUpperCase(); this.nodeName = this.tagName; this.style = { setProperty: noop, removeProperty: noop }; this.classList = new ClassList(); this.dataset = {};
    this.children = []; this.childNodes = this.children; this.parentNode = null; this.parentElement = null; this.attributes = {};
    this.textContent = ''; this.innerHTML = ''; this.innerText = ''; this.value = ''; this.id = ''; this.className = '';
    this.width = 0; this.height = 0; this.clientWidth = params.width || 1280; this.clientHeight = params.height || 800; this.offsetWidth = this.clientWidth; this.offsetHeight = this.clientHeight;
  }
  appendChild(c) { this.children.push(c); c.parentNode = c.parentElement = this; return c; }
  append(...c) { c.forEach((x) => typeof x === 'object' && this.appendChild(x)); }
  prepend(...c) { this.append(...c); }
  insertBefore(c) { return this.appendChild(c); }
  replaceChildren(...c) { this.children.length = 0; this.append(...c); }
  removeChild(c) { const i = this.children.indexOf(c); if (i >= 0) this.children.splice(i, 1); c.parentNode = null; return c; }
  remove() { this.parentNode?.removeChild(this); }
  setAttribute(k, v) { this.attributes[k] = String(v); }
  getAttribute(k) { return this.attributes[k] ?? null; }
  hasAttribute(k) { return k in this.attributes; }
  removeAttribute(k) { delete this.attributes[k]; }
  querySelector() { return new Element(); }
  querySelectorAll() { return []; }
  getElementsByTagName() { return []; }
  closest() { return null; }
  contains() { return false; }
  getBoundingClientRect() { return { left: 0, top: 0, x: 0, y: 0, width: this.clientWidth, height: this.clientHeight, right: this.clientWidth, bottom: this.clientHeight }; }
  insertAdjacentHTML() {} insertAdjacentElement(_p, e) { return this.appendChild(e); } insertAdjacentText() {} after() {} before() {} replaceWith() {} toggleAttribute() {} matches() { return false; } hasChildNodes() { return this.children.length > 0; }
  get firstChild() { return this.children[0] || null; } get lastChild() { return this.children[this.children.length - 1] || null; } get firstElementChild() { return this.firstChild; } get childElementCount() { return this.children.length; } get isConnected() { return true; }
  focus() {} blur() {} click() {} scrollIntoView() {} animate() { return { finished: Promise.resolve(), cancel: noop }; }
  requestPointerLock() { return Promise.resolve(); }
  setPointerCapture() {} releasePointerCapture() {}
  cloneNode() { return new Element(this.tagName); }
}
const CTX_PROPS = new Set(['fillStyle', 'strokeStyle', 'font', 'lineWidth', 'lineCap', 'lineJoin', 'miterLimit', 'lineDashOffset', 'globalAlpha', 'globalCompositeOperation', 'textAlign', 'textBaseline', 'direction', 'shadowBlur', 'shadowColor', 'shadowOffsetX', 'shadowOffsetY', 'filter', 'imageSmoothingEnabled', 'imageSmoothingQuality', 'letterSpacing', 'then']);
// a canvas with no pixels (JavaScriptCore): 2D calls are no-ops, getImageData is blank
const ctx2d = (canvas) => new Proxy({ canvas, measureText: (t) => ({ width: String(t).length * 6, actualBoundingBoxAscent: 8, actualBoundingBoxDescent: 2 }), getImageData: (x, y, w, h) => ({ width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }), createImageData: (w, h) => ({ width: w, height: h, data: new Uint8ClampedArray(w * h * 4) }), createLinearGradient: () => ({ addColorStop: noop }), createRadialGradient: () => ({ addColorStop: noop }), createPattern: () => ({}), getTransform: () => ({ a: 1, b: 0, c: 0, d: 1, e: 0, f: 0 }), isPointInPath: () => false }, {
  get: (t, k) => (k in t ? t[k] : typeof k === 'string' && !CTX_PROPS.has(k) ? noop : t[k]),
  set: (t, k, v) => { t[k] = v; return true; },
});
class HTMLCanvasElement extends Element {
  constructor() { super('canvas'); this.width = 300; this.height = 150; }
  getContext(kind) { return kind === '2d' ? (this.__ctx ||= ctx2d(this)) : null; }
  toDataURL() { return 'data:,'; }
  toBlob(cb) { cb(new Blob([])); }
  transferControlToOffscreen() { return this; }
}
// images decoded by the host are plain pixel records (image.data): the native 2D context draws them via a canvas copy
const nativeSource = (img) => {
  if (!img || !ArrayBuffer.isView(img.data) || typeof img.getContext === 'function') return img;
  if (!img.__canvas && H.imageCanvas) img.__canvas = H.imageCanvas(img.width, img.height, img.data);
  return img.__canvas || img;
};
const patch2d = (x) => {
  if (!x || x.__fr) return x;
  x.__fr = true;
  const di = x.drawImage.bind(x), cp = x.createPattern.bind(x);
  x.drawImage = (img, ...a) => di(nativeSource(img), ...a);
  x.createPattern = (img, rep) => cp(nativeSource(img), rep);
  return x;
};
const realCanvas = (w = 300, h = 150) => {
  const c = H.createCanvas(w, h);
  // make it look like an element (style, events) without hiding its native 2D API
  if (!c.style) { c.style = {}; c.addEventListener = noop; c.removeEventListener = noop; c.getBoundingClientRect = () => ({ left: 0, top: 0, width: w, height: h }); c.dataset = {}; c.classList = new ClassList(); c.requestPointerLock = () => Promise.resolve(); c.focus = noop; c.setPointerCapture = noop; c.releasePointerCapture = noop; }
  const gc = c.getContext.bind(c);
  c.getContext = (kind, o) => (kind === '2d' ? patch2d(gc('2d', o)) : null);
  return c;
};
const newCanvas = (w, h) => { if (H.createCanvas) { stats.canvases.real++; return realCanvas(w, h); } stats.canvases.stub++; const c = new HTMLCanvasElement(); if (w) { c.width = w; c.height = h; } return c; };
G.OffscreenCanvas ||= function OffscreenCanvas(w, h) { return newCanvas(w, h); };

// ------------------------------------------------------------------ files, blobs, images
const blobs = new Map(); let blobN = 0;
class MiniBlob { constructor(parts = [], o = {}) { this.type = o.type || ''; const bufs = parts.map((p) => (typeof p === 'string' ? new TextEncoder().encode(p) : p instanceof ArrayBuffer ? new Uint8Array(p) : ArrayBuffer.isView(p) ? new Uint8Array(p.buffer, p.byteOffset, p.byteLength) : p.__bytes || new Uint8Array(0))); const n = bufs.reduce((a, b) => a + b.length, 0); this.__bytes = new Uint8Array(n); let o2 = 0; for (const b of bufs) { this.__bytes.set(b, o2); o2 += b.length; } this.size = n; } arrayBuffer() { return Promise.resolve(this.__bytes.slice().buffer); } text() { return Promise.resolve(new TextDecoder().decode(this.__bytes)); } }
if (!G.Blob || __WORLD_TARGET__ === 'jsc') G.Blob = MiniBlob;
const bytesOf = (b) => (b.__bytes ? Promise.resolve(b.__bytes) : b.arrayBuffer().then((a) => new Uint8Array(a)));
const U = G.URL;
const createObjectURL = (b) => { const u = `blob:fr/${++blobN}`; blobs.set(u, b); return u; };
if (U) { U.createObjectURL = createObjectURL; U.revokeObjectURL = (u) => blobs.delete(u); }

/** url -> local path under the game's public/ (null for remote) */
function localPath(url) {
  let u = String(url);
  if (/^https?:\/\/localhost(:\d+)?\//.test(u)) u = u.replace(/^https?:\/\/localhost(:\d+)?/, '');
  if (/^[a-z]+:/i.test(u)) return null;
  return decodeURIComponent(u.replace(/^\.?\//, '').split(/[?#]/)[0]);
}
async function load(url) {
  if (String(url).startsWith('blob:')) { const b = blobs.get(String(url)); return b ? bytesOf(b) : null; }
  if (String(url).startsWith('data:')) { const m = /^data:([^,]*?)(;base64)?,(.*)$/s.exec(url); if (!m) return null; return m[2] ? Uint8Array.from(atob(m[3]), (c) => c.charCodeAt(0)) : new TextEncoder().encode(decodeURIComponent(m[3])); }
  const p = localPath(url);
  if (p === null) { stats.fetch.remote++; return undefined; }
  const ab = H.readFile ? H.readFile(p) : null;
  if (!ab) { stats.fetch.missing++; (stats.fetch.missingList ||= []).length < 20 && stats.fetch.missingList.push(p); return null; }
  stats.fetch.local++; stats.fetch.bytes += ab.byteLength;
  return new Uint8Array(ab);
}
class Response_ {
  constructor(bytes, url, status = 200) { this.__b = bytes; this.url = url; this.status = status; this.ok = status >= 200 && status < 300; this.statusText = this.ok ? 'OK' : 'Not Found'; this.headers = { get: (k) => (/length/i.test(k) ? String(bytes.length) : null), has: () => false }; this.body = undefined; this.bodyUsed = false; }
  arrayBuffer() { return Promise.resolve(this.__b.slice().buffer); }
  text() { return Promise.resolve(new TextDecoder().decode(this.__b)); }
  json() { return this.text().then(JSON.parse); }
  blob() { return Promise.resolve(new G.Blob([this.__b])); }
  clone() { return new Response_(this.__b, this.url, this.status); }
}
G.fetch = async (input) => {
  const url = typeof input === 'string' ? input : input.url || String(input);
  const b = await load(url);
  if (b === undefined) throw new TypeError(`fetch blocked (remote, offline run): ${url}`);
  return b ? new Response_(b, url) : new Response_(new Uint8Array(0), url, 404);
};
G.Request = class { constructor(url, o = {}) { this.url = url; Object.assign(this, o); } };
G.Headers = class { constructor() {} get() { return null; } has() { return false; } set() {} };

/** decoded pixels as an element-like image FastRenderer reads directly (image.data RGBA) */
function decodeTo(target, bytes, url, flip) {
  const r = bytes && H.decodeImage ? H.decodeImage(bytes, url) : null;
  if (!r) { stats.images.failed++; return false; }
  stats.images.decoded++;
  let data = r.data;
  if (flip) { const row = r.width * 4, out = new Uint8Array(data.length); for (let y = 0; y < r.height; y++) out.set(data.subarray(y * row, (y + 1) * row), (r.height - 1 - y) * row); data = out; }
  target.width = target.naturalWidth = r.width; target.height = target.naturalHeight = r.height; target.data = data;
  return true;
}
class HTMLImageElement extends Element {
  constructor() { super('img'); this.complete = false; this.crossOrigin = null; this.decoding = 'auto'; }
  get src() { return this.__src || ''; }
  set src(url) {
    this.__src = url;
    load(url).then((b) => {
      if (decodeTo(this, b, url, false)) { this.complete = true; this.dispatchEvent(new Ev('load')); } else this.dispatchEvent(new Ev('error'));
    }, () => this.dispatchEvent(new Ev('error')));
  }
  decode() { return Promise.resolve(); }
}
class ImageBitmap { close() { this.data = null; } }
G.createImageBitmap = async (src, ...rest) => {
  const o = rest.length >= 4 ? rest[4] || {} : rest[0] || {};
  const flip = o.imageOrientation === 'flipY';
  const bm = new ImageBitmap();
  if (src && src.data && src.width) { bm.width = src.width; bm.height = src.height; bm.data = src.data; return bm; }
  const bytes = src && (src.__bytes || src.arrayBuffer) ? await bytesOf(src) : null;
  if (!decodeTo(bm, bytes, 'blob', flip)) throw new Error('createImageBitmap: undecodable image');
  return bm;
};
G.HTMLCanvasElement ||= HTMLCanvasElement; G.HTMLImageElement ||= HTMLImageElement; G.ImageBitmap ||= ImageBitmap;
G.HTMLVideoElement ||= class extends Element {}; G.HTMLElement ||= Element; G.Element ||= Element; G.Node ||= Element;
G.Image ||= HTMLImageElement; G.ImageData ||= class { constructor(d, w, h) { if (typeof d === 'number') { h = w; w = d; d = new Uint8ClampedArray(w * h * 4); } this.data = d; this.width = w; this.height = h; } };

// ------------------------------------------------------------------ document / window
const byId = new Map();
const body = new Element('body');
const document = Object.assign(new Target(), {
  body, head: new Element('head'), documentElement: new Element('html'), hidden: false, visibilityState: 'visible', readyState: 'complete', pointerLockElement: null, fullscreenElement: null, cookie: '',
  getElementById(id) { let e = byId.get(id); if (!e) { e = new Element('div'); e.id = id; byId.set(id, e); } return e; },
  createElement(tag) { return tag === 'canvas' ? newCanvas() : tag === 'img' ? new HTMLImageElement() : new Element(tag); },
  createElementNS(_ns, tag) { return this.createElement(tag); },
  createTextNode(t) { return { textContent: t }; },
  createDocumentFragment() { return new Element('fragment'); },
  querySelector() { return new Element(); }, querySelectorAll() { return []; }, getElementsByTagName() { return []; },
  exitPointerLock() {}, exitFullscreen() { return Promise.resolve(); }, hasFocus: () => true,
  fonts: { ready: Promise.resolve(), load: () => Promise.resolve([]), add: noop, check: () => true },
});
G.document = document;
G.window = G; G.self = G; G.top = G; G.parent = G;
for (const k of ['addEventListener', 'removeEventListener', 'dispatchEvent']) if (typeof G[k] !== 'function') G[k] = Target.prototype[k].bind(G);
const w = params.width || 1280, h = params.height || 800;
Object.assign(G, { innerWidth: w, innerHeight: h, outerWidth: w, outerHeight: h, devicePixelRatio: 1, screen: { width: w, height: h, availWidth: w, availHeight: h } });
const search = params.search || '';
G.location = { search, hash: '', pathname: '/', href: `http://localhost/${search}`, origin: 'http://localhost', host: 'localhost', hostname: 'localhost', protocol: 'http:', port: '', reload: noop, assign: noop, replace: noop };
G.history = { state: null, replaceState: noop, pushState: noop, back: noop };
try { Object.defineProperty(G, 'navigator', { value: { userAgent: 'Mozilla/5.0 (Macintosh) fastrender', platform: 'MacIntel', language: 'en-US', languages: ['en-US'], hardwareConcurrency: 8, maxTouchPoints: 0, deviceMemory: 8, onLine: false, clipboard: { writeText: () => Promise.resolve() }, getGamepads: () => [], userActivation: { isActive: false } }, configurable: true, writable: true }); } catch {}
G.matchMedia = (q) => ({ matches: false, media: q, addEventListener: noop, removeEventListener: noop, addListener: noop, removeListener: noop });
G.getComputedStyle = () => ({ getPropertyValue: () => '' });
G.localStorage = G.sessionStorage = (() => { const m = new Map(); return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), removeItem: (k) => m.delete(k), clear: () => m.clear(), key: () => null, get length() { return m.size; } }; })();
G.ResizeObserver ||= class { observe() {} unobserve() {} disconnect() {} };
G.IntersectionObserver ||= class { observe() {} unobserve() {} disconnect() {} };
G.MutationObserver ||= class { observe() {} disconnect() {} };
G.AudioContext ||= class { constructor() { this.state = 'suspended'; this.currentTime = 0; this.destination = {}; } resume() { return Promise.resolve(); } createGain() { return { gain: { value: 1, setValueAtTime: noop, linearRampToValueAtTime: noop }, connect: noop }; } };
G.speechSynthesis ||= undefined;
// Web Workers (USDA parser pool, mesh simplifier): none headless; work posted to them never completes
G.Worker = class extends Target { constructor() { super(); stats.workers++; } postMessage() {} terminate() {} };
G.alert = noop; G.confirm = () => false; G.prompt = () => null; G.open = () => null; G.scrollTo = noop;
G.requestIdleCallback ||= (f) => setTimeout(() => f({ didTimeout: false, timeRemaining: () => 8 }), 1);
G.cancelIdleCallback ||= (id) => clearTimeout(id);
