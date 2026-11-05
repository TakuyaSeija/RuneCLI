# Deploy (binding)

One entry point at the project root, `DeployStudio.sh`, beside `WebsiteDev.sh`.
It only calls the scripts that already own each job; it holds no deploy logic of
its own.

    DeployStudio.sh plan                 order and current state (read-only)
    DeployStudio.sh status               site releases, health, published rune versions
    DeployStudio.sh build                build the Linux gnu+musl releases that are missing
    DeployStudio.sh rune                 build, then publish the CLI release (asks the signing passphrase)
    DeployStudio.sh website              install units once → release → switch
    DeployStudio.sh all                  rune, then website
    DeployStudio.sh release|switch|rollback|stop-standby    the site alone
    DeployStudio.sh help

`Ops/Release/PublishSet.sh` publishes the released versions in one run: it
creates the signing key once if it is missing, builds the Linux releases still
missing, publishes the current version, then publishes each archived version with
`--binary --version`. Every version is one signing passphrase prompt, because the
signer refuses a passphrase from a pipe. `--dry-run` shows the plan.

The archived versions are not a list in the script: it asks every binary kept in
`~/.local/share/RuneBackups` what version it is and publishes each one it finds
(0.7.8 through the current release, 41 versions when this was written), so a
version found later needs no edit. The changelog's words live with the site, not with the artifact:
`Website/UI/Pages/ChangelogNotes.json` holds them in English (which owns the
versions) and `UI/Pages/<code>/ChangelogNotes.json` holds each translation, and
the site refuses to start unless a translation holds exactly the same versions
with the same number of lines. A version with no entry has no recorded change and
the page says so in that language. Because the words are the site's, a version
published long ago can still gain its notes — a published version is never
replaced.

`Ops/Release/SyncDevReleases.sh` gives the **DEV** site the real published
releases in DEV's own folder (`~/.data/Atoryn/StudioDev/Releases`), copying any
version PROD has and DEV does not. DEV and PROD stay two instances with two
folders — the dev site is never pointed at PROD's directory — so a test build
published into the DEV folder can still never appear as a real release.

`StudioWeb.sh release` freezes the **whole** HEAD, not only `Website/`: the site's
tests read the application's sources (Engine/, Cli/, Cargo.toml) to prove the
manual still matches them, so a release also needs `Cargo.toml` committed with
the version the manual documents.

## Two artifacts, two homes

- **The site** (`/Studio/`): `Ops/Web/StudioWeb.sh` freezes the *committed*
  `Website/` into `~/Deploy/Atoryn/Studio/releases/<id>/`, each with its own
  `atoryn-studio-web` binary, `Config.json` and `Release.json`. `switch` starts
  the new release on the idle colour (blue 3301 / green 3311, health +2), waits
  for health, then points nginx at it; the old colour stays as a rollback
  standby. `Ops/Web/Install.sh` installs the units once and retires the old dev
  unit.
- **The CLI** (`rune`): `Ops/Release/PublishRune.sh` packs the Linux builds with
  the license and notices, writes `release.json` and `SHA256SUMS`, signs the sums
  with the release key, verifies them with the same `openssl` check the installers
  run, and only then moves the version into
  `~/.data/Atoryn/Studio/Releases/<version>/`. A published version is never
  replaced. DEV reads its own `~/.data/Atoryn/StudioDev/Releases`, so a test
  build can never appear as a real release. The site lists it within 30 seconds;
  nothing is restarted. Other
  platforms (macOS, Windows, arm64) come from a green CI run of the same commit
  with `--ci-run <id>`.
- `install.sh` / `install.ps1` are not files on disk: the site renders them from
  `Website/UI/Install/` and serves them at `/Studio/install.sh` and
  `/Studio/install.ps1`. They download `/Studio/download/<version>/<file>` and
  verify `SHA256SUMS.sig` against `SigningKey.pem` in the release root before
  installing. The unit reads that root read-only at `/srv/studio/downloads`, and
  `/Studio/download/<version>/<file>` streams the file (nothing is buffered).

## Dry-run

Every sub-command accepts `--dry-run` (or `STUDIO_DEPLOY_DRY_RUN=1`). All
read-only checks still run for real — `gofmt`/`vet`/`go test`, pointer reading,
health probes, the signer/key/build checks, the version checks — and every step
that would change something is printed as `[dry-run] ...` instead: nothing is
built, signed, written, restarted or published. A precondition that fails is
reported as the reason, e.g. `Website/ has uncommitted changes`, which is the
rule for a site release, not a dry-run limitation.

`../Tools/DeployDryRunTest.sh` runs every sub-command dry and fails if any file,
symlink, unit, port or listener changed.
