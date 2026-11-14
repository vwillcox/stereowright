// Stereowright 3D photo viewer for the Meta Quest Browser (WebXR + WebGL2, no libraries).
// Each eye sees its own half of the side-by-side photo on a floating screen.
'use strict';

const POLL_MS = 3000;
const BOX_W = 2.2;      // default photo box (metres) …
const BOX_H = 1.5;
const DISTANCE = 2.0;   // … this far in front of you
const CAPTION_MS = 3500;

const $ = (id) => document.getElementById(id);
const ui = {
  grid: $('grid'), count: $('count'), enter: $('enter'), follow: $('follow'),
  pt: $('passthrough'), ptLabel: $('pt-label'), notice: $('notice'), empty: $('empty'),
  wifi: $('wifi'),
};

let photos = [];
let newestId = null;
let index = 0;
let xrModes = { vr: false, ar: false };

// ------------------------------------------------------------------ preferences

function pref(key, fallback) {
  try { const v = localStorage.getItem('ds.' + key); return v === null ? fallback : JSON.parse(v); }
  catch { return fallback; }
}
function savePref(key, value) {
  try { localStorage.setItem('ds.' + key, JSON.stringify(value)); } catch { /* private mode */ }
}
ui.follow.checked = pref('follow', true);
ui.pt.checked = pref('passthrough', false);
ui.follow.addEventListener('change', () => savePref('follow', ui.follow.checked));
ui.pt.addEventListener('change', () => savePref('passthrough', ui.pt.checked));

// ------------------------------------------------------------------ gallery (2D page)

function ago(ms) {
  const s = Math.max(0, (Date.now() - ms) / 1000);
  if (s < 60) return 'just now';
  if (s < 3600) return `${Math.floor(s / 60)} min ago`;
  if (s < 86400) return `${Math.floor(s / 3600)} h ago`;
  return new Date(ms).toLocaleDateString();
}

const LAYOUT_LABEL = { sbs: 'Side-by-side', hsbs: 'Half SBS', tb: 'Top/bottom' };

function renderGrid() {
  ui.count.textContent = photos.length ? `${photos.length} photo${photos.length === 1 ? '' : 's'}` : '';
  ui.empty.hidden = photos.length > 0;
  ui.enter.disabled = !photos.length || !(xrModes.vr || xrModes.ar);
  const existing = new Map([...ui.grid.children].map((el) => [el.dataset.id, el]));
  const frag = document.createDocumentFragment();
  photos.forEach((p, i) => {
    let card = existing.get(p.id);
    if (!card) {
      card = document.createElement('button');
      card.className = 'card';
      card.dataset.id = p.id;
      const img = document.createElement('img');
      img.className = 'thumb';
      img.loading = 'lazy';
      img.decoding = 'async';
      img.alt = '';
      img.src = `thumb/${p.id}`;
      img.addEventListener('error', () => {
        const ph = document.createElement('span');
        ph.className = 'thumb missing';
        ph.textContent = '3D photo';
        img.replaceWith(ph);
      }, { once: true });
      const meta = document.createElement('span');
      meta.className = 'meta';
      const name = document.createElement('span');
      name.className = 'name';
      name.textContent = p.name;
      const sub = document.createElement('span');
      sub.className = 'sub';
      meta.append(name, sub);
      card.append(img, meta);
      card.addEventListener('click', () => {
        const at = photos.findIndex((q) => q.id === card.dataset.id);
        startXR(at < 0 ? 0 : at);
      });
    }
    card.querySelector('.sub').textContent =
      `${ago(p.mtime)} · ${p.folder} · ${LAYOUT_LABEL[p.layout] || p.layout}`;
    card.classList.toggle('new', i === 0 && Date.now() - p.mtime < 60_000);
    frag.append(card);
  });
  ui.grid.replaceChildren(frag);
}

let polling = false;
async function poll() {
  if (polling) return;
  polling = true;
  try {
    const r = await fetch('api/photos', { cache: 'no-store' });
    if (!r.ok) throw new Error(`HTTP ${r.status}`);
    const data = await r.json();
    const currentId = photos[index]?.id;
    photos = data.photos;
    const top = photos[0]?.id ?? null;
    const isNew = newestId !== null && top !== null && top !== newestId;
    newestId = top;
    index = Math.max(0, photos.findIndex((p) => p.id === currentId));
    if (isNew && ui.follow.checked && xr.session) {
      show(0, 'New photo');
    }
    renderGrid();
    showWifi(data.wifi_url);
    if (ui.notice.dataset.kind === 'offline') setNotice('');
  } catch (e) {
    setNotice('Lost contact with Stereowright share on the computer. Is it still running?', 'offline');
  } finally {
    polling = false;
  }
}

function setNotice(text, kind = '') {
  ui.notice.textContent = text;
  ui.notice.hidden = !text;
  ui.notice.dataset.kind = kind;
}

function showWifi(url) {
  if (!url) { ui.wifi.hidden = true; return; }
  ui.wifi.replaceChildren(
    'Cable-free next time: open ',
    Object.assign(document.createElement('a'), { href: url, textContent: url }),
    ' once over Wi-Fi, accept the security warning (the certificate was made on your computer), and bookmark it.',
  );
  ui.wifi.hidden = false;
}

// ------------------------------------------------------------------ 4x4 matrices (column-major)

const M = {
  mul(a, b) {
    const o = new Float32Array(16);
    for (let c = 0; c < 4; c++) {
      for (let r = 0; r < 4; r++) {
        o[c * 4 + r] = a[r] * b[c * 4] + a[4 + r] * b[c * 4 + 1] + a[8 + r] * b[c * 4 + 2] + a[12 + r] * b[c * 4 + 3];
      }
    }
    return o;
  },
  // Translate to (x,y,z), turn by yaw around Y, scale (sx, sy).
  place(x, y, z, yaw, sx = 1, sy = 1) {
    const c = Math.cos(yaw), s = Math.sin(yaw);
    return new Float32Array([c * sx, 0, -s * sx, 0, 0, sy, 0, 0, s, 0, c, 0, x, y, z, 1]);
  },
  scaleXY(sx, sy, dy = 0, dz = 0) {
    return new Float32Array([sx, 0, 0, 0, 0, sy, 0, 0, 0, 0, 1, 0, 0, dy, dz, 1]);
  },
};

// ------------------------------------------------------------------ WebGL

const VS = `#version 300 es
in vec2 a_pos;
uniform mat4 u_mvp;
out vec2 v_uv;
void main() {
  v_uv = vec2(a_pos.x + 0.5, 0.5 - a_pos.y);
  gl_Position = u_mvp * vec4(a_pos, 0.0, 1.0);
}`;
const FS = `#version 300 es
precision mediump float;
in vec2 v_uv;
uniform sampler2D u_tex;
uniform float u_alpha;
out vec4 o;
void main() {
  vec4 c = texture(u_tex, v_uv);
  o = vec4(c.rgb, c.a * u_alpha);
}`;

const xr = {
  session: null, gl: null, prog: null, vao: null, ref: null, aniso: null, maxTex: 4096,
  anchor: null, size: 1, needRecenter: true, lastPoll: 0, lastT: 0,
  photo: null, loading: 0, cache: new Map(), pending: new Map(),
  caption: { tex: null, aspect: 6, shownAt: -1e9, sticky: false, key: '' },
  sticks: {},
};

function initGL() {
  if (xr.gl) return xr.gl;
  const canvas = document.createElement('canvas');
  const gl = canvas.getContext('webgl2', { xrCompatible: true, alpha: true, antialias: false });
  if (!gl) throw new Error('WebGL2 is not available');
  const sh = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  const p = gl.createProgram();
  gl.attachShader(p, sh(gl.VERTEX_SHADER, VS));
  gl.attachShader(p, sh(gl.FRAGMENT_SHADER, FS));
  gl.bindAttribLocation(p, 0, 'a_pos');
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  xr.prog = { p, mvp: gl.getUniformLocation(p, 'u_mvp'), tex: gl.getUniformLocation(p, 'u_tex'),
    alpha: gl.getUniformLocation(p, 'u_alpha') };
  xr.vao = gl.createVertexArray();
  gl.bindVertexArray(xr.vao);
  const buf = gl.createBuffer();
  gl.bindBuffer(gl.ARRAY_BUFFER, buf);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-0.5, -0.5, 0.5, -0.5, -0.5, 0.5, 0.5, 0.5]), gl.STATIC_DRAW);
  gl.enableVertexAttribArray(0);
  gl.vertexAttribPointer(0, 2, gl.FLOAT, false, 0, 0);
  xr.aniso = gl.getExtension('EXT_texture_filter_anisotropic');
  xr.maxTex = gl.getParameter(gl.MAX_TEXTURE_SIZE);
  xr.gl = gl;
  return gl;
}

function upload(source, mip = true) {
  const gl = xr.gl;
  const t = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, t);
  gl.pixelStorei(gl.UNPACK_FLIP_Y_WEBGL, false);
  gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, false);
  gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, source);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  if (mip) {
    gl.generateMipmap(gl.TEXTURE_2D);
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR_MIPMAP_LINEAR);
    if (xr.aniso) {
      const max = gl.getParameter(xr.aniso.MAX_TEXTURE_MAX_ANISOTROPY_EXT);
      gl.texParameterf(gl.TEXTURE_2D, xr.aniso.TEXTURE_MAX_ANISOTROPY_EXT, Math.min(8, max));
    }
  } else {
    gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  }
  return t;
}

// Load a photo (once, even if asked twice at the same time).
function loadPhoto(p) {
  if (xr.cache.has(p.id)) return Promise.resolve(xr.cache.get(p.id));
  if (!xr.pending.has(p.id)) {
    const job = fetchPhoto(p).finally(() => xr.pending.delete(p.id));
    xr.pending.set(p.id, job);
  }
  return xr.pending.get(p.id);
}

// Download a photo and split it into one texture per eye.
async function fetchPhoto(p) {
  const r = await fetch(`photo/${p.id}`);
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  const full = await createImageBitmap(await r.blob());
  const W = full.width, H = full.height;
  let eyes, aspect; // eyes: [sx, sy, sw, sh] for left, right
  if (p.layout === 'tb') {
    const h = Math.floor(H / 2);
    eyes = [[0, 0, W, h], [0, h, W, h]];
    aspect = W / h;
  } else {
    const w = Math.floor(W / 2);
    eyes = [[0, 0, w, H], [w, 0, w, H]];
    aspect = p.layout === 'hsbs' ? W / H : w / H;
  }
  const tex = [];
  for (const [sx, sy, sw, sh] of eyes) {
    const k = Math.min(1, xr.maxTex / Math.max(sw, sh));
    const opts = k < 1
      ? { resizeWidth: Math.floor(sw * k), resizeHeight: Math.floor(sh * k), resizeQuality: 'high' }
      : {};
    const bmp = await createImageBitmap(full, sx, sy, sw, sh, opts);
    tex.push(upload(bmp));
    bmp.close();
  }
  full.close();
  if (!xr.session) { tex.forEach((t) => xr.gl.deleteTexture(t)); throw new Error('VR ended'); }
  const entry = { id: p.id, tex, aspect };
  xr.cache.set(p.id, entry);
  // Keep the current photo and its neighbours; free the rest of the GPU memory.
  const keep = new Set([-1, 0, 1].map((d) => photos[(index + d + photos.length) % photos.length]?.id));
  keep.add(p.id);
  if (xr.photo) keep.add(xr.photo.id);
  for (const [id, e] of xr.cache) {
    if (!keep.has(id)) { e.tex.forEach((t) => xr.gl.deleteTexture(t)); xr.cache.delete(id); }
  }
  return entry;
}

function setCaption(lines, sticky = false) {
  const key = lines.join('\n');
  xr.caption.shownAt = performance.now();
  xr.caption.sticky = sticky;
  if (key === xr.caption.key && xr.caption.tex) return;
  xr.caption.key = key;
  const c = document.createElement('canvas');
  c.width = 1024; c.height = 44 + 76 * lines.length;
  const g = c.getContext('2d');
  g.fillStyle = 'rgba(12,14,18,0.78)';
  if (g.roundRect) { g.beginPath(); g.roundRect(0, 0, c.width, c.height, 30); g.fill(); }
  else g.fillRect(0, 0, c.width, c.height);
  g.textAlign = 'center';
  g.textBaseline = 'middle';
  lines.forEach((line, i) => {
    g.fillStyle = i === 0 ? '#f2f5f9' : '#aab3c2';
    g.font = `${i === 0 ? 600 : 400} ${i === 0 ? 62 : 50}px system-ui, sans-serif`;
    g.fillText(line, c.width / 2, 22 + 76 * i + 38, c.width - 60);
  });
  if (xr.caption.tex) xr.gl.deleteTexture(xr.caption.tex);
  xr.caption.tex = upload(c, false);
  xr.caption.aspect = c.width / c.height;
}

function captionFor(i, extra) {
  const p = photos[i];
  if (!p) return [extra || 'No photos'];
  const lines = [p.name, `${i + 1} / ${photos.length} · ${ago(p.mtime)}`];
  if (extra) lines.unshift(extra);
  return lines;
}

async function show(i, extra) {
  if (!photos.length) { setCaption(['No 3D photos yet', 'Export some in Stereowright']); return; }
  index = (i + photos.length) % photos.length;
  const p = photos[index];
  const ticket = ++xr.loading;
  setCaption(captionFor(index, extra || (xr.cache.has(p.id) ? '' : 'Loading…')));
  try {
    const entry = await loadPhoto(p);
    if (ticket !== xr.loading || !xr.session) return;
    xr.photo = entry;
    setCaption(captionFor(index, extra));
    // Warm up the neighbours so next/previous is instant.
    for (const d of [1, -1]) {
      const q = photos[(index + d + photos.length) % photos.length];
      if (q && !xr.cache.has(q.id)) loadPhoto(q).catch(() => {});
    }
  } catch (e) {
    if (ticket === xr.loading) setCaption([`Couldn't load ${p.name}`, String(e.message || e)]);
  }
}

// ------------------------------------------------------------------ XR session

async function detectXR() {
  if (!navigator.xr) {
    const secure = window.isSecureContext;
    setNotice(secure
      ? 'This browser has no VR support. Open this page in the Meta Quest Browser.'
      : '3D needs a secure link: use the USB link (http://localhost:…) or the Wi-Fi https:// link.');
    return;
  }
  try { xrModes.vr = await navigator.xr.isSessionSupported('immersive-vr'); } catch { /* no */ }
  try { xrModes.ar = await navigator.xr.isSessionSupported('immersive-ar'); } catch { /* no */ }
  ui.ptLabel.hidden = !xrModes.ar;
  if (!xrModes.vr && !xrModes.ar) setNotice('This browser has no VR support. Open this page in the Meta Quest Browser.');
  renderGrid();
}

async function startXR(i) {
  if (xr.session || !photos.length) return;
  if (!xrModes.vr && !xrModes.ar) { window.open(`photo/${photos[i].id}`, '_blank', 'noopener'); return; }
  const mode = ui.pt.checked && xrModes.ar ? 'immersive-ar' : 'immersive-vr';
  let session;
  try {
    const gl = initGL();
    session = await navigator.xr.requestSession(mode, { optionalFeatures: ['local'] });
    await gl.makeXRCompatible();
    const scale = XRWebGLLayer.getNativeFramebufferScaleFactor
      ? XRWebGLLayer.getNativeFramebufferScaleFactor(session) : 1;
    const layer = new XRWebGLLayer(session, gl, { antialias: true, alpha: true, framebufferScaleFactor: scale });
    if ('fixedFoveation' in layer) layer.fixedFoveation = 0; // keep the edges of the photo sharp
    session.updateRenderState({ baseLayer: layer, depthNear: 0.05, depthFar: 100 });
    xr.ref = await session.requestReferenceSpace('local');
    xr.ref.addEventListener?.('reset', () => { xr.needRecenter = true; });
  } catch (e) {
    session?.end().catch(() => {});
    setNotice(`Could not start VR: ${e.message || e}`);
    return;
  }
  xr.session = session;
  xr.passthrough = mode === 'immersive-ar';
  xr.needRecenter = true;
  xr.size = 1;
  xr.photo = null;
  xr.sticks = {};
  session.addEventListener('end', onEnd);
  session.addEventListener('select', (e) => step(e.inputSource.handedness === 'left' ? -1 : 1));
  session.addEventListener('squeeze', () => { xr.needRecenter = true; });
  session.requestAnimationFrame(onFrame);
  show(i);
}

function onEnd() {
  const gl = xr.gl;
  for (const e of xr.cache.values()) e.tex.forEach((t) => gl.deleteTexture(t));
  xr.cache.clear();
  if (xr.caption.tex) { gl.deleteTexture(xr.caption.tex); xr.caption.tex = null; xr.caption.key = ''; }
  xr.session = null;
  xr.photo = null;
  const card = ui.grid.querySelector(`[data-id="${photos[index]?.id}"]`);
  card?.scrollIntoView({ block: 'center' });
  card?.focus({ preventScroll: true });
}

function step(d) {
  if (photos.length) show(index + d);
}

// Put the photo straight ahead of where you are looking (level, at eye height).
function recenter(pose) {
  const m = pose.transform.matrix;
  let fx = -m[8], fz = -m[10];
  const len = Math.hypot(fx, fz) || 1;
  fx /= len; fz /= len;
  const p = pose.transform.position;
  xr.anchor = { x: p.x + fx * DISTANCE, y: p.y, z: p.z + fz * DISTANCE, yaw: Math.atan2(-fx, -fz) };
  xr.needRecenter = false;
}

function handleInput(dt) {
  for (const src of xr.session.inputSources) {
    const gp = src.gamepad;
    if (!gp) continue;
    const st = (xr.sticks[src.handedness] ||= { armed: true, pressed: [] });
    const x = gp.axes[2] ?? 0, y = gp.axes[3] ?? 0;
    if (st.armed && Math.abs(x) > 0.7) { step(x > 0 ? 1 : -1); st.armed = false; }
    else if (Math.abs(x) < 0.3) st.armed = true;
    if (Math.abs(y) > 0.25 && Math.abs(y) > Math.abs(x)) {
      xr.size = Math.min(3, Math.max(0.3, xr.size * Math.exp(-y * dt * 0.9)));
    }
    const edge = (i) => {
      const now = !!gp.buttons[i]?.pressed;
      const was = st.pressed[i];
      st.pressed[i] = now;
      return now && !was;
    };
    if (edge(3)) xr.size = 1;                                                    // thumbstick press
    if (edge(4)) setCaption(captionFor(index), !xr.caption.sticky);              // A / X
    if (edge(5)) { xr.session.end(); return; }                                   // B / Y
  }
}

function onFrame(t, frame) {
  const session = frame.session;
  session.requestAnimationFrame(onFrame);
  const dt = Math.min(0.1, (t - (xr.lastT || t)) / 1000);
  xr.lastT = t;
  if (t - xr.lastPoll > POLL_MS) { xr.lastPoll = t; poll(); }

  const pose = frame.getViewerPose(xr.ref);
  if (!pose) return;
  if (xr.needRecenter || !xr.anchor) recenter(pose);
  handleInput(dt);
  if (!xr.session) return;

  const gl = xr.gl;
  const layer = session.renderState.baseLayer;
  gl.bindFramebuffer(gl.FRAMEBUFFER, layer.framebuffer);
  if (xr.passthrough) gl.clearColor(0, 0, 0, 0); else gl.clearColor(0.045, 0.05, 0.06, 1);
  gl.clear(gl.COLOR_BUFFER_BIT | gl.DEPTH_BUFFER_BIT);
  gl.useProgram(xr.prog.p);
  gl.bindVertexArray(xr.vao);
  gl.activeTexture(gl.TEXTURE0);
  gl.uniform1i(xr.prog.tex, 0);
  gl.disable(gl.DEPTH_TEST);
  gl.disable(gl.CULL_FACE);
  gl.enable(gl.BLEND);
  gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);

  const a = xr.anchor;
  const base = M.place(a.x, a.y, a.z, a.yaw);
  let photoModel = null, photoH = BOX_H * xr.size;
  if (xr.photo) {
    const asp = xr.photo.aspect;
    const w = Math.min(BOX_W, BOX_H * asp) * xr.size;
    photoH = w / asp;
    photoModel = M.mul(base, M.scaleXY(w, photoH));
  }
  const age = performance.now() - xr.caption.shownAt;
  const capAlpha = xr.caption.sticky ? 1 : Math.max(0, Math.min(1, (CAPTION_MS - age) / 500));
  let capModel = null;
  if (xr.caption.tex && capAlpha > 0) {
    const cw = 0.9;
    capModel = M.mul(base, M.scaleXY(cw, cw / xr.caption.aspect, -(photoH / 2 + 0.06 + cw / xr.caption.aspect / 2), 0.002));
  }

  for (const view of pose.views) {
    const vp = layer.getViewport(view);
    gl.viewport(vp.x, vp.y, vp.width, vp.height);
    const vpm = M.mul(view.projectionMatrix, view.transform.inverse.matrix);
    if (photoModel) {
      gl.bindTexture(gl.TEXTURE_2D, xr.photo.tex[view.eye === 'right' ? 1 : 0]);
      gl.uniform1f(xr.prog.alpha, 1);
      gl.uniformMatrix4fv(xr.prog.mvp, false, M.mul(vpm, photoModel));
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    }
    if (capModel) {
      gl.bindTexture(gl.TEXTURE_2D, xr.caption.tex);
      gl.uniform1f(xr.prog.alpha, capAlpha);
      gl.uniformMatrix4fv(xr.prog.mvp, false, M.mul(vpm, capModel));
      gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
    }
  }
}

// ------------------------------------------------------------------ start

ui.enter.addEventListener('click', () => startXR(0));
detectXR();
poll();
setInterval(() => { if (!xr.session && document.visibilityState === 'visible') poll(); }, POLL_MS);
