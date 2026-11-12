/* =========================================================================
   CCTV Bandar Lampung — Client application (Ruang Kendali)
   Pure vanilla ES module. Zero build step.
   Modules:
     state       — dataset, active filters, geolocation, user multi-stream picks
     map         — MapLibre instance with OpenFreeMap/Esri basemaps & custom markers
     legend      — operable category switchboard with live counts & swatches
     gps         — Geolocation API, distance calculus, user position marker
     cards       — virtualised-ish camera cards with distance tags
     drawer      — detail drawer + HLS / embed / snapshot player
     multistream — preset grid + custom layout with search-to-add, delete, & localStorage
   ========================================================================= */

const STORAGE_KEYS = {
  theme: 'cctv-theme',
  customMulti: 'cctv-custom-multistream-ids-v1',
  basemap: 'cctv-basemap',
  railCollapsed: 'cctv-rail-collapsed',
};

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
  activeTag: '',          // '' = all, 'macet', 'strategis', 'ptz'
  activeSource: '',
  query: '',
  sort: 'name',           // 'name' | 'district' | 'status' | 'distance'
  selectedId: null,

  // Multi-stream
  msPreset: 'traffic-corridor',
  msIds: [],              // active list rendered in tiles
  customIds: [],          // user's saved picks (persisted to localStorage)
  msRotate: 0,
  msRotateTimer: null,
  players: new Map(),     // key -> Hls instance

  // Geolocation
  userCoords: null,       // [lng, lat]
  userAccuracy: null,
  watchId: null,

  // Basemap
  basemap: 'auto',        // 'auto' (tracks theme: OpenFreeMap dark / positron) | 'satellite'
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

/* ---------------------- distance / geodesy ------------------------- */
/**
 * Haversine distance in metres between two [lng, lat] coordinates.
 */
function distanceMeters([lng1, lat1], [lng2, lat2]) {
  const R = 6371e3;
  const toRad = (d) => (d * Math.PI) / 180;
  const dLat = toRad(lat2 - lat1);
  const dLon = toRad(lng2 - lng1);
  const a =
    Math.sin(dLat / 2) ** 2 +
    Math.cos(toRad(lat1)) * Math.cos(toRad(lat2)) * Math.sin(dLon / 2) ** 2;
  const c = 2 * Math.atan2(Math.sqrt(a), Math.sqrt(1 - a));
  return R * c;
}

function formatDistance(m) {
  if (m == null || Number.isNaN(m)) return '';
  if (m < 1000) return `${Math.round(m)} m`;
  if (m < 10000) return `${(m / 1000).toFixed(1)} km`;
  return `${Math.round(m / 1000)} km`;
}

function computeDistances() {
  if (!state.userCoords) return;
  for (const cam of state.all) {
    cam.distanceM = distanceMeters(state.userCoords, [cam.longitude, cam.latitude]);
  }
}

/* ------------------------------ map ------------------------------- */
let map = null;
const markersById = new Map();
let userMarker = null;

// Basemaps.
// CARTO's dark_all/voyager raster endpoints started requiring an API key and
// answer unauthenticated requests with a 2.5KB watermarked placeholder, so the
// entire map rendered as tiled "API KEY REQUIRED" text. OpenFreeMap serves real
// key-free OpenMapTiles vector data (crisp at any DPI, no watermark, no key).
// Esri World Imagery stays for the satellite view.
const BASEMAP_STYLES = {
  dark: 'https://tiles.openfreemap.org/styles/dark',
  light: 'https://tiles.openfreemap.org/styles/positron',
  // Esri World Imagery (high-res satellite) + Esri boundaries/labels overlay
  satellite: {
    version: 8,
    sources: {
      esri: {
        type: 'raster',
        tiles: [
          'https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}',
        ],
        tileSize: 256,
        attribution: '© Esri, Maxar, Earthstar Geographics',
        maxzoom: 19,
      },
      labels: {
        type: 'raster',
        tiles: [
          'https://server.arcgisonline.com/ArcGIS/rest/services/Reference/World_Boundaries_and_Places/MapServer/tile/{z}/{y}/{x}',
        ],
        tileSize: 256,
        maxzoom: 19,
      },
    },
    layers: [
      { id: 'esri', type: 'raster', source: 'esri' },
      { id: 'labels', type: 'raster', source: 'labels', paint: { 'raster-opacity': 0.85 } },
    ],
  },
};

function resolveCurrentStyle() {
  if (state.basemap === 'satellite') return BASEMAP_STYLES.satellite;
  const theme = document.documentElement.dataset.theme === 'light' ? 'light' : 'dark';
  return BASEMAP_STYLES[theme];
}

// OpenFreeMap ships a neutral grey. Repainting the structural layers in the
// app's own ink (--bg-page, --accent, --text-*) makes the basemap read as the
// dark colour field the glass chrome sits on, instead of a grey slab behind it.
// Only the dark variant is retuned — positron is already light and low-noise.
const DARK_MAP_RECOLOR = {
  background: '#05080f',
  water: '#0a1526',
  waterway: '#0a1526',
  landuse_residential: '#0c1424',
  landcover_wood: '#0e1a2a',
  landuse_park: '#0e1a2a',
  building: '#101a2c',
  highway_path: '#16223a',
  highway_minor: '#1a2740',
  highway_major_casing: '#22334f',
  highway_major_inner: '#2c4165',
  highway_motorway_casing: '#1d2c46',
  highway_motorway_inner: '#3a5a86',
  railway: '#22314c',
  boundary_state: '#2a3c5c',
  'boundary_country_z5-': '#2a3c5c',
};

function retuneDarkBasemap() {
  if (!map || state.basemap === 'satellite') return;
  if (document.documentElement.dataset.theme === 'light') return;

  const isFill = { water: 1, landuse_residential: 1, landcover_wood: 1, landuse_park: 1, building: 1 };

  for (const [id, color] of Object.entries(DARK_MAP_RECOLOR)) {
    if (!map.getLayer(id)) continue;
    try {
      if (isFill[id]) {
        map.setPaintProperty(id, 'fill-color', color);
      } else {
        map.setPaintProperty(id, 'line-color', color);
      }
    } catch { /* layer shape differs upstream — skip it */ }
  }

  // Label ink: lift place names, keep road names quiet.
  for (const [id, color] of [
    ['place_other', '#7f95b8'], ['place_suburb', '#7f95b8'],
    ['place_village', '#7f95b8'], ['place_town', '#93a9c9'], ['place_city', '#a9bcd8'],
    ['highway_name_other', '#6d84a8'], ['water_name', '#4d6b94'],
  ]) {
    if (map.getLayer(id)) {
      try { map.setPaintProperty(id, 'text-color', color); } catch { /* ignore */ }
    }
  }
}

function initMap() {
  map = new maplibregl.Map({
    container: 'map',
    style: resolveCurrentStyle(),
    center: [105.266, -5.425],
    zoom: 12.2,
    attributionControl: false,
  });

  // Minimal bottom-right attribution
  map.addControl(
    new maplibregl.AttributionControl({ compact: true }),
    'bottom-right',
  );

  map.on('load', () => {
    retuneDarkBasemap();
    renderMarkers();
    renderUserMarker();
  });

  // Re-render markers if the map style changes
  map.on('style.load', () => {
    retuneDarkBasemap();
    renderMarkers();
    renderUserMarker();
  });
}

function switchBasemap(mode) {
  state.basemap = mode;
  try { localStorage.setItem(STORAGE_KEYS.basemap, mode); } catch { /* ignore */ }
  $('#basemap-btn')?.classList.toggle('is-active', mode === 'satellite');
  $('#basemap-btn')?.setAttribute('aria-pressed', String(mode === 'satellite'));
  map?.setStyle(resolveCurrentStyle());
}

function markerClass(cam) {
  if (cam.tags.includes('ptz')) return 'ptz';
  if (cam.tags.includes('macet')) return 'macet';
  return 'strategis';
}

function renderMarkers() {
  if (!map) return;

  for (const m of markersById.values()) m.remove();
  markersById.clear();

  for (const cam of state.filtered) {
    const elBtn = document.createElement('button');
    const cat = markerClass(cam);
    elBtn.className = `cctv-marker ${cat}`;
    elBtn.title = `${cam.name} (${fmtTag[cat] || cat})`;
    elBtn.setAttribute('aria-label', cam.name);
    if (cam.id === state.selectedId) elBtn.classList.add('is-selected');

    elBtn.addEventListener('click', (ev) => {
      ev.stopPropagation();
      openDrawer(cam);
    });

    // Rich popup: name + district + distance (if available) + live button
    const popupBody = el('div');
    popupBody.append(el('strong', { className: 'popup-name', textContent: cam.name }));
    const addr = [cam.district, cam.subdistrict].filter(Boolean).join(' · ');
    if (addr) popupBody.append(el('span', { className: 'popup-sub', textContent: addr }));
    if (cam.distanceM != null) {
      popupBody.append(el('span', {
        className: 'popup-meta',
        textContent: `Jarak: ${formatDistance(cam.distanceM)}`,
      }));
    }

    const popup = new maplibregl.Popup({
      offset: 14,
      closeButton: false,
      maxWidth: '260px',
    }).setDOMContent(popupBody);

    const marker = new maplibregl.Marker({ element: elBtn })
      .setLngLat([cam.longitude, cam.latitude])
      .setPopup(popup)
      .addTo(map);

    markersById.set(cam.id, marker);
  }

  // Update counts on the legend
  renderLegend();
}

function renderUserMarker() {
  if (!map || !state.userCoords) {
    if (userMarker) { userMarker.remove(); userMarker = null; }
    return;
  }

  if (!userMarker) {
    const wrap = el('div', { className: 'gps-marker', title: 'Posisi Anda saat ini' });
    wrap.append(el('div', { className: 'gps-halo' }));
    wrap.append(el('div', { className: 'gps-dot' }));

    userMarker = new maplibregl.Marker({ element: wrap, anchor: 'center' })
      .setLngLat(state.userCoords)
      .addTo(map);
  } else {
    userMarker.setLngLat(state.userCoords);
  }
}

/* ──────────────────── Operable legend (switchboard) ─────────────── */
const LEGEND_ITEMS = [
  { tag: '', label: 'Semua', shape: 'ring', color: 'var(--text-secondary)' },
  { tag: 'macet', label: 'Pantau Macet', shape: 'square', color: 'var(--macet-color)' },
  { tag: 'strategis', label: 'Lokasi Strategis', shape: 'dot', color: 'var(--strategis-color)' },
  { tag: 'ptz', label: 'Kamera PTZ', shape: 'diamond', color: 'var(--ptz-color)' },
];

function renderLegend() {
  const host = $('#legend');
  if (!host) return;
  host.innerHTML = '';

  // Counts across the currently source/query-filtered set
  const base = state.all.filter((c) => {
    if (state.activeSource && c.source !== state.activeSource) return false;
    if (state.query) {
      const q = state.query.toLowerCase();
      const hay = `${c.name} ${c.cityAddress || ''} ${c.district || ''}`.toLowerCase();
      if (!hay.includes(q)) return false;
    }
    return true;
  });

  const counts = {
    '': base.length,
    macet: base.filter((c) => c.tags.includes('macet')).length,
    strategis: base.filter((c) => c.tags.includes('strategis')).length,
    ptz: base.filter((c) => c.tags.includes('ptz')).length,
  };

  LEGEND_ITEMS.forEach((it, i) => {
    if (i === 1) host.append(el('span', { className: 'legend-sep', 'aria-hidden': 'true' }));

    const btn = el('button', {
      className: `legend-item ${state.activeTag === it.tag ? 'is-active' : ''}`,
      'aria-pressed': String(state.activeTag === it.tag),
      title: `Filter: ${it.label}`,
    });
    btn.dataset.tag = it.tag;

    const swatch = el('span', {
      className: 'legend-swatch',
      'aria-hidden': 'true',
    });
    swatch.dataset.shape = it.shape;
    swatch.style.setProperty('--swatch-color', it.color);
    btn.append(swatch);

    btn.append(el('span', { textContent: it.label }));
    btn.append(el('span', { className: 'legend-count', textContent: String(counts[it.tag] ?? 0) }));

    btn.addEventListener('click', () => {
      // Toggle off if clicked twice, else activate
      state.activeTag = state.activeTag === it.tag ? '' : it.tag;
      applyFilters();
    });

    host.append(btn);
  });
}

/* ────────────────────────── GPS / Geolocation ───────────────────── */
function initGPS() {
  const btn = $('#gps-btn');
  if (!btn) return;

  if (!('geolocation' in navigator)) {
    btn.disabled = true;
    btn.title = 'Geolocation tidak didukung oleh browser ini';
    return;
  }

  btn.addEventListener('click', () => {
    if (state.userCoords) {
      // Already located: re-center and zoom to user
      map?.flyTo({ center: state.userCoords, zoom: 15, essential: true });
      return;
    }
    requestGPS();
  });
}

function requestGPS() {
  const btn = $('#gps-btn');
  const statusEl = $('#gps-status');

  btn?.classList.add('is-locating');
  if (statusEl) {
    statusEl.hidden = false;
    statusEl.textContent = 'Mencari lokasi GPS…';
  }

  navigator.geolocation.getCurrentPosition(
    (pos) => {
      btn?.classList.remove('is-locating');
      btn?.classList.add('is-active');
      btn?.setAttribute('aria-pressed', 'true');

      const coords = [pos.coords.longitude, pos.coords.latitude];
      state.userCoords = coords;
      state.userAccuracy = pos.coords.accuracy;

      computeDistances();

      // Show accuracy in the rail header
      if (statusEl) {
        statusEl.hidden = false;
        statusEl.textContent = `Akurasi ±${Math.round(pos.coords.accuracy)}m`;
      }

      // Fly to user
      renderUserMarker();
      map?.flyTo({ center: coords, zoom: 14.5, essential: true });

      // Automatically sort by distance once GPS lands
      state.sort = 'distance';
      const sortSelect = $('#sort-select');
      if (sortSelect) sortSelect.value = 'distance';

      sortList();
      renderCards();
      renderMarkers();
    },
    (err) => {
      btn?.classList.remove('is-locating');
      btn?.classList.remove('is-active');
      btn?.setAttribute('aria-pressed', 'false');
      console.warn('GPS error:', err.message);

      let msg = 'Gagal mengambil lokasi GPS';
      if (err.code === 1) msg = 'Izin lokasi ditolak';
      else if (err.code === 2) msg = 'Posisi tidak tersedia';
      else if (err.code === 3) msg = 'Waktu pencarian lokasi habis';

      if (statusEl) {
        statusEl.hidden = false;
        statusEl.textContent = msg;
        setTimeout(() => { statusEl.hidden = true; }, 4500);
      }
    },
    { enableHighAccuracy: true, timeout: 12000, maximumAge: 30000 },
  );
}

/* ------------------------------ cards ------------------------------ */
const cardTpl = document.getElementById('tpl-card');
let cardObserver = null;

function thumbUrl(cam) {
  if (cam.source === 'seribuwajah') return `/api/cameras/${cam.id}/snapshot`;
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
    if (cam.id === state.selectedId) node.classList.add('is-selected');

    const img = $('img', node);
    img.alt = cam.name;
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

    // Tags & distance
    const tagsEl = $('.card-tags', node);
    for (const t of cam.tags) {
      const b = document.createElement('span');
      b.className = `cat-chip cat-${t}`;
      b.textContent = fmtTag[t] || t;
      tagsEl.append(b);
    }

    const distEl = $('.card-distance', node);
    if (distEl && cam.distanceM != null) {
      distEl.hidden = false;
      distEl.textContent = `· ${formatDistance(cam.distanceM)}`;
    }

    $('[data-play]', node).addEventListener('click', (e) => {
      e.stopPropagation();
      openDrawer(cam);
    });
    $('[data-multi]', node).addEventListener('click', (e) => {
      e.stopPropagation();
      addToCustomMulti(cam.id);
    });

    node.addEventListener('click', () => {
      map?.flyTo({ center: [cam.longitude, cam.latitude], zoom: 15, essential: true });
      openDrawer(cam);
    });

    frag.append(node);
  }

  container.append(frag);

  cardObserver = new IntersectionObserver(
    (entries) => {
      for (const ent of entries) {
        if (!ent.isIntersecting) continue;
        const img = ent.target;
        if (img.dataset.src && !img.src) img.src = img.dataset.src;
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
  destroyHls(drawerHls);
  drawerHls = null;
  if (state.selectedId) state.players.delete(state.selectedId);

  state.selectedId = cam.id;
  $('#drawer-name').textContent = cam.name;
  $('#drawer').hidden = false;

  // Clear MapLibre popups so no map chrome sits on the video
  for (const m of markersById.values()) m.getPopup()?.remove();

  // Selection state
  $$('.card').forEach((c) => c.classList.toggle('is-selected', c.dataset.id === cam.id));
  $$('.cctv-marker').forEach((m) => m.classList.toggle('is-selected', m.title.startsWith(cam.name)));

  renderPlayer($('#drawer-player'), cam);

  const meta = $('#drawer-meta');
  meta.innerHTML = '';
  const rows = [
    ['Kecamatan', cam.district || '—', false],
    ['Kelurahan', cam.subdistrict || '—', false],
    ['Alamat', cam.cityAddress || '—', false],
    ['Kategori', cam.category || '—', false],
    ['Sumber', cam.source === 'seribuwajah' ? 'Seribu Wajah' : 'Polresta', false],
    ['Status', cam.status === 'online' ? 'Online' : 'Offline', false],
    ['Tipe', hlsSrc(cam) ? 'HLS (proxied)' : cam.embedUrl ? 'Embed' : 'Snapshot', false],
    ['Koordinat', `${cam.latitude.toFixed(5)}, ${cam.longitude.toFixed(5)}`, true],
    ['Jarak', cam.distanceM != null ? formatDistance(cam.distanceM) : '—', true],
    ['Tag', cam.tags.map((t) => fmtTag[t] || t).join(', ') || '—', false],
  ];

  for (const [k, v, isNum] of rows) {
    meta.append(el('dt', { textContent: k }));
    const dd = el('dd', { textContent: v });
    if (isNum) dd.dataset.num = '1';
    meta.append(dd);
  }

  $('#drawer-big').onclick = () => openBig(cam);
  $('#drawer-add').onclick = () => addToCustomMulti(cam.id);
}

function closeDrawer() {
  $('#drawer').hidden = true;
  destroyHls(drawerHls);
  drawerHls = null;
  $('#drawer-player').innerHTML = '';
  state.selectedId = null;
  $$('.card').forEach((c) => c.classList.remove('is-selected'));
  $$('.cctv-marker').forEach((m) => m.classList.remove('is-selected'));
}

function destroyHls(inst) {
  try { inst?.destroy(); } catch { /* ignore */ }
}

function hlsSrc(cam) {
  if (cam.status !== 'online') return null;
  if (cam.source === 'seribuwajah') return cam.streamUrl ? `/api/cameras/${cam.id}/stream` : null;
  if (cam.source === 'polresta') return cam.token || cam.embedUrl ? `/api/cameras/${cam.id}/stream` : null;
  return cam.playback === 'hls' ? `/api/cameras/${cam.id}/stream` : null;
}

function renderPlayer(host, cam) {
  host.innerHTML = '';

  if (cam.status !== 'online') {
    host.append(el('div', {
      className: 'player-offline',
      textContent: 'Kamera offline',
      style: 'display:flex;align-items:center;justify-content:center;height:100%;color:var(--text-muted);font-size:13px',
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
      style: 'position:absolute;color:var(--text-muted);font-size:12px;pointer-events:none',
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

  host.append(el('img', {
    src: `/api/cameras/${cam.id}/snapshot`,
    alt: cam.name,
    style: 'width:100%;height:100%;object-fit:contain;background:#000',
  }));
}

function attachHls(video, src, key, cam) {
  if (window.Hls && Hls.isSupported()) {
    return attachHlsJs(video, src, key, cam);
  }
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
      style: 'position:absolute;bottom:8px;right:8px;background:rgba(5,8,15,0.85);color:var(--text-secondary);font-size:11px;padding:3px 8px;border-radius:4px;border:1px solid var(--border)',
    }));
    host.append(wrap);
  }
}

/* --------------------------- big view ---------------------------- */
function openBig(cam) {
  state.msIds = [cam.id];
  state.msPreset = 'custom';
  showMulti();
}

/* ──────────────────── Multi-stream + Custom curation ────────────── */
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
    cols: 'repeat(auto-fit, minmax(320px, 1fr))',
    rows: 'auto',
    areas: null,
  },
};

function loadCustomMulti() {
  try {
    const raw = localStorage.getItem(STORAGE_KEYS.customMulti);
    if (raw) {
      const parsed = JSON.parse(raw);
      if (Array.isArray(parsed)) state.customIds = parsed;
    }
  } catch { /* ignore */ }
}

function saveCustomMulti() {
  try {
    localStorage.setItem(STORAGE_KEYS.customMulti, JSON.stringify(state.customIds));
  } catch { /* ignore */ }
}

function addToCustomMulti(id) {
  if (!state.customIds.includes(id)) {
    state.customIds.push(id);
    saveCustomMulti();
  }
  state.msPreset = 'custom';
  showMulti();
}

function removeFromCustomMulti(id) {
  state.customIds = state.customIds.filter((x) => x !== id);
  saveCustomMulti();
  renderMulti();
}

function showMulti() {
  $('#multistream').hidden = false;
  const isCustom = state.msPreset === 'custom';
  $('#ms-picker').hidden = !isCustom;

  renderMulti();

  $$('.ms-preset').forEach((b) => {
    b.classList.toggle('is-active', b.dataset.preset === state.msPreset);
  });
}

function closeMulti() {
  $('#multistream').hidden = true;
  stopRotate();
  for (const [key, inst] of state.players) {
    if (key.startsWith('ms:')) {
      destroyHls(inst);
      state.players.delete(key);
    }
  }
  $('#ms-grid').innerHTML = '';
  $('#ms-results').hidden = true;
}

function currentPresetCameras() {
  if (state.msPreset === 'custom') {
    // If user opened with an explicit one-off (like openBig), honour it
    const ids = state.msIds.length ? state.msIds : state.customIds;
    return ids
      .map((id) => state.all.find((c) => c.id === id))
      .filter(Boolean);
  }
  const p = state.presets.find((x) => x.id === state.msPreset);
  const ids = p?.cameraIds || p?.camera_ids || [];
  return ids.map((id) => state.all.find((c) => c.id === id)).filter(Boolean);
}

function renderMulti() {
  const grid = $('#ms-grid');
  grid.innerHTML = '';

  for (const [key, inst] of state.players) {
    if (key.startsWith('ms:')) {
      destroyHls(inst);
      state.players.delete(key);
    }
  }

  const isCustom = state.msPreset === 'custom';
  $('#ms-picker').hidden = !isCustom;

  const cams = currentPresetCameras();
  const cfg = MS_GRIDS[state.msPreset] || MS_GRIDS.custom;

  grid.dataset.preset = state.msPreset;
  grid.style.gridTemplateColumns = cfg.cols;
  grid.style.gridTemplateRows = cfg.rows;
  grid.style.gridTemplateAreas = cfg.areas || 'none';

  if (!cams.length) {
    const emptyMsg = isCustom
      ? 'Susunan multi-stream Anda masih kosong. Cari kamera pada bilah pencarian di atas, atau klik tombol "Multi" pada daftar kamera.'
      : 'Belum ada kamera pada preset ini.';
    grid.append(el('div', {
      className: 'ms-empty',
      style: 'grid-column:1/-1;display:flex;align-items:center;justify-content:center;text-align:center;padding:48px 24px;line-height:1.6',
      textContent: emptyMsg,
    }));
    return;
  }

  cams.forEach((cam, i) => {
    const tile = el('div', { className: 'ms-tile' });
    if (cfg.areas) {
      const key = Object.keys(cfg.map || {}).find((k) => cfg.map[k] === i);
      tile.style.gridArea = key || 'auto';
    }

    const head = el('div', { className: 'ms-tile-header' });
    head.append(el('span', { className: 'ms-tile-title', textContent: cam.name }));
    head.append(el('span', { className: 'ms-tile-sub', textContent: cam.district || '' }));
    tile.append(head);

    // Delete button on custom tiles
    if (isCustom) {
      const rm = el('button', {
        className: 'ms-tile-remove',
        title: `Hapus ${cam.name} dari susunan`,
        'aria-label': `Hapus ${cam.name} dari susunan custom`,
      });
      rm.innerHTML = '<svg viewBox="0 0 24 24"><path d="M18 6L6 18M6 6l12 12" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"/></svg>';
      rm.addEventListener('click', (e) => {
        e.stopPropagation();
        // If msIds was a one-off (like openBig), clear it too
        state.msIds = state.msIds.filter((x) => x !== cam.id);
        removeFromCustomMulti(cam.id);
      });
      tile.append(rm);
    }

    const content = el('div', { className: 'ms-tile-content' });
    tile.append(content);

    tile.addEventListener('dblclick', () => openDrawer(cam));
    grid.append(tile);

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

function initMultiSearch() {
  const input = $('#ms-search');
  const results = $('#ms-results');
  if (!input || !results) return;

  function update() {
    const q = input.value.trim().toLowerCase();
    if (!q) { results.hidden = true; return; }

    const matches = state.all
      .filter((c) => {
        const hay = `${c.name} ${c.district || ''} ${c.subdistrict || ''}`.toLowerCase();
        return hay.includes(q);
      })
      .slice(0, 8);

    if (!matches.length) {
      results.hidden = false;
      results.innerHTML = '<div class="ms-empty">Tidak ada kamera yang cocok</div>';
      return;
    }

    results.hidden = false;
    results.innerHTML = '';
    matches.forEach((cam) => {
      const already = state.customIds.includes(cam.id);
      const b = el('button', {
        className: 'ms-result',
        type: 'button',
      });
      const body = el('span', { className: 'ms-result-body' });
      body.append(el('span', { className: 'ms-result-name', textContent: cam.name }));
      body.append(el('span', {
        className: 'ms-result-sub',
        textContent: [cam.district, cam.subdistrict].filter(Boolean).join(' · ') || '—',
      }));
      b.append(body);
      b.append(el('span', {
        className: 'ms-result-add',
        textContent: already ? '✓ Ada' : '+ Tambah',
      }));

      b.addEventListener('click', () => {
        addToCustomMulti(cam.id);
        input.value = '';
        results.hidden = true;
      });
      results.append(b);
    });
  }

  input.addEventListener('input', update);
  input.addEventListener('keydown', (e) => {
    if (e.key === 'Escape') { results.hidden = true; }
    if (e.key === 'Enter') {
      const first = $('.ms-result', results);
      if (first) first.click();
    }
  });

  document.addEventListener('click', (e) => {
    if (!e.target.closest('#ms-picker')) results.hidden = true;
  });
}

/* Rotation */
function toggleRotate() {
  state.msRotate = state.msRotate ? 0 : 15;
  const btn = $('#ms-rotate');
  btn.setAttribute('aria-pressed', String(!!state.msRotate));
  if (state.msRotate) startRotate();
  else stopRotate();
}

function startRotate() {
  stopRotate();
  state.msRotateTimer = setInterval(() => {
    const cams = currentPresetCameras();
    if (cams.length < 2) return;
    if (state.msPreset === 'custom') {
      state.customIds.unshift(state.customIds.pop());
      saveCustomMulti();
    } else {
      state.msIds = cams.map((c) => c.id);
      state.msIds.unshift(state.msIds.pop());
    }
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
    if (s === 'distance') {
      const da = a.distanceM ?? Infinity;
      const db = b.distanceM ?? Infinity;
      return da - db;
    }
    if (s === 'district') return (a.district || '').localeCompare(b.district || '');
    if (s === 'status') return (a.status === 'online' ? -1 : 1) - (b.status === 'online' ? -1 : 1);
    return a.name.localeCompare(b.name);
  });
}

/* ------------------------------ init ------------------------------ */
async function boot() {
  loadCustomMulti();

  // Restore saved theme
  try {
    const savedTheme = localStorage.getItem(STORAGE_KEYS.theme);
    if (savedTheme) document.documentElement.dataset.theme = savedTheme;
  } catch { /* ignore */ }

  // Restore saved basemap preference
  try {
    const savedBasemap = localStorage.getItem(STORAGE_KEYS.basemap);
    if (savedBasemap) state.basemap = savedBasemap;
  } catch { /* ignore */ }

  initMap();
  initGPS();
  initMultiSearch();

  // Rail toggle / handle
  const rail = $('#camera-list');
  const toggleBtn = $('#rail-toggle');
  const handleBtn = $('#rail-handle');

  function toggleRail(force) {
    const next = force != null ? force : rail.dataset.collapsed !== 'true';
    rail.dataset.collapsed = String(next);
    toggleBtn?.setAttribute('aria-pressed', String(!next));
    try { localStorage.setItem(STORAGE_KEYS.railCollapsed, String(next)); } catch { /* ignore */ }
  }

  toggleBtn?.addEventListener('click', () => toggleRail());
  handleBtn?.addEventListener('click', () => toggleRail());

  try {
    const savedRail = localStorage.getItem(STORAGE_KEYS.railCollapsed);
    if (savedRail === 'true') toggleRail(true);
  } catch { /* ignore */ }

  // Search
  $('#search').addEventListener('input', (e) => {
    state.query = e.target.value;
    applyFilters();
  });

  // Source & sort
  $('#source-filter').addEventListener('change', (e) => {
    state.activeSource = e.target.value;
    applyFilters();
  });

  $('#sort-select').addEventListener('change', (e) => {
    state.sort = e.target.value;
    sortList();
    renderCards();
  });

  // Drawer / Multi triggers
  $('#drawer-close').addEventListener('click', closeDrawer);
  $('#ms-close').addEventListener('click', closeMulti);
  $('#ms-rotate').addEventListener('click', toggleRotate);
  $('#ms-open-btn').addEventListener('click', () => {
    // If user has custom picks, default to custom; else traffic-corridor
    state.msPreset = state.customIds.length ? 'custom' : 'traffic-corridor';
    state.msIds = [];
    showMulti();
  });

  $$('.ms-preset').forEach((btn) => {
    btn.addEventListener('click', () => {
      state.msPreset = btn.dataset.preset;
      state.msIds = [];
      showMulti();
    });
  });

  // Zoom buttons
  $('#zoom-in')?.addEventListener('click', () => map?.zoomIn());
  $('#zoom-out')?.addEventListener('click', () => map?.zoomOut());

  // Basemap toggle: CARTO (themed) <-> Esri satellite
  $('#basemap-btn')?.addEventListener('click', () => {
    const next = state.basemap === 'satellite' ? 'auto' : 'satellite';
    switchBasemap(next);
  });
  if (state.basemap === 'satellite') {
    $('#basemap-btn')?.classList.add('is-active');
    $('#basemap-btn')?.setAttribute('aria-pressed', 'true');
  }

  // Theme toggle
  $('#theme-toggle').addEventListener('click', () => {
    const html = document.documentElement;
    const next = html.dataset.theme === 'dark' ? 'light' : 'dark';
    html.dataset.theme = next;
    try { localStorage.setItem(STORAGE_KEYS.theme, next); } catch { /* ignore */ }

    // If on auto basemap, reload map style to match theme
    if (state.basemap === 'auto') {
      map?.setStyle(resolveCurrentStyle());
    }
  });

  // Escape closes overlays
  document.addEventListener('keydown', (e) => {
    if (e.key !== 'Escape') return;
    if (!$('#multistream').hidden) closeMulti();
    else if (!$('#drawer').hidden) closeDrawer();
  });

  // Load cameras & presets
  try {
    const [data, presets] = await Promise.all([API.cameras(), API.presets()]);
    state.all = data.cameras;
    state.presets = presets.presets || [];
    computeDistances();
    applyFilters();
  } catch (err) {
    console.error(err);
    $('#list-count').textContent = 'Gagal memuat data';
    $('#cards').append(el('p', {
      textContent: 'Tidak dapat memuat data kamera. Pastikan server berjalan.',
      style: 'padding:16px;color:var(--status-offline);font-size:12.5px',
    }));
  }
}

boot();
