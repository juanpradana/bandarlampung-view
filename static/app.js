/* =========================================================================
   CCTV Bandar Lampung — client application
   Vanilla JS (no build step). Modules:
     state      — dataset + filters
     map        — MapLibre markers
     cards      — virtualised-ish card list
     drawer     — camera detail + player
     multistream— preset grid + HLS players
   ========================================================================= */

const API = {
  async cameras() {
    const r = await fetch('/api/cameras?live=false');
    if (!r.ok) throw new Error('Gagal memuat data kamera');
    return r.json();
  },
  async presets() {
    const r = await fetch('/api/presets');
    if (!r.ok) throw new Error('Gagal memuat preset');
    return r.json();
  },
};

/* ------------------------------ state ------------------------------ */
const state = {
  all: [],
  filtered: [],
  presets: [],
  activeTag: '',
  activeSource: '',
  query: '',
  sort: 'name',
  selectedId: null,
  msIds: [], // ids in current multi-stream
  msPreset: 'traffic-corridor',
  msRotate: 0,
  msRotateTimer: null,
  players: new Map(), // id -> Hls instance (for cleanup)
};

/* --------------------------- DOM helpers --------------------------- */
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => [...root.querySelectorAll(sel)];

function el(tag, props = {}, children = []) {
  const e = document.createElement(tag);
  Object.assign(e, props);
  for (const c of children) {
    if (typeof c === 'string') e.append(document.createTextNode(c));
    else if (c) e.append(c);
  }
  return e;
}

const fmtTag = { macet: 'Macet', strategis: 'Strategis', ptz: 'PTZ' };

/* ------------------------------ map ------------------------------- */
let map = null;
let markerLayer = null;
const markersById = new Map();

function initMap() {
  map = new maplibregl.Map({
    container: 'map',
    style: {
      version: 8,
      sources: {
        osm: {
          type: 'raster',
          tiles: [
            'https://a.tile.openstreetmap.org/{z}/{x}/{y}.png',
            'https://b.tile.openstreetmap.org/{z}/{x}/{y}.png',
            'https://c.tile.openstreetmap.org/{z}/{x}/{y}.png',
          ],
          tileSize: 256,
          attribution: '© OpenStreetMap contributors',
        },
      },
      layers: [
        {
          id: 'osm',
          type: 'raster',
          source: 'osm',
          paint: { 'raster-saturation': -0.35, 'raster-brightness-max': 0.85 },
        },
      ],
    },
    center: [105.266, -5.425],
    zoom: 11.6,
    attributionControl: true,
  });

  map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'bottom-right');

  map.on('load', () => {
    // Darken tiles in dark mode
    if (document.documentElement.dataset.theme === 'dark') {
      map.setPaintProperty('osm', 'raster-brightness-max', 0.55);
      map.setPaintProperty('osm', 'raster-saturation', -0.6);
    }
    renderMarkers();
  });
}

function markerClass(cam) {
  if (cam.tags.includes('ptz')) return 'ptz';
  if (cam.tags.includes('macet')) return 'macet';
  return 'strategis';
}

function renderMarkers() {
  if (!map) return;

  // Remove previous
  for (const m of markersById.values()) m.remove();
  markersById.clear();

  for (const cam of state.filtered) {
    const elDiv = document.createElement('button');
    elDiv.className = `cctv-marker ${markerClass(cam)}`;
    elDiv.title = cam.name;
    elDiv.setAttribute('aria-label', cam.name);
    elDiv.addEventListener('click', (ev) => {
      ev.stopPropagation();
      openDrawer(cam);
    });

    // Build popup DOM directly — never interpolate upstream names into HTML.
    const popupBody = el('div');
    popupBody.append(el('strong', { textContent: cam.name }));
    popupBody.append(el('br'));
    popupBody.append(el('span', {
      textContent: cam.district || '',
      style: 'font-size:11px;opacity:.7',
    }));
    const popup = new maplibregl.Popup({ offset: 14, closeButton: false })
      .setDOMContent(popupBody);

    const marker = new maplibregl.Marker({ element: elDiv })
      .setLngLat([cam.longitude, cam.latitude])
      .setPopup(popup)
      .addTo(map);

    markersById.set(cam.id, marker);
  }
}

function escapeHtml(s) {
  return String(s ?? '').replace(/[&<>"']/g, (c) => ({
    '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;',
  }[c]));
}

/* ------------------------------ cards ------------------------------ */
const cardTpl = document.getElementById('tpl-card');
let cardObserver = null;

function thumbUrl(cam) {
  // Seribu Wajah has no thumbnail field but does have a snapshot endpoint.
  if (cam.source === 'seribuwajah') {
    return `/api/cameras/${cam.id}/snapshot`;
  }
  return cam.thumbnail ? `/api/cameras/${cam.id}/thumb` : '';
}

function renderCards() {
  const container = $('#cards');
  container.innerHTML = '';
  if (cardObserver) cardObserver.disconnect();

  const frag = document.createDocumentFragment();

  for (const cam of state.filtered) {
    const node = cardTpl.content.firstElementChild.cloneNode(true);
    node.dataset.id = cam.id;
    const img = $('img', node);
    img.alt = cam.name;

    // Lazy-load via IntersectionObserver so 380 cards don't fetch at once.
    const src = thumbUrl(cam);
    img.dataset.src = src;
    img.addEventListener('error', () => {
      img.removeAttribute('src');
      img.parentElement.classList.add('no-img');
    });

    $('.card-name', node).textContent = cam.name;
    $('.card-addr', node).textContent =
      [cam.district, cam.subdistrict, cam.cityAddress].filter(Boolean).join(' · ') || '—';

    const statusEl = $('.card-status', node);
    statusEl.dataset.status = cam.status === 'online' ? 'online' : 'offline';

    $('.card-source', node).textContent =
      cam.source === 'seribuwajah' ? 'SW' : 'RESTA';

    const tagsEl = $('.card-tags', node);
    for (const t of cam.tags) {
      const b = document.createElement('span');
      b.className = `tag-badge tag-${t}`;
      b.textContent = fmtTag[t] || t;
      tagsEl.append(b);
    }

    $('[data-play]', node).addEventListener('click', (e) => {
      e.stopPropagation();
      openDrawer(cam);
    });
    $('[data-multi]', node).addEventListener('click', (e) => {
      e.stopPropagation();
      addToMulti(cam.id);
    });

    node.addEventListener('click', () => openDrawer(cam));

    frag.append(node);
  }

  container.append(frag);

  // Set up lazy loading
  cardObserver = new IntersectionObserver(
    (entries) => {
      for (const ent of entries) {
        if (!ent.isIntersecting) continue;
        const img = ent.target;
        if (img.dataset.src && !img.src) {
          img.src = img.dataset.src;
        }
        cardObserver.unobserve(img);
      }
    },
    { root: container, rootMargin: '200px' },
  );

  for (const img of $$('.card-thumb img', container)) {
    cardObserver.observe(img);
  }

  $('#list-count').textContent = `${state.filtered.length} kamera`;
}

/* ------------------------------ drawer ----------------------------- */
let drawerHls = null;

function openDrawer(cam) {
  // Tear down the previous drawer player before attaching a new one, otherwise
  // its hls.js instance keeps fetching segments in the background.
  destroyHls(drawerHls);
  drawerHls = null;
  if (state.selectedId) state.players.delete(state.selectedId);

  state.selectedId = cam.id;
  $('#drawer-name').textContent = cam.name;
  $('#drawer').hidden = false;

  $$('.card').forEach((c) => c.classList.toggle('is-selected', c.dataset.id === cam.id));

  renderPlayer($('#drawer-player'), cam);

  const meta = $('#drawer-meta');
  meta.innerHTML = '';
  const rows = [
    ['Kecamatan', cam.district || '—'],
    ['Kelurahan', cam.subdistrict || '—'],
    ['Alamat', cam.cityAddress || '—'],
    ['Kategori', cam.category || '—'],
    ['Sumber', cam.source === 'seribuwajah' ? 'Seribu Wajah' : 'Polresta'],
    ['Status', cam.status === 'online' ? 'Online' : 'Offline'],
    ['Tipe', hlsSrc(cam) ? 'HLS (proxied)' : cam.embedUrl ? 'Embed' : 'Snapshot'],
    ['Koordinat', `${cam.latitude.toFixed(5)}, ${cam.longitude.toFixed(5)}`],
    ['Tag', cam.tags.join(', ') || '—'],
  ];
  for (const [k, v] of rows) {
    meta.append(el('dt', { textContent: k }));
    meta.append(el('dd', { textContent: v }));
  }

  $('#drawer-big').onclick = () => openBig(cam);
  $('#drawer-add').onclick = () => addToMulti(cam.id);
}

function closeDrawer() {
  $('#drawer').hidden = true;
  destroyHls(drawerHls);
  drawerHls = null;
  $('#drawer-player').innerHTML = '';
  state.selectedId = null;
  $$('.card').forEach((c) => c.classList.remove('is-selected'));
}

function destroyHls(inst) {
  try { inst?.destroy(); } catch { /* ignore */ }
}

/**
 * Our own HLS endpoint for a camera, or `null` when it has none.
 *
 * Both upstreams are unified behind `/api/cameras/:id/stream`: Seribu Wajah
 * serves a static HLS tree, and Polresta's IP-bound ITN gateway session is
 * minted and rewritten server-side. Playing both through one <video> keeps the
 * player chrome, latency behaviour and multi-stream tiling identical, instead
 * of dropping third-party iframes into tiles for half the cameras.
 */
function hlsSrc(cam) {
  if (cam.status !== 'online') return null;
  if (cam.source === 'seribuwajah') return cam.streamUrl ? `/api/cameras/${cam.id}/stream` : null;
  if (cam.source === 'polresta') {
    return cam.token || cam.embedUrl ? `/api/cameras/${cam.id}/stream` : null;
  }
  return cam.playback === 'hls' ? `/api/cameras/${cam.id}/stream` : null;
}

/**
 * Render a camera into `host`.
 *  - live  → <video> + hls.js, proxied through /api/cameras/:id/stream
 *  - embed → <iframe> fallback (Polresta only, if HLS fails)
 *  - still → snapshot
 */
function renderPlayer(host, cam) {
  host.innerHTML = '';

  if (cam.status !== 'online') {
    host.append(el('div', {
      className: 'player-offline',
      textContent: 'Kamera offline',
      style: 'display:flex;align-items:center;justify-content:center;height:100%;color:#94a3b8;font-size:12px',
    }));
    return;
  }

  const src = hlsSrc(cam);
  if (src) {
    const wrapper = el('div', {
      style: 'position:relative;width:100%;height:100%;background:#000;display:flex;align-items:center;justify-content:center',
    });
    const video = el('video', {
      controls: true,
      autoplay: true,
      muted: true,
      playsinline: true,
      style: 'width:100%;height:100%;object-fit:contain',
    });
    const loader = el('div', {
      className: 'player-loader',
      textContent: 'Menghubungkan stream…',
      style: 'position:absolute;color:#94a3b8;font-size:12px;pointer-events:none',
    });
    video.addEventListener('playing', () => { loader.style.display = 'none'; }, { once: true });
    video.addEventListener('loadeddata', () => { loader.style.display = 'none'; }, { once: true });

    wrapper.append(video);
    wrapper.append(loader);
    host.append(wrapper);

    attachHls(video, src, cam.id, cam);
    return;
  }

  if (cam.embedUrl) {
    host.append(el('iframe', {
      src: cam.embedUrl,
      allow: 'autoplay; fullscreen; picture-in-picture',
      allowfullscreen: true,
      loading: 'lazy',
      title: cam.name,
      style: 'width:100%;height:100%;border:0',
    }));
    return;
  }

  // Snapshot-only fallback
  host.append(el('img', {
    src: `/api/cameras/${cam.id}/snapshot`,
    alt: cam.name,
    style: 'width:100%;height:100%;object-fit:contain;background:#000',
  }));
}

function attachHls(video, src, key, cam) {
  // Prefer MSE-based hls.js whenever it is available.
  //
  // Do NOT gate this on `video.canPlayType('application/vnd.apple.mpegurl')`:
  // Chrome reports "maybe" for that MIME type — a truthy string — yet has no
  // native HLS decoder, so handing it a bare .m3u8 fails with
  // MEDIA_ERR_SRC_NOT_SUPPORTED. Only Safari/iOS (where MSE is absent or
  // limited) should get the native path, and only after hls.js is ruled out.
  if (window.Hls && Hls.isSupported()) {
    return attachHlsJs(video, src, key, cam);
  }

  // No hls.js: let the platform try natively, and surface a fallback if it fails.
  video.src = src;
  video.addEventListener('error', () => fallbackFor(video, cam, key), { once: true });
  return null;
}

function attachHlsJs(video, src, key, cam) {
  const hls = new Hls({
    enableWorker: true,
    lowLatencyMode: true,
    backBufferLength: 30,
    maxBufferLength: 15,
    // Fail fast on dead upstream streams so the user gets a fallback snapshot
    // in ~3 seconds rather than stalling on a black video box.
    manifestLoadingMaxRetry: 3,
    manifestLoadingRetryDelay: 800,
    levelLoadingMaxRetry: 3,
    fragLoadingMaxRetry: 3,
    fragLoadingRetryDelay: 500,
  });

  let recoveries = 0;

  hls.loadSource(src);
  hls.attachMedia(video);

  hls.on(Hls.Events.ERROR, (_evt, data) => {
    if (!data.fatal) return;
    switch (data.type) {
      case Hls.ErrorTypes.NETWORK_ERROR:
        hls.startLoad();
        break;
      case Hls.ErrorTypes.MEDIA_ERROR:
        hls.recoverMediaError();
        break;
      default:
        destroyHls(hls);
        fallbackFor(video, cam, key);
        return;
    }
    if (++recoveries > 4) {
      destroyHls(hls);
      fallbackFor(video, cam, key);
    }
  });

  state.players.set(key, hls);
  if (key === state.selectedId) drawerHls = hls;
  return hls;
}

/// Replace a failed <video> with this camera's last-resort renderer.
function fallbackFor(video, cam, key) {
  state.players.delete(key);
  if (key === state.selectedId) drawerHls = null;

  const host = video.parentElement;
  if (!host) return;
  host.innerHTML = '';

  if (cam && cam.embedUrl) {
    host.append(el('iframe', {
      src: cam.embedUrl,
      allow: 'autoplay; fullscreen',
      allowfullscreen: true,
      title: cam.name,
    }));
    return;
  }
  if (cam) {
    const wrap = el('div', {
      style: 'position:relative;width:100%;height:100%;display:flex;align-items:center;justify-content:center;background:#000',
    });
    wrap.append(el('img', {
      src: `/api/cameras/${cam.id}/snapshot`,
      alt: cam.name,
      style: 'width:100%;height:100%;object-fit:contain',
    }));
    wrap.append(el('span', {
      textContent: 'Snapshot (Stream Offline)',
      style: 'position:absolute;bottom:8px;right:8px;background:rgba(15,23,42,0.85);color:#cbd5e1;font-size:11px;padding:3px 8px;border-radius:4px;border:1px solid rgba(255,255,255,0.1)',
    }));
    host.append(wrap);
  }
}

/* --------------------------- big view ---------------------------- */
function openBig(cam) {
  // Reuse multi-stream overlay with a single tile.
  state.msIds = [cam.id];
  state.msPreset = 'custom';
  showMulti();
}

/* --------------------------- multistream -------------------------- */
const MS_GRIDS = {
  'traffic-corridor': {
    cols: 'repeat(2, minmax(0, 1fr))',
    rows: 'repeat(2, minmax(0, 1fr))',
    areas: null,
  },
  'intersection': {
    cols: 'repeat(4, minmax(0, 1fr))',
    rows: 'repeat(3, minmax(0, 1fr))',
    areas: '"m m m p1" "m m m p2" "m m m p3"',
    map: { m: 0, p1: 1, p2: 2, p3: 3 },
  },
  'strategic-cluster': {
    cols: 'repeat(2, minmax(0, 1fr))',
    rows: 'repeat(3, minmax(0, 1fr))',
    areas: null,
  },
  'custom': {
    cols: 'repeat(auto-fit, minmax(280px, 1fr))',
    rows: 'auto',
    areas: null,
  },
};

function addToMulti(id) {
  if (!state.msIds.includes(id)) state.msIds.push(id);
  if (state.msIds.length > 6) state.msIds = state.msIds.slice(-6);
  // The user is curating now, so show their picks rather than a server preset.
  state.msPreset = 'custom';
  showMulti();
}

function showMulti() {
  $('#multistream').hidden = false;
  renderMulti();

  $$('.ms-preset').forEach((b) => {
    b.classList.toggle('is-active', b.dataset.preset === state.msPreset);
  });
}

function closeMulti() {
  $('#multistream').hidden = true;
  stopRotate();
  // destroy all players created for tiles
  for (const [key, inst] of state.players) {
    if (key.startsWith('ms:')) {
      destroyHls(inst);
      state.players.delete(key);
    }
  }
  $('#ms-grid').innerHTML = '';
}

function currentPresetCameras() {
  if (state.msPreset === 'custom') {
    // Custom always reflects the user's explicit picks — never a server preset.
    return state.msIds
      .map((id) => state.all.find((c) => c.id === id))
      .filter(Boolean);
  }
  // A named preset always seeds from the server, so switching chips is honest:
  // the tiles change when the chip changes.
  const p = state.presets.find((x) => x.id === state.msPreset);
  const ids = p?.cameraIds || p?.camera_ids || [];
  return ids.map((id) => state.all.find((c) => c.id === id)).filter(Boolean);
}

function renderMulti() {
  const grid = $('#ms-grid');
  grid.innerHTML = '';

  // destroy previous MS players
  for (const [key, inst] of state.players) {
    if (key.startsWith('ms:')) {
      destroyHls(inst);
      state.players.delete(key);
    }
  }

  const cams = currentPresetCameras();
  const cfg = MS_GRIDS[state.msPreset] || MS_GRIDS.custom;

  grid.dataset.preset = state.msPreset;
  grid.style.gridTemplateColumns = cfg.cols;
  grid.style.gridTemplateRows = cfg.rows;
  grid.style.gridTemplateAreas = cfg.areas || 'none';

  if (!cams.length) {
    grid.append(el('div', {
      style: 'grid-column:1/-1;display:flex;align-items:center;justify-content:center;color:#94a3b8;font-size:13px',
      textContent: 'Belum ada kamera. Pilih dari daftar dengan tombol "Multi".',
    }));
    return;
  }

  cams.forEach((cam, i) => {
    const tile = el('div', { className: 'ms-tile' });
    if (cfg.areas) {
      // area name from map or fallback
      const key = Object.keys(cfg.map || {}).find((k) => cfg.map[k] === i);
      tile.style.gridArea = key || 'auto';
    }

    const head = el('div', { className: 'ms-tile-header' });
    head.append(el('span', { textContent: cam.name }));
    head.append(el('span', {
      textContent: cam.district || '',
      style: 'opacity:.7;font-weight:400',
    }));
    tile.append(head);

    const content = el('div', { className: 'ms-tile-content' });
    tile.append(content);

    tile.addEventListener('dblclick', () => openDrawer(cam));

    grid.append(tile);

    // Render player into content.
    const src = hlsSrc(cam);
    if (src) {
      const video = el('video', {
        muted: true, autoplay: true, playsinline: true, controls: false,
      });
      content.append(video);
      attachHls(video, src, `ms:${cam.id}`, cam);
    } else if (cam.embedUrl) {
      content.append(el('iframe', {
        src: cam.embedUrl,
        allow: 'autoplay; fullscreen',
        allowfullscreen: true,
        title: cam.name,
      }));
    } else {
      content.append(el('img', {
        src: `/api/cameras/${cam.id}/snapshot`,
        alt: cam.name,
        style: 'width:100%;height:100%;object-fit:cover',
      }));
    }
  });
}

/* rotation */
function toggleRotate() {
  state.msRotate = state.msRotate ? 0 : 15; // seconds
  const btn = $('#ms-rotate');
  btn.setAttribute('aria-pressed', String(!!state.msRotate));

  if (state.msRotate) {
    startRotate();
  } else {
    stopRotate();
  }
}

function startRotate() {
  stopRotate();
  state.msRotateTimer = setInterval(() => {
    const cams = currentPresetCameras();
    if (cams.length < 2) return;
    // rotate the list: move last to front
    state.msIds = cams.map((c) => c.id);
    state.msIds.unshift(state.msIds.pop());
    renderMulti();
  }, state.msRotate * 1000);
}

function stopRotate() {
  if (state.msRotateTimer) {
    clearInterval(state.msRotateTimer);
    state.msRotateTimer = null;
  }
}

/* --------------------------- filtering ---------------------------- */
function applyFilters() {
  const q = state.query.trim().toLowerCase();

  state.filtered = state.all.filter((c) => {
    if (state.activeTag && !c.tags.includes(state.activeTag)) return false;
    if (state.activeSource && c.source !== state.activeSource) return false;
    if (q) {
      const hay = `${c.name} ${c.cityAddress || ''} ${c.district || ''} ${c.subdistrict || ''}`.toLowerCase();
      if (!hay.includes(q)) return false;
    }
    return true;
  });

  sortList();
  renderCards();
  renderMarkers();
}

function sortList() {
  const s = state.sort;
  state.filtered.sort((a, b) => {
    if (s === 'district') return (a.district || '').localeCompare(b.district || '');
    if (s === 'status') return (a.status === 'online' ? -1 : 1) - (b.status === 'online' ? -1 : 1);
    return a.name.localeCompare(b.name);
  });
}

/* ------------------------------ init ------------------------------ */
async function boot() {
  initMap();

  // Wire UI
  $('#search').addEventListener('input', (e) => {
    state.query = e.target.value;
    applyFilters();
  });

  $$('.tab').forEach((btn) => {
    btn.addEventListener('click', () => {
      $$('.tab').forEach((b) => {
        b.classList.toggle('is-active', b === btn);
        b.setAttribute('aria-selected', String(b === btn));
      });
      state.activeTag = btn.dataset.tag;
      applyFilters();
    });
  });

  $('#source-filter').addEventListener('change', (e) => {
    state.activeSource = e.target.value;
    applyFilters();
  });

  $('#sort-select').addEventListener('change', (e) => {
    state.sort = e.target.value;
    sortList();
    renderCards();
  });

  $('#drawer-close').addEventListener('click', closeDrawer);
  $('#ms-close').addEventListener('click', closeMulti);
  $('#ms-rotate').addEventListener('click', toggleRotate);
  $('#ms-open-btn').addEventListener('click', () => {
    state.msPreset = 'traffic-corridor';
    state.msIds = [];
    showMulti();
  });

  $$('.ms-preset').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.msPreset = btn.dataset.preset;
      state.msIds = []; // clear explicit selection when switching preset
      showMulti();
    });
  });

  $('#theme-toggle').addEventListener('click', () => {
    const html = document.documentElement;
    const next = html.dataset.theme === 'dark' ? 'light' : 'dark';
    html.dataset.theme = next;
    try { localStorage.setItem('cctv-theme', next); } catch { /* ignore */ }
    if (map && map.getLayer('osm')) {
      map.setPaintProperty('osm', 'raster-brightness-max', next === 'dark' ? 0.55 : 0.85);
      map.setPaintProperty('osm', 'raster-saturation', next === 'dark' ? -0.6 : -0.35);
    }
  });

  // Restore theme
  try {
    const saved = localStorage.getItem('cctv-theme');
    if (saved) document.documentElement.dataset.theme = saved;
  } catch { /* ignore */ }

  // Escape closes overlays
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (!$('#multistream').hidden) closeMulti();
    else if (!$('#drawer').hidden) closeDrawer();
  });

  // Load data
  try {
    const [data, presets] = await Promise.all([API.cameras(), API.presets()]);
    state.all = data.cameras;
    state.presets = presets.presets || [];
    applyFilters();
  } catch (err) {
    console.error(err);
    $('#list-count').textContent = 'Gagal memuat data';
    $('#cards').append(el('p', {
      textContent: 'Tidak dapat memuat data kamera. Pastikan server berjalan.',
      style: 'padding:16px;color:#ef4444;font-size:12px',
    }));
  }
}

boot();