# Kebijakan Git dan GitHub

## Batas repositori

- Root Git: direktori lokal **`Atoryn/Studio/`**, bukan parent Atoryn.
- Remote `origin`: `https://github.com/TakuyaSeija/RuneCLI.git`.
- Branch utama: `main`, mengikuti `origin/main`.
- Path repo di GitHub langsung dimulai dari `Engine/`, `Cli/`, dan `Cargo.toml`; tidak ada lapisan `Studio/` di dalam repo.
- Atoryn hanya folder penampung. Proyek lain memiliki repositori Git dan remote sendiri, bukan bagian dari repo Studio.
- Jangan menjalankan `git init` lagi di `Studio/`, `Engine/`, atau `Cli/`; Studio sudah memiliki Git dan kedua crate berada dalam repo yang sama.
- Git menyimpan riwayat lokal. GitHub menerima perubahan hanya setelah commit dan push.
- Lisensi dan identitas commit ditentukan pemilik, bukan diasumsikan oleh setup.

Semua path berkas di bawah **relatif terhadap root repo Studio**, meskipun
nama folder lokal berubah ketika di-clone. Perintah Git dijalankan dari
Studio; dari Atoryn gunakan `git -C Studio ...` bila tidak ingin pindah direktori.

## Masuk Git

| Berkas | Alasan |
| --- | --- |
| `Engine/`, `Cli/src/` | Source dan kontrak implementasi |
| `Engine/tests/`, `Cli/tests/`, skrip verifikasi | Tes dan fixture sintetis yang sudah diperiksa |
| `Engine/Schema.sql` | Schema sumber, bukan database runtime |
| Semua `Cargo.toml` dan `Cargo.lock` | Manifest dan dependency terkunci untuk aplikasi yang dapat direproduksi |
| `.cargo/config.toml` | Konfigurasi bersama dengan path relatif untuk tes |
| `Cli/Assets/*.png`, `*.rgba` | Input aplikasi; sebagian RGBA di-embed saat kompilasi |
| `AGENTS.md`, `CLAUDE.md`, README, `docs/architecture.md`, LICENSE setelah dipilih | Instruksi inti sinkron dan kontrak arsitektur rinci |
| `rust-toolchain.toml`, `.editorconfig` | Baseline toolchain lokal/CI dan whitespace editor bersama |
| `.gitignore`, `.gitattributes`, `.github/workflows/`, `scripts/` | Kebijakan dan otomasi proyek |
| `.rune/skills/` bila kelak ada | Skill proyek yang sengaja dibagikan; harus bebas rahasia |
| `.env.example` / `.env.*.example` | Contoh konfigurasi dengan placeholder saja |

**Jangan mengabaikan semua `*.lock`, `*.json`, `*.sql`, atau berkas biner.**
`Cargo.lock`, fixture JSON, schema SQL, dan aset PNG/RGBA merupakan input
proyek. Aset saat ini kecil; Git LFS belum diperlukan. Bila menambah aset
besar, tentukan kebijakannya sebelum commit.

## Tidak masuk Git

| Berkas/folder | Perlakuan |
| --- | --- |
| `target/` (termasuk bila dibuat di crate) | Build, binary, dependency hasil kompilasi, test-tmp, dan artefak audit |
| `tmp/` di root repo | Scratch/catatan lokal; jangan digunakan sebagai source atau dependency build |
| `__pycache__/`, `*.pyc`, cache tes, virtualenv Python | Dibuat ulang secara lokal |
| `.env`, `.env.*` selain contoh | Bisa berisi konfigurasi dan kredensial pribadi |
| `.cargo/credentials*`, private key/certificate bundle | Kredensial, bukan konfigurasi bersama |
| Data `.rune/` selain skills; `rune.db*`, `master.key`, `secrets.vault` | Runtime, riwayat, checkpoint, memo pribadi, dan rahasia |
| `rune.toml` di root repo | Konfigurasi runtime pribadi; contoh bersama harus memakai nama berbeda dan placeholder |
| State Claude/editor lokal, swap/backup file, metadata OS | Spesifik mesin dan dapat dibuat ulang |

Memo sesi disimpan di `~/.rune/projects/<project>/memory/`, di luar
repositori. Jangan menyalinnya ke dokumentasi publik tanpa peninjauan dan
sanitasi. `tmp/1.txt` dan `tmp/2.txt` adalah laporan lokal, bukan fixture.

`.gitignore` bukan pemindai rahasia dan **tidak menghapus file yang sudah
tracked**. Token dalam source atau dokumentasi tetap bisa bocor meskipun
nama filenya tidak terlihat sensitif. Gunakan data sintetis untuk fixture.

## Kontrol otomatis dan batas audit

`check_git_policy.py` memeriksa root repository, input wajib, source yang
tersembunyi oleh ignore, data privat yang dipaksa tracked, Git bersarang,
attributes teks/biner, sinkronisasi instruksi, dan anggaran injeksi `AGENTS.md`
32.768 karakter. `.editorconfig` menyamakan format editor dengan attributes;
`rust-toolchain.toml` menjadi sumber baseline yang sama bagi lokal dan CI.
State Claude lokal (history, projects, cache, credentials) diabaikan, sedangkan
`.claude/settings.json` tetap boleh dibagikan setelah diperiksa bebas rahasia.

Instruksi inti membaca `docs/architecture.md` ketika mengubah interaksi
engine/UI/provider/store. Dokumen rinci dipisahkan agar aturan pemilik tidak
hilang akibat pemotongan konteks; isi kontraknya tetap berlaku.

Lulus pemeriksaan tidak membuktikan tidak ada token di isi file, keamanan
seluruh dependency, hak push, atau aturan server GitHub. Identitas commit,
lisensi, commit/push source, branch protection dan secret scanning tetap
keputusan/konfigurasi pemilik. Checker tidak mengubah index, identitas global,
remote, atau file di parent repository.

## Sebelum commit pertama source

Dari folder Atoryn, masuk ke `Studio/` dahulu. Saat membuka terminal langsung
di Studio atau setelah clone, tidak perlu menjalankan `cd Studio` lagi.
Nama/email di bawah adalah placeholder; ganti dengan identitas milikmu.
Untuk menjaga privasi email di GitHub, gunakan alamat noreply yang diberikan
Settings → Emails, jangan mengarangnya.

```bash
# Hanya bila terminal masih berada di Atoryn:
cd Studio

# Repo-local: tidak mengubah identitas Git proyek lain.
git config --local user.name "NAMA KAMU"
git config --local user.email "EMAIL COMMIT KAMU"

# Tambahkan LICENSE bila sudah dipilih; jangan gunakan kredensial live di contoh.
python3 scripts/check_git_policy.py --check-tracked
python3 -B scripts/test_git_policy.py
git status --short --branch
git status --short --ignored

# Review daftar sebelum staging; tmp/ dan target/ tidak boleh muncul sebagai calon file.
git add --dry-run --all

# Hanya setelah kamu puas dengan isinya:
git add --all
python3 scripts/check_git_policy.py --check-tracked
git diff --cached --stat
git diff --cached --check
git diff --cached

# Jika semua sudah benar:
git commit -m "Set up Studio and RuneCLI repository"
```

Untuk membatalkan staging tanpa menghapus perubahan file, gunakan
`git restore --staged <path>`. Jangan `git add -f` terhadap data yang diabaikan
kecuali benar-benar sudah disanitasi dan ada alasan yang ditinjau.

## Push ke GitHub

```bash
git fetch origin
git status --short --branch
# Jika remote maju, selesaikan integrasi riwayat dahulu. Jangan force-push.
git push -u origin main
```

Remote HTTPS dapat dibaca tanpa login jika publik. **Hak push memerlukan
autentikasi** dengan akun yang punya akses tulis: credential manager/PAT
untuk HTTPS atau SSH yang sudah disiapkan. Jangan memasukkan token ke URL,
file proyek, command yang tersimpan, atau memo. Password akun GitHub biasa
bukan autentikasi Git HTTPS. GitHub CLI tidak wajib.

`pull.ff=only` dipasang repo-local: `git pull` menolak divergensi, alih-alih
membuat merge diam-diam. Pilih merge/rebase secara sadar bila memang perlu.

## Pengembangan selanjutnya

```bash
git switch -c nama-perubahan
# Edit, uji, review, commit.
git push -u origin nama-perubahan
```

Buat pull request ke `main`. Workflow Linux berjalan dari root repo Studio
untuk push/PR ke `main` atau secara manual. Workflow menjalankan pemeriksaan
file Git beserta regression test-nya, `fmt`, `clippy`, dan tes Cargo dengan
dependency `--locked` serta `RUNE_HOME` terisolasi. Tes live yang diabaikan
Cargo dan skrip desktop PTY manual tidak otomatis tercakup. Workflow hanya
punya permission `contents: read`, checkout dipin ke commit, tidak menyimpan
credential Git, dan tidak memakai `pull_request_target` atau secret provider.

Branch protection/ruleset GitHub memerlukan pengaturan pada akun pemilik.
Setelah workflow pernah berjalan, pertimbangkan mewajibkan pull request,
status check **Rust quality gate (Linux)**, melarang force-push, dan menyalakan
secret scanning/push protection bila tersedia. Pengaturan server ini bukan
bagian dari `.gitignore` dan belum otomatis diaktifkan.

## Proyek lain di Atoryn

Buat folder sibling di luar Studio, bukan di dalam repo ini. Contoh struktur:

```text
Atoryn/                  # bukan repo Git
├── Studio/              # repo RuneCLI
│   ├── .git/
│   ├── Engine/
│   ├── Cli/
│   └── ...
└── ProyekLain/           # repo terpisah jika/ketika proyek itu dibuat
    ├── .git/
    └── ...
```

Inisialisasi atau clone repo proyek tersebut di foldernya sendiri. Setiap
repo mempunyai remote, branch, ignore, dokumentasi, dan CI sesuai kebutuhan.
Jangan menambahkan Git atau berkas khusus Studio ke root Atoryn, dan jangan
membuat repo dummy untuk subproyek yang belum ada. Bila benar-benar perlu
repo eksternal di dalam Studio, pilih vendor/submodule secara eksplisit,
bukan meninggalkan `.git/` bersarang tanpa sengaja.
