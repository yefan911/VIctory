# VIctory

*Read this in: [English](../README.md)*

Menjalankan Grand Theft Auto VI (Xbox Series X|S) secara native di Windows, dari file yang disediakan oleh pengguna.

Kode x86-64 milik game berjalan tanpa perubahan di dalam proses kita; VIctory menyediakan platform di bawahnya dan mengonversi data game — di muka jika memungkinkan, dan saat pertama dipakai jika tidak (shader). Lihat [docs/architecture.md](../docs/architecture.md).

## Status (2026-11-23)

**Belum bisa dimainkan.** Game berhasil boot, menjalankan menu-menunya di 30 fps, dan memuat dunia — namun rendering-nya gagal.

| Tahap | Status |
|-------|--------|
| Boot, layar pertama kali (aksesibilitas, bahasa, pengaturan, hukum) | berjalan, 30 fps |
| Video intro dengan suara | berjalan |
| Layar start, profil dan save | berjalan |
| Loading ke dalam dunia (sekitar 50 detik) | berjalan |
| Dunia (intro misi pertama) | tercapai dan berjalan dengan indirect draws; title card-nya merender, scene setelahnya masih gelap; 0,5–1,8 detik per frame |

Diuji pada satu mesin: Windows 11, AMD RX 6700 XT.

> **Peringatan.** Memasuki dunia di GPU nyata saat ini dapat membuatnya hang, lalu Windows akan mereset driver display (semua monitor menjadi hitam sesaat). Untuk investigasi lebih lanjut, gunakan diagnostics build dengan `VICTORY_D3D12_WARP=1`, yang merender di atas software adapter.

Apa yang dikerjakan setiap bagian saat ini dan apa yang masih dibutuhkan:

| Subsistem | Saat Ini | Yang Dibutuhkan |
|-----------|----------|-----------------|
| Loader | Menjalankan executable dan DLL-nya di dalam proses kita; laporan crash menyebut kode yang bermasalah dan disimpan di `crashes.log` | – |
| Runtime services | Satu pengguna offline yang sudah login, lisensi, paket, save yang bertahan walau dimatikan di langkah mana pun, dialog input teks | Speech masih bisu; dialog dan relaunch belum diuji secara live |
| Network | Offline by design: game tidak bisa mengakses jaringan | – |
| File dan streaming | Arsip dan data streaming terbaca, zlib dan BCPack didekode secara paralel | Loading dibatasi oleh penulisan ke memori game |
| Input | XInput pad, keyboard, keyboard sebagai pengganti pad | Trigger rumble |
| Audio | Suara menu, video, dan loading; XMA voices dan Opus stream terdekode | Reverb masih bisu; hasil Opus belum diverifikasi terhadap game |
| GPU: menu | Draw, compute, dan presentasi di host D3D12; hanya memori game yang berubah yang disalin | Frame pacing drift di satu layar pada release build |
| GPU: dunia | Mesh pipeline, shader-written buffer, descriptor lifetime, indirect execution (argumen di-clamp), pipeline yang dibuat oleh worker thread dan disimpan ke disk | Sebagian besar scene masih hilang (shader yang belum diterjemahkan menggugurkan draw); frame memakan waktu detik-an; real query, depth bounds, HDR, dan scaling belum ada |
| Shader | Kode GPU konsol dikompilasi ulang ke DXIL saat pertama dipakai, disimpan di cache disk. Menu: semua. Dunia: mesh 8.191 dari 8.506, vertex/pixel 10.617 dari 12.346, compute 1.643 dari 2.539 | 64-bit image atomic, merged geometry stage, shader untuk ray-tracing builder; beberapa program compute memerlukan GPU dengan 64-lane wave |
| Tekstur | Tiled layout konsol dan detiling untuk mode yang dipakai game | Render-target metadata plane |
| Mode Series S | Berjalan melewati menu hingga layar loading | Dunia, sama seperti di atas |

## Memulai

Persyaratan: Windows 10/11 x64, CPU dengan AVX2, GPU Direct3D 12 dengan shader model 6.6 (dan mesh shader untuk dunia), Rust (versi dikunci di `rust-toolchain.toml`).

Game juga membutuhkan beberapa GiB Windows commit yang tersedia saat startup. Periksa **Committed** di **Task Manager → Performance → Memory**. Jika `crashes.log` melaporkan Windows error 1455, tutup aplikasi lain atau perbesar ukuran paging file Windows, lalu coba lagi.

```powershell
cargo xtask install-hooks                # cek kebijakan aset di setiap commit
copy victory.example.toml victory.toml   # lalu atur path kamu (di luar repo)
cargo test --workspace                   # tidak membutuhkan data game
cargo run -p victory -- config           # periksa path kamu
```

Jalankan game:

```powershell
cargo build --release -p victory
target\release\victory.exe run <direktori game>\<executable game> --execute
```

Caption jendela menampilkan framerate. Save, pengaturan, dan `crashes.log` disimpan di folder data (default `%LOCALAPPDATA%\victory\data`), data turunan di folder cache.

Input: XInput pad. Jika tidak ada yang terhubung, keyboard berfungsi sebagai pad selama jendela game dalam fokus:

| Tombol | Pad |
|--------|-----|
| W A S D / I J K L | stick kiri / kanan |
| arrow keys | D-pad |
| Enter, Backspace, X, Y | A, B, X, Y |
| Q / E, Z / C | shoulder button, trigger |
| F / H | klik stick |
| Esc, Tab | Menu, View |

## Perintah

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <argumen game>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (release build mencatat info ke atas).

## Debugging

Release build tidak memiliki alat bantu debugging. Aktifkan dengan:

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variabel | Efek |
|----------|------|
| `VICTORY_LOG=<target>=debug,...` | output debug dan trace |
| `VICTORY_STACKS=20,60` | stack setiap thread pada detik-detik tersebut |
| `VICTORY_WATCH=<rva>,...` | mencatat u32 milik game saat nilainya berubah |
| `VICTORY_KEYS=30:Enter,35:X` | menekan tombol pada detik-detik tersebut, dengan atau tanpa fokus |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | menyimpan memori game ke folder cache saat crash |
| `VICTORY_CAPTURE=<dir>` | menyimpan frame yang dipresentasikan (pangkat dua, atau `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | bersama dua di atas, menyimpan setiap render target dari frame tersebut |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | bersama di atas, GPU-based validation (penggunaan descriptor buruk dilaporkan, bukan di-fault; lambat) |
| `VICTORY_D3D12_DRED=1` | saat terjadi kegagalan GPU, alokasi yang terkena dampak dan perintah yang sedang berjalan |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | lewati draw dan dispatch, termasuk yang indirect, yang pipeline-nya mengandung hash shader 16-digit yang dicantumkan |
| `VICTORY_GPU_TIMING=1` | bersama di atas, setiap daftar submission dijalankan dan diukur waktunya sendiri; daftar yang lambat dicatat bersama perintah-perintah paling lambatnya |
| `VICTORY_D3D12_WARP=1` | perangkat host di software adapter (lambat; tidak bisa mereset driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | loop shader yang sudah diterjemahkan berhenti setelah `n` iterasi per lane, bukan 2^20 (entri cache shader tersendiri); jika `n` yang lebih kecil membuat frame dunia jauh lebih cepat, berarti loop yang mencapai batas tersebut yang membuatnya lambat |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | merekam audio yang diputar game (proses kita saja) |
| `VICTORY_XMA_DUMP=<dir>` | menyimpan konteks dan input dari 32 XMA voice pertama |

Setiap build menambahkan crash dan kegagalan GPU host ke `crashes.log` di folder data game.

## Lisensi

GPL-3.0-or-later. Lihat [LICENSE](../LICENSE).
