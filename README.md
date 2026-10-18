# RuneCLI

Repositori mandiri untuk **Rune**, coding agent yang berjalan lokal di
komputer pengguna. Folder lokal proyek ini adalah **`Studio/`**; saat di-clone,
foldernya bisa bernama `RuneCLI` atau nama lain tanpa mengubah struktur repo.

## Struktur

| Path dalam repo | Isi |
| --- | --- |
| `Engine/` | Engine agent, provider, tools, permissions, sandbox, penyimpanan, dan checkpoint |
| `Cli/` | Binary `rune`, antarmuka terminal, dan host protokol JSON |
| `Cli/Assets/` | Aset logo PNG dan RGBA yang digunakan CLI |
| `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml` | Workspace, dependency terkunci, dan konfigurasi build bersama |
| `AGENTS.md`, `CLAUDE.md`, `docs/architecture.md` | Instruksi inti yang sinkron dan kontrak arsitektur rinci |
| `rust-toolchain.toml`, `.editorconfig`, `.gitattributes` | Toolchain bersama dan aturan format lintas editor/platform |
| `.github/workflows/studio-ci.yml` | Quality gate Linux di GitHub Actions |
| `scripts/` | Pemeriksaan dan tes kebijakan Git |
| `docs/git.md` | Kebijakan berkas repositori dan panduan commit/push |

Target desain Rune adalah Linux, Windows, dan macOS. **Implementasi saat ini
hanya mendukung Linux**; GUI belum memiliki kode.

## Build dan pemeriksaan lokal

Toolchain baseline lokal dan CI ditentukan oleh **`rust-toolchain.toml`**,
dengan komponen `rustfmt` dan `clippy`. Jalankan dari root repo (folder lokal `Studio/`):

```bash
mkdir -p target/test-tmp
cargo build --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings

# Jangan memakai ~/.rune milik pengguna untuk pengujian.
RUNE_HOME="$PWD/target/ci-rune-home" \
  cargo test --workspace --locked --no-fail-fast
```

Konfigurasi `.cargo/config.toml` menempatkan folder sementara pengujian di
`target/test-tmp/`. Skrip Python PTY dan pengujian terminal desktop memiliki
kebutuhan tambahan; workflow standar menjalankan tes Cargo, bukan seluruh
smoke test desktop atau provider live. Baca aturan proyek sebelum menjalankan
fixture atau binary secara manual.

## Git

**Git berada di dalam `Studio/`, bukan di parent `Atoryn/`.** Atoryn hanya
menampung folder proyek; proyek lain nantinya memiliki repo Git, remote,
aturan ignore, dokumentasi, dan CI sendiri. Repo ini hanya mencakup Studio.
Jangan membuat repo Git bersarang di `Engine/` atau `Cli/`.

Source, tes, aset, `Cargo.lock`, dan konfigurasi build proyek ikut Git;
hasil build, cache, rahasia, data Rune, serta `tmp/` tidak.

```bash
# Dari root repo Studio
python3 scripts/check_git_policy.py --check-tracked
python3 -B scripts/test_git_policy.py
git status --short --branch
```

Lihat [`docs/git.md`](docs/git.md) untuk alur commit dan push yang aman.
GitHub Actions tidak melakukan rilis, pemasangan binary, atau deployment.

## Lisensi

Lisensi belum ditentukan oleh pemilik. Ketersediaan source di repositori
publik **bukan pemberian lisensi** untuk penggunaan, modifikasi, atau
distribusi. Tambahkan `LICENSE` ketika pemilik sudah menetapkan ketentuannya.
