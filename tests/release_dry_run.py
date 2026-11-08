#!/usr/bin/env python3
"""`packaging/release.sh --dry-run`, on a throwaway clone with a throwaway key.

It checks each thing the script must refuse, then does one whole dry run and looks at what came
out: the signature, the hashes and what's in the .deb. The clone's HOME is under the scratch folder,
so the real signing key can't be reached, and a pretend `gh` on the PATH fails the test if the
dry run ever calls it. This never runs packaging/make-release-key.sh: the test key is made here.

    unshare -rn bash -c 'ip link set lo up && python3 -I tests/release_dry_run.py <scratch folder>'

Commit first, as the clone is of HEAD. $CARGO_TARGET_DIR is used if set. It needs python3-cryptography.
"""
import json
import hashlib
import os
import pathlib
import shutil
import stat
import subprocess
import sys

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey

REPO = pathlib.Path(__file__).resolve().parent.parent
failures = 0


def check(name, ok, detail=""):
    global failures
    print(f"{'PASS' if ok else 'FAIL'}: {name}" + (f" ({detail})" if detail and not ok else ""), flush=True)
    if not ok:
        failures += 1


def make_key(path):
    """The same commands packaging/make-release-key.sh uses."""
    path.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(["openssl", "genpkey", "-algorithm", "ed25519", "-out", str(path)], check=True)
    path.chmod(0o600)
    der = subprocess.run(["openssl", "pkey", "-in", str(path), "-pubout", "-outform", "DER"], check=True, capture_output=True).stdout
    return der[-32:].hex()


class Tree:
    def __init__(self, scratch):
        self.dir = scratch / "repo"
        self.home = scratch / "home"
        self.bin = scratch / "bin"
        self.gh_log = scratch / "gh-called"
        self.notes = scratch / "notes.md"
        self.target = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", scratch / "target"))
        shutil.rmtree(scratch, ignore_errors=True)
        scratch.mkdir(parents=True)
        subprocess.run(["git", "clone", "-q", "--local", str(REPO), str(self.dir)], check=True)
        subprocess.run(["git", "-C", str(self.dir), "remote", "remove", "origin"], check=True)
        # The version in Cargo.toml has usually been released already, and this is a pretend one.
        tags = subprocess.run(["git", "-C", str(self.dir), "tag"], capture_output=True, text=True, check=True).stdout.split()
        if tags:
            subprocess.run(["git", "-C", str(self.dir), "tag", "-d", *tags], check=True, capture_output=True)
        self.bin.mkdir()
        gh = self.bin / "gh"
        gh.write_text(f"#!/bin/sh\necho \"$@\" >> {self.gh_log}\nexit 1\n")
        gh.chmod(0o755)
        self.notes.write_text("What changed.\n")
        self.key = self.home / ".config/mesh-kvm-release/signing.key"
        self.public = make_key(self.key)
        self.key_file = self.dir / "packaging/release-key.pub"
        self.version = next(l.split('"')[1] for l in (self.dir / "Cargo.toml").read_text().splitlines() if l.startswith("version"))

    def git(self, *args):
        subprocess.run(["git", "-C", str(self.dir), "-c", "user.name=test", "-c", "user.email=test@example.invalid", *args], check=True, capture_output=True)

    def set_key(self, text):
        self.key_file.write_text(text + "\n")
        self.git("add", "packaging/release-key.pub")
        self.git("commit", "-q", "-m", "Set the key")

    def run(self, *args, timeout=60):
        env = {k: v for k, v in os.environ.items() if not k.startswith(("DBUS", "XDG_", "WAYLAND", "DISPLAY"))}
        env.update(HOME=str(self.home), PATH=f"{self.bin}:{os.environ['PATH']}", CARGO_TARGET_DIR=str(self.target), CARGO_NET_OFFLINE="true")
        env.setdefault("CARGO_HOME", str(pathlib.Path.home() / ".cargo"))
        env.setdefault("RUSTUP_HOME", str(pathlib.Path.home() / ".rustup"))
        return subprocess.run(["packaging/release.sh", *args], cwd=self.dir, env=env, capture_output=True, text=True, timeout=timeout)

    def dry(self, version=None, latest="0.0.1", notes=None, timeout=60):
        args = ["--dry-run", "--latest", latest, version or self.version, str(notes or self.notes)]
        return self.run(*args, timeout=timeout)


def refuses(name, result, says):
    check(name, result.returncode != 0 and says in result.stderr, f"{result.returncode}: {result.stderr[-300:]}")


def refusals(tree):
    refuses("the placeholder key is refused", tree.dry(), "placeholder")
    refuses("the placeholder is refused for a real run too", tree.run(tree.version, str(tree.notes)), "placeholder")

    other = bytes(range(32)).hex()
    tree.set_key(other)
    refuses("a private key that doesn't match the public one is refused", tree.dry(), "doesn't match")
    tree.set_key(tree.public)

    moved = tree.key.with_suffix(".moved")
    tree.key.rename(moved)
    refuses("a missing signing key is refused", tree.dry(), "missing")
    moved.rename(tree.key)

    tree.key.chmod(0o640)
    refuses("a signing key others can read is refused", tree.dry(), "readable only by its owner")
    tree.key.chmod(0o600)

    (tree.dir / "README.md").write_text("changed\n")
    refuses("uncommitted changes are refused", tree.dry(), "uncommitted")
    tree.git("checkout", "README.md")
    (tree.dir / "stray.txt").write_text("new\n")
    refuses("a stray file is refused", tree.dry(), "uncommitted")
    (tree.dir / "stray.txt").unlink()

    refuses("a version that isn't the one in Cargo.toml is refused", tree.dry(version="9.9.9"), "Cargo.toml")
    for version in ["1.2", "v1.2.3", "1.2.3-rc1", "01.2.3", "1.2.3;id"]:
        refuses(f"{version} isn't a version", tree.dry(version=version), "isn't a version")
    refuses("the latest version again is refused", tree.dry(latest=tree.version), "isn't newer")
    refuses("an older version than the latest is refused", tree.dry(latest="99.0.0"), "isn't newer")

    tree.git("tag", f"v{tree.version}", "HEAD~1")
    refuses("a tag on another commit is refused", tree.dry(), "another commit")
    tree.git("tag", "-d", f"v{tree.version}")

    empty = tree.notes.with_name("empty.md")
    empty.write_text("")
    refuses("empty release notes are refused", tree.dry(notes=empty), "notes")
    refuses("--latest can't stand in for a real run", tree.run("--latest", "0.0.1", tree.version, str(tree.notes)), "--dry-run")


def whole_dry_run(tree):
    result = tree.dry(timeout=1500)
    check("a whole dry run finishes", result.returncode == 0, result.stdout[-500:] + result.stderr[-1500:])
    if result.returncode:
        return
    version = tree.version
    deb_name = f"mesh-kvm_{version}_amd64.deb"
    dist = tree.target / f"release-{version}"
    manifest_bytes = (dist / "release.json").read_bytes()
    signature = (dist / "release.json.sig").read_bytes()
    public = Ed25519PublicKey.from_public_bytes(bytes.fromhex(tree.public))
    public.verify(signature, manifest_bytes)
    check("the signature verifies against the public key", True)
    manifest = json.loads(manifest_bytes)
    check("release.json has the version", manifest["version"] == version, manifest)
    check("release.json lists both files", set(manifest["files"]) == {deb_name, "mesh-kvm-windows-x86_64.exe"}, manifest)
    for name, entry in manifest["files"].items():
        data = (dist / name).read_bytes()
        check(f"{name} matches its size and hash", entry == {"size": len(data), "sha256": hashlib.sha256(data).hexdigest()}, entry)
    programs = {"mesh-kvm-windows-x86_64.exe": (dist / "mesh-kvm-windows-x86_64.exe").read_bytes()}
    for path in ["./usr/bin/mesh-kvm", "./usr/lib/mesh-kvm/apply-update"]:
        unpack = subprocess.Popen(["dpkg-deb", "--fsys-tarfile", str(dist / deb_name)], stdout=subprocess.PIPE)
        programs[path] = subprocess.run(["tar", "-xO", path], stdin=unpack.stdout, capture_output=True, check=True).stdout
        unpack.wait()
    for name, data in programs.items():
        check(f"{name} holds the release key", tree.public.encode() in data)
        check(f"{name} has no test hooks", b"MESH_KVM_TEST" not in data)

    listing = subprocess.run(["dpkg-deb", "-c", str(dist / deb_name)], capture_output=True, text=True, check=True).stdout
    lines = {line.split()[-1]: line.split()[0] for line in listing.splitlines()}
    check("the helper is in the .deb, executable", lines.get("./usr/lib/mesh-kvm/apply-update") == "-rwxr-xr-x", listing)
    check("the helper isn't setuid", "s" not in lines.get("./usr/lib/mesh-kvm/apply-update", "s"), listing)
    policy = "./usr/share/polkit-1/actions/io.github.benbone21.mesh-kvm.apply-update.policy"
    check("the polkit action is in the .deb", lines.get(policy) == "-rw-r--r--", listing)
    check("the polkit rule is in the .deb", lines.get("./usr/share/polkit-1/rules.d/50-mesh-kvm-update.rules") == "-rw-r--r--", listing)
    fields = subprocess.run(["dpkg-deb", "-f", str(dist / deb_name), "Version", "Depends"], capture_output=True, text=True, check=True).stdout
    check("the .deb depends on pkexec and polkit", "pkexec" in fields and "polkitd" in fields, fields)
    check("the .deb has the version", f"Version: {version}" in fields, fields)

    out = result.stdout
    steps = [out.find(step) for step in (f"git tag -a v{version}", "gh release create", f"git push origin v{version}", "gh release edit")]
    check("it tags, uploads a draft, pushes the tag and publishes, in that order", -1 not in steps and steps == sorted(steps), out[-900:])
    check("the upload is a draft", "--draft --title" in out, out[-900:])
    check("the release ends up full and latest", "--draft=false --latest --verify-tag" in out, out[-900:])
    check("all four files would be uploaded", all(n in out for n in ["release.json.sig", deb_name, "mesh-kvm-windows-x86_64.exe"]), out[-600:])
    check("the notes come from the file given", str(tree.notes) in out, out[-600:])


def built_wrongly(tree):
    """What was built is changed under the script, with `cargo` doing nothing, so that each check
    on what's about to ship is the only thing that could catch it."""
    cargo = tree.bin / "cargo"
    cargo.write_text("#!/bin/sh\nexit 0\n")
    cargo.chmod(0o755)
    exe = tree.target / "release/mesh-kvm"
    helper = tree.target / "release/apply-update"
    windows = tree.target / "x86_64-pc-windows-gnu/release/mesh-kvm.exe"
    key = tree.public.encode()
    version = tree.version.encode()

    def with_change(name, path, change, says):
        saved = path.with_name(path.name + ".saved")
        shutil.copy2(path, saved)
        path.unlink()
        path.write_bytes(change(saved.read_bytes()))
        path.chmod(0o755)
        refuses(name, tree.dry(timeout=120), says)
        saved.replace(path)

    hooked = lambda data: data + b"MESH_KVM_TEST_RELEASE_KEY"
    keyless = lambda data: data.replace(key, b"0" * len(key))
    with_change("a test-hooks Debian program in the .deb is refused", exe, hooked, "test hooks")
    with_change("a test-hooks helper in the .deb is refused", helper, hooked, "test hooks")
    with_change("a test-hooks Windows exe is refused", windows, hooked, "test hooks")
    with_change("a Debian program with no key is refused", exe, keyless, "doesn't hold the release key")
    with_change("a helper with no key is refused", helper, keyless, "doesn't hold the release key")
    with_change("a Windows exe with no key is refused", windows, keyless, "doesn't hold the release key")
    with_change("a Windows exe of another version is refused", windows, lambda d: d.replace(b"mesh-kvm/" + version, b"mesh-kvm/9.9.9"), "isn't version")

    # A stand-in that does everything the real one does but say its version.
    real = exe.with_name("mesh-kvm.real")
    shutil.copy2(exe, real)
    script = f"#!/bin/sh\n# {tree.public}\n[ \"$1\" = --version ] && {{ echo 'mesh-kvm 9.9.9'; exit 0; }}\nexec {real} \"$@\"\n".encode()
    with_change("a program that says another version is refused", exe, lambda _: script, "not version")
    real.unlink()

    dpkg_deb = tree.bin / "dpkg-deb"
    for field, value, says in [("Package", "something-else", "isn't the mesh-kvm package"), ("Version", "9.9.9", "isn't version")]:
        dpkg_deb.write_text(f"#!/bin/sh\n[ \"$1\" = -f ] && [ \"$3\" = {field} ] && {{ echo {value}; exit 0; }}\nexec /usr/bin/dpkg-deb \"$@\"\n")
        dpkg_deb.chmod(0o755)
        refuses(f"a .deb with the wrong {field} is refused", tree.dry(timeout=120), says)
    dpkg_deb.unlink()

    result = tree.dry(timeout=120)
    check("with everything back, it passes again", result.returncode == 0, result.stderr[-400:])

    # A run that failed after the tag: run again, it carries on from the tag on this commit.
    tree.git("tag", f"v{tree.version}")
    result = tree.dry(timeout=120)
    check("a tag on this commit doesn't stop a second run", result.returncode == 0, result.stderr[-400:])
    check("the second run doesn't tag again", "git tag -a" not in result.stdout and "gh release create" in result.stdout, result.stdout[-500:])
    tree.git("tag", "-d", f"v{tree.version}")
    cargo.unlink()


def nothing_escaped(tree):
    check("gh was never called", not tree.gh_log.exists())
    tags = subprocess.run(["git", "-C", str(tree.dir), "tag"], capture_output=True, text=True).stdout
    check("no tag was made", tags.strip() == "", tags)


if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    tree = Tree(pathlib.Path(sys.argv[1]).resolve())
    refusals(tree)
    whole_dry_run(tree)
    built_wrongly(tree)
    nothing_escaped(tree)
    print("ALL PASS" if failures == 0 else f"{failures} FAILED")
    sys.exit(1 if failures else 0)
