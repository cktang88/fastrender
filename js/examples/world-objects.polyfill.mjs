// Web-platform globals a bare JavaScriptCore context lacks (it has the language, Promise, typed arrays, WebAssembly;
// not the web APIs). Each is installed only when missing, so node and browsers keep their own. Small and
// spec-incomplete on purpose: enough for a game's URL parsing, text decoding (GLTFLoader's JSON chunk) and logging.
const G = globalThis;
const noop = () => {};

// console: JavaScriptCore's host defines only console.log
if (G.console) for (const k of ['warn', 'error', 'info', 'debug', 'trace', 'time', 'timeEnd', 'group', 'groupEnd', 'groupCollapsed', 'table', 'count', 'assert']) if (typeof G.console[k] !== 'function') G.console[k] = k === 'warn' || k === 'error' ? (...a) => G.console.log(`[${k}]`, ...a) : noop;

if (typeof G.TextDecoder === 'undefined') {
  G.TextDecoder = class {
    decode(b) {
      if (!b) return '';
      const u = b instanceof Uint8Array ? b : ArrayBuffer.isView(b) ? new Uint8Array(b.buffer, b.byteOffset, b.byteLength) : new Uint8Array(b);
      let s = '', i = 0;
      const n = u.length, chunk = [];
      while (i < n) {
        let c = u[i++];
        if (c >= 0xf0) c = ((c & 7) << 18) | ((u[i++] & 63) << 12) | ((u[i++] & 63) << 6) | (u[i++] & 63);
        else if (c >= 0xe0) c = ((c & 15) << 12) | ((u[i++] & 63) << 6) | (u[i++] & 63);
        else if (c >= 0xc0) c = ((c & 31) << 6) | (u[i++] & 63);
        if (c > 0xffff) { c -= 0x10000; chunk.push(0xd800 + (c >> 10), 0xdc00 + (c & 1023)); } else chunk.push(c);
        if (chunk.length > 8000) { s += String.fromCharCode.apply(null, chunk); chunk.length = 0; }
      }
      return s + String.fromCharCode.apply(null, chunk);
    }
  };
}
if (typeof G.TextEncoder === 'undefined') {
  G.TextEncoder = class {
    encode(s = '') {
      const out = [];
      for (let i = 0; i < s.length; i++) {
        let c = s.charCodeAt(i);
        if (c >= 0xd800 && c < 0xdc00 && i + 1 < s.length) c = 0x10000 + ((c - 0xd800) << 10) + (s.charCodeAt(++i) - 0xdc00);
        if (c < 0x80) out.push(c);
        else if (c < 0x800) out.push(0xc0 | (c >> 6), 0x80 | (c & 63));
        else if (c < 0x10000) out.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
        else out.push(0xf0 | (c >> 18), 0x80 | ((c >> 12) & 63), 0x80 | ((c >> 6) & 63), 0x80 | (c & 63));
      }
      return new Uint8Array(out);
    }
  };
}
const B64 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
if (typeof G.atob === 'undefined') {
  G.atob = (s) => { s = String(s).replace(/[^A-Za-z0-9+/]/g, ''); let o = '', b = 0, n = 0; for (const ch of s) { b = (b << 6) | B64.indexOf(ch); n += 6; if (n >= 8) { n -= 8; o += String.fromCharCode((b >> n) & 255); } } return o; };
  G.btoa = (s) => { let o = ''; for (let i = 0; i < s.length; i += 3) { const a = s.charCodeAt(i), b = s.charCodeAt(i + 1), c = s.charCodeAt(i + 2); o += B64[a >> 2] + B64[((a & 3) << 4) | (b >> 4 || 0)] + (i + 1 < s.length ? B64[((b & 15) << 2) | (c >> 6 || 0)] : '=') + (i + 2 < s.length ? B64[c & 63] : '='); } return o; };
}
if (typeof G.URLSearchParams === 'undefined') {
  G.URLSearchParams = class {
    constructor(init = '') {
      this._p = [];
      if (typeof init === 'string') for (const kv of init.replace(/^\?/, '').split('&')) { if (!kv) continue; const i = kv.indexOf('='); const d = (x) => decodeURIComponent(x.replace(/\+/g, ' ')); this._p.push(i < 0 ? [d(kv), ''] : [d(kv.slice(0, i)), d(kv.slice(i + 1))]); }
      else if (init && typeof init === 'object') for (const [k, v] of init[Symbol.iterator] ? init : Object.entries(init)) this._p.push([String(k), String(v)]);
    }
    get(k) { const e = this._p.find((p) => p[0] === k); return e ? e[1] : null; }
    getAll(k) { return this._p.filter((p) => p[0] === k).map((p) => p[1]); }
    has(k) { return this._p.some((p) => p[0] === k); }
    set(k, v) { this.delete(k); this._p.push([k, String(v)]); }
    append(k, v) { this._p.push([k, String(v)]); }
    delete(k) { this._p = this._p.filter((p) => p[0] !== k); }
    forEach(f) { for (const [k, v] of this._p) f(v, k, this); }
    keys() { return this._p.map((p) => p[0])[Symbol.iterator](); }
    values() { return this._p.map((p) => p[1])[Symbol.iterator](); }
    entries() { return this._p.map((p) => [p[0], p[1]])[Symbol.iterator](); }
    [Symbol.iterator]() { return this.entries(); }
    get size() { return this._p.length; }
    toString() { return this._p.map(([k, v]) => `${encodeURIComponent(k)}=${encodeURIComponent(v)}`).join('&'); }
  };
}
if (typeof G.URL === 'undefined') {
  G.URL = class {
    constructor(u, base) {
      u = String(u);
      if (!/^[a-z][a-z0-9+.-]*:/i.test(u)) {
        const b = base ? new G.URL(base) : { protocol: 'http:', host: 'localhost', pathname: '/' };
        const dir = b.pathname.replace(/[^/]*$/, '');
        const path = u.startsWith('/') ? u : dir + u;
        const parts = [];
        for (const seg of path.split(/(?=[?#])/)[0].split('/')) { if (seg === '..') parts.pop(); else if (seg !== '.') parts.push(seg); }
        u = `${b.protocol}//${b.host}${parts.join('/')}${(/[?#].*$/.exec(path) || [''])[0]}`;
      }
      const m = /^([a-z][a-z0-9+.-]*:)(?:\/\/([^/?#]*))?([^?#]*)(\?[^#]*)?(#.*)?$/i.exec(u) || [];
      this.protocol = m[1] || ''; this.host = this.hostname = m[2] || ''; this.port = ''; this.pathname = m[3] || '/'; this.search = m[4] || ''; this.hash = m[5] || '';
      this.origin = `${this.protocol}//${this.host}`; this.href = u; this.searchParams = new G.URLSearchParams(this.search);
    }
    toString() { return this.href; }
  };
}
G.AbortController ||= class { constructor() { this.signal = { aborted: false, addEventListener: noop, removeEventListener: noop }; } abort() { this.signal.aborted = true; } };
G.WeakRef ||= class { constructor(t) { this.t = t; } deref() { return this.t; } };
G.FinalizationRegistry ||= class { register() {} unregister() {} };
G.structuredClone ||= (v) => JSON.parse(JSON.stringify(v));

// the native host's file + image access (src/image.rs) as world-objects.dom.mjs expects it
if (G.__fr && typeof G.__fr.readFile === 'function') {
  const H = (G.__worldHost ||= {});
  H.readFile = (p) => G.__fr.readFile(p);
  H.decodeImage = (bytes) => {
    const ab = G.__fr.decodeImage(bytes instanceof Uint8Array ? bytes : new Uint8Array(bytes));
    if (!ab) return null;
    const dv = new DataView(ab);
    return { width: dv.getUint32(0, true), height: dv.getUint32(4, true), data: new Uint8Array(ab, 8) };
  };
}
