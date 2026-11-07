# mesh-kvm: updates that find and install themselves

Date: 2026-11-06
Status: awaiting review.

## Goal

Stop updating each device by hand. Every copy of the app notices a new GitHub release, downloads
it in the background, and offers one button that updates both devices.

Success means:

- A new release shows up in the app within 6 hours, already downloaded and checked.
- One click updates both devices, with no password prompt, terminal or Task Manager.
- Nothing is installed unless it's signed by the release key and newer than what's running.
- Nothing restarts until the person clicks.
- A failed update leaves a working app and says what happened.

## Decisions

- **Where releases come from:** `Benbone21/mesh-kvm` becomes public, so no login is needed. It
  switches to public when the first updater release ships, not before. The history was checked
  and holds no passwords, tokens or keys. It does show the commit email, the Linux username, the
  laptop's name and its home network address.
- **How much it does alone:** it downloads and checks by itself, then waits for a click.
- **Full releases:** releases are full releases, not pre-releases. The updater only reads the
  latest full release.

## Scope

In: checking, downloading, signature checks, installing on Windows and on the `.deb` install of
Debian, updating both devices from one click, a release script, and the app's update banner.

Out: release channels, a Mac build, updating a copy run from `target/release`, and automatic
installing. A copy that isn't the installed one shows that an update exists, with no button.

## Checking and downloading

The service does this, so it works with the app window closed.

- It reads `https://api.github.com/repos/Benbone21/mesh-kvm/releases/latest` 2 minutes after
  starting and every 6 hours after that, without a login. It also checks when the person clicks
  Check now in Settings.
- If the release is newer, it downloads `release.json`, `release.json.sig` and this platform's
  file: `mesh-kvm-windows-x86_64.exe` or `mesh-kvm_<version>_amd64.deb`.
- Downloads go to `config_dir()/updates`, resume after a dropped connection and retry with growing
  gaps, as Wi-Fi here is bad. Older downloads are deleted once a newer one is complete.
- A failed check is quiet. After 3 days without a successful check, Settings says so.

## Trust

Everything is checked against an Ed25519 public key built into the app. HTTPS isn't what's trusted,
so a compromised GitHub account alone can't push an update.

`release.json` lists the version and each file's SHA-256 and size. `release.json.sig` is its
signature. Before anything is offered or installed:

1. The signature on `release.json` must verify.
2. Its version must be newer than the running version, which blocks replaying an old signed release.
3. Its version must match the release tag.
4. The downloaded file's size and SHA-256 must match `release.json`.

The private key lives at `~/.config/mesh-kvm-release/signing.key` on Debian, outside the repository,
readable only by its owner. It must be backed up. If it's lost, a new key can only reach devices
through one more hand install.

## The update banner

Once a verified update is ready, the app shows one line at the top: "Version 0.7.0 is ready" with
Update both devices, and a link to the release notes.

- **Other device ready too:** the button updates the other device first, then this one. Whichever
  device is clicked, the other goes first, so the app on screen can show the whole thing.
- **Other device offline:** the button becomes Update this device, with a note that the two won't
  work together until both match.
- **Other device not ready:** this device waits for its download, showing how far along it is.
- **During an update:** the banner shows each step: Updating SACBOOKPRO, Waiting for it to come
  back, Updating this device. Sharing drops for a few seconds, and it says so first.
- **Not updatable:** a copy run from `target/release` shows the version only.

Status gains `update` with `current`, `available`, `ready`, `state` and `note`. Starting an update
is a new API action, allowed from the local app or the configured peer. A peer can only ask for
the already verified download to be installed, so the action names no file or version.

If the other device doesn't come back on the new version within 5 minutes, this device stays as
it is, and the banner says which device failed. The 5 minutes allows for Debian's Timeshift
snapshot.

## Installing on Windows

The exe already lives at `%LOCALAPPDATA%\mesh-kvm\mesh-kvm.exe`, so no admin prompt is needed.
`src/app/install.rs` already moves a running exe aside and copies a new one in, and this reuses it.

1. Release any mute, as a normal shutdown does.
2. Swap in the new exe, keeping the old one as `mesh-kvm.exe.old`.
3. Start the new service and app from the installed path, then exit.
4. The new copy marks itself healthy once its API answers and it has run for 30 seconds. The marker
   is deleted at that point.
5. If it isn't healthy within 60 seconds of the swap, a small watcher started in step 3 restores
   `.old`, starts it again, and leaves a note for the app.

## Installing on Debian

Installing a `.deb` needs root, and a helper that installs anything for the logged-in user would
be a way to take over the machine. So the helper trusts nothing it's handed.

- `/usr/lib/mesh-kvm/apply-update` runs as root through `pkexec`. A polkit rule in the `.deb` lets
  the active local user run that one program with no password.
- It takes no arguments except the version. It copies `release.json`, its signature and the `.deb`
  from the invoking user's `updates` folder into a root-owned folder under `/var/cache/mesh-kvm`,
  then repeats every check in [Trust](#trust) on those copies, with the key built into itself.
- It then installs through `apt-get install -y`, so the existing Timeshift hook still runs, and
  keeps the last installed `.deb` in that folder.
- Afterwards the service restarts itself through `systemctl --user restart mesh-kvm`, and the app
  and tray reopen.

If apt fails, the old version keeps running and the banner shows apt's last line. Debian has no
automatic rollback. The kept `.deb` gives a way back by hand, and the banner gives the command.

## Publishing a release

`packaging/release.sh <version>` replaces the manual steps: it checks the tree and the tests, builds
both targets and the `.deb`, writes `release.json`, signs it, tags, pushes the tag and creates a
full release with `--latest`. It refuses to run without the signing key, and it refuses a version
that isn't newer than the latest release. Release notes come from a file passed to it.

## Order of work

1. The updater ships as v0.7.0, on top of v0.6.1, on `stage-2-native-app`. The repository goes
   public at that release.
2. v0.7.0 is the last hand install, on both devices.
3. The encryption work merges onto it and ships as an update. Both devices must then be paired, so
   its release notes say so. If an update breaks the link between devices, each device can still
   update by itself from GitHub, as the button falls back to Update this device.

## Failure cases

| What happens | Result |
| --- | --- |
| No internet or GitHub down | Checks quietly again later |
| Download drops | Resumes, nothing is offered until complete |
| Signature or hash wrong | File deleted, note in Settings, not retried for that version |
| Older or equal version signed | Ignored |
| Peer offline at click | Offers Update this device |
| Peer never returns | This device stays put, banner says so |
| Windows new version won't start | Old exe restored after 60 seconds, note shown |
| apt fails | Old version keeps running, apt's message shown |
| Disk full | Note shown, partial file deleted |
| Update clicked while a screen mode trial runs | Waits for the trial to end |
| Speakers muted by Play only on the other device | Released before restart, as on any shutdown |

## Testing

- Unit tests: version comparison, signature and hash checks with good, bad, swapped and replayed
  files, download resume, banner states, the other-device-first order, and the Windows watcher.
- A local fake release server with a test key. The server address and key can only be changed in
  test builds, so a shipped copy can't be pointed elsewhere.
- A `.deb` run in a throwaway container or namespace: helper refuses tampered files, symlinks in the
  `updates` folder and a downgrade.
- `tests/wine_link.py` extended for the Windows swap and restore.
- An end-to-end test on the two real devices, from v0.7.0 to a v0.7.1 that changes nothing else,
  before the encryption release depends on it.
