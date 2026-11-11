# CCTV Bandar Lampung — Unified Dashboard & Multi-Stream

Aplikasi terpadu pemantau CCTV Kota Bandar Lampung yang menggabungkan seluruh titik kamera dari dua portal resmi:
1. **Diskominfo Kota Bandar Lampung** — *Seribu Wajah* (`https://seribuwajah.bandarlampungkota.go.id/map`)
2. **Polresta Bandar Lampung** — *Layanan Kota Presisi* (`https://restabandarlampung.lampung.polri.go.id/city`)

Dibangun sepenuhnya dalam bahasa **Rust** (Cargo workspace: `web-core`, `api`, `cli`), menggunakan antarmuka modern berbasis **MapLibre GL**, **HLS.js**, dan tata letak multi-stream adaptif yang mendukung seluruh ukuran viewport (ponsel, tablet, desktop, hingga TV wall).

---

## Fitur Utama

- **Penggabungan Data Otomatis & Deduplikasi**:
  - **383 titik mentah** dari kedua portal (264 Seribu Wajah + 119 Polresta).
  - Menjadi **370 kamera unik** setelah deduplikasi: 13 titik Polresta yang berada dalam radius < 50 meter dan bernama serupa dilebur ke 11 rekaman Seribu Wajah, sehingga rekaman hasil gabungan menyimpan **dua sumber stream sekaligus** (HLS dari Diskominfo + iframe embed Polresta sebagai cadangan).
- **Klasifikasi Cerdas (Auto-Tagging)**:
  - 🚦 **Pantau Macet** (`macet`): **190 kamera** — ruas jalan protokol, flyover, underpass, pertigaan, perempatan, jembatan, dan bundaran arteri.
  - 🏛️ **Pantau Lokasi Strategis** (`strategis`): **222 kamera** — kantor pemerintahan kota, kantor kecamatan/kelurahan, puskesmas, pasar, RS, terminal, stasiun, mall, rumah ibadah.
  - 🔄 **PTZ** (`ptz`): **24 kamera** kendali Pan-Tilt-Zoom (21 Seribu Wajah + 3 Polresta seperti Al-Bakrie dan Tj Maut).
  - Satu kamera dapat memiliki lebih dari satu tag (mis. pertigaan PTZ = `macet` + `ptz`), sehingga jumlah per kategori melebihi total kamera.
- **Template Multi-Stream & Frame Kustom**:
  - **Koridor 2×2**: 4 kamera ruas jalan arteri berdampingan.
  - **Simpang 1 Utama + 3 PiP**: 1 kamera utama resolusi penuh + 3 sudut persimpangan (otomatis beradaptasi vertikal di layar ponsel).
  - **Kluster 3×2**: 6 kamera kawasan strategis/kantor pemerintahan.
  - **Custom Grid**: pilih kamera mana saja dari daftar, susun grid bebas.
  - **Auto-Rotation**: rotasi otomatis antar kamera setiap 15 detik.
- **HLS Proxy dengan Bypass Hotlink**:
  - Proxy streaming terintegrasi menyuntikkan header `Origin` dan `Referer` resmi Seribu Wajah untuk mengatasi proteksi Cloudflare hotlink.
  - Resolusi tiket sesi Polresta (ITN gateway) dengan manajemen TTL 45 detik dan *automatic fallback* ke iframe embed jika gateway sedang warm-up.
- **Liveness Probe**:
  - Verifikasi status muxer upstream secara berkala untuk menyaring kamera yang berstatus `online` di API tetapi mengembalikan galat `500 muxer instance not available`.
- **Dukungan Penuh Seluruh Viewport**:
  - **Ponsel (≤ 600px)**: bilah kategori horizontal yang dapat digeser, layout multi-stream adaptif 1-atas + 3-bawah.
  - **Tablet (600–900px)**: split view vertikal (peta atas, daftar bawah 45vh).
  - **Desktop & TV Wall (≥ 900px)**: split horizontal layar penuh, akselerasi perangkat keras HLS.

---

## Struktur Proyek

```
.
├── Cargo.toml                  # Workspace manifest (api + web-core + cli)
├── Cargo.lock
├── Dockerfile                  # Multi-stage build (rust:1.99 -> debian:bookworm-slim)
├── docker-compose.yml          # Konfigurasi container port 3447
├── crates/
│   ├── web-core/               # Domain model, deduplikasi, klasifikasi, parser GeoJSON
│   ├── cli/                    # Binary pipeline `cctv-data` (fetch, liveness, emit)
│   └── api/                    # Binary web server `cctv-api` (Axum 0.7, HLS proxy, cache)
└── static/                     # Antarmuka web mandiri (HTML5, CSS3 tokens, Vanilla JS ES modules)
    ├── index.html
    ├── styles.css
    ├── app.js
    └── data/                   # Dataset terpadu cameras.json, GeoJSON, dan presets
```

---

## Cara Menjalankan

### Opsi 1: Menggunakan Docker Compose (Direkomendasikan)

Aplikasi telah dikemas ke dalam satu container ringan yang berjalan pada **port 3447**:

```bash
# Bangun dan jalankan
docker compose up -d --build

# Buka dashboard di peramban
# http://localhost:3447
```

Periksa kesehatan container:
```bash
docker compose ps
# cctv-bandarlampung   cctv-bandarlampung:0.1.0   healthy   0.0.0.0:3447->3447/tcp
```

Memperbarui dataset langsung di dalam container:
```bash
docker compose run --rm --entrypoint cctv-data cctv
```

Menghentikan container:
```bash
docker compose down
```

---

### Opsi 2: Menjalankan Langsung via Cargo (Native Rust)

**Prasyarat**:
- Rust toolchain 1.80+ (`cargo`, `rustc`)
- Akses internet ke portal Diskominfo & Polresta (untuk pembaruan data)

1. **Jalankan Pipeline Pengumpulan Data**:
   ```bash
   # Mengambil data dari upstream, liveness check, klasifikasi, dan emit ke static/data/
   cargo run --release --bin cctv-data

   # Atau jalankan dalam mode offline jika menggunakan data cache yang ada:
   cargo run --release --bin cctv-data -- --offline
   ```

2. **Jalankan API Server di Port 3447**:
   ```bash
   cargo run --release --bin cctv-api -- --bind 127.0.0.1:3447
   ```

   Buka peramban ke: `http://localhost:3447`

---

## API Endpoints

| Method | Endpoint | Deskripsi |
|---|---|---|
| `GET` | `/api/health` | Status kesehatan server dan jumlah kamera termuat |
| `GET` | `/api/cameras` | Daftar seluruh kamera (JSON dengan filter `tag`, `source`, `q`) |
| `GET` | `/api/cameras.geojson` | Data geospasial format GeoJSON FeatureCollection untuk peta |
| `GET` | `/api/presets` | Konfigurasi preset multi-stream (koridor, simpang, kluster) |
| `GET` | `/api/cameras/:id/stream` | Entrypoint HLS proxied (master/index `.m3u8`) |
| `GET` | `/api/cameras/:id/stream/*asset` | Segment video (`.ts`, `.m4s`) & child playlist |
| `GET` | `/api/cameras/:id/snapshot` | Snapshot JPEG terkini dari kamera |

---

## Pengujian & Verifikasi

Unit test terisolasi di `crates/web-core`, `crates/api`, dan `crates/cli` — total **24 test, semuanya lulus**:

```bash
cargo test --workspace
# api: 17 passed | cli: 5 passed | web-core: 2 passed
```

Pengujian antarmuka telah diverifikasi menggunakan Playwright di berbagai breakpoint:
- Mobile Phone (375 × 667)
- Tablet (768 × 1024)
- Desktop / Wide Monitor (1600 × 900)
