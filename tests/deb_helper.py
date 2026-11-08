#!/usr/bin/env python3
"""The root helper, as an unprivileged local attacker would meet it.

It runs the real `apply-update` as root inside a throwaway user and mount namespace, with a pretend
`apt-get` and a pretend user, and tries everything a program running as that user could try:
tampered files, symlinks, a downgrade, a poisoned environment, and swapping files mid-run. Nothing
here can reach the real apt, polkit, /usr or /etc: the namespace's root is nobody outside it, and
what it writes goes to memory that vanishes with it.

Build the helper with its test hooks, as the key it checks against is this test's own:

    cargo build --features test-hooks --bin apply-update
    python3 -I tests/deb_helper.py path/to/apply-update [path/to/shipped/apply-update]

The second path is a build without the hooks, which must trust nothing here. It needs `unshare`
with subordinate ids set up, as the pretend user needs a uid of its own, and python3-cryptography.
"""
import hashlib
import json
import os
import pathlib
import resource
import shutil
import signal
import stat
import subprocess
import sys
import threading
import time

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey

USER = 1000
HOME = pathlib.Path("/home/tester")
UPDATES = HOME / ".config/mesh-kvm/updates"
CACHE = pathlib.Path("/var/cache/mesh-kvm")
RECORD = pathlib.Path("/mnt/fake/apt-record")
ANSWER = pathlib.Path("/mnt/fake/apt-answer")
RUNNING = "0.6.1"
NEW = "0.7.0"

FAKE_APT = """#!/bin/sh
# Writes down everything `apply-update` handed it, then does as it's told.
ls -l /proc/$$/fd > /mnt/fake/apt-fds
{
    echo "args=$*"
    echo "cwd=$(pwd)"
    echo "umask=$(umask)"
    echo "fsize=$(ulimit -Sf)"
    echo "stack=$(ulimit -Ss)"
    echo "session=$(cut -d ' ' -f 6 /proc/$$/stat)"
    echo "sigign=$(sed -n 's/^SigIgn:[[:space:]]*//p' /proc/$$/status)"
    echo "sigblk=$(sed -n 's/^SigBlk:[[:space:]]*//p' /proc/$$/status)"
    for arg in "$@"; do last=$arg; done
    echo "sha=$(sha256sum "$last" | cut -d ' ' -f 1)"
    echo "owner=$(stat -c '%u %a' "$last")"
    echo '---env---'
    env
} > /mnt/fake/apt-record
if [ -e /mnt/fake/apt-answer ]; then
    cat /mnt/fake/apt-answer
    exit 100
fi
"""

failures = 0
passed = 0


def check(name, ok, detail=""):
    global failures, passed
    if ok:
        passed += 1
    else:
        failures += 1
    print(f"[{'ok' if ok else 'FAIL'}] {name}{'' if ok else f' -- {detail}'}", flush=True)


class Signer:
    def __init__(self):
        self.key = Ed25519PrivateKey.generate()
        raw = self.key.public_key().public_bytes(serialization.Encoding.Raw, serialization.PublicFormat.Raw)
        self.public = raw.hex()

    def sign(self, data):
        return self.key.sign(data)


def manifest(version, package, name=None):
    name = name or f"mesh-kvm_{version}_amd64.deb"
    body = {"version": version, "files": {name: {"size": len(package), "sha256": hashlib.sha256(package).hexdigest()}}}
    return json.dumps(body).encode()


debs = {}


def deb(version, package="mesh-kvm", control_version=None):
    """A real package, small, as the helper reads its fields with dpkg-deb."""
    key = (version, package, control_version)
    if key not in debs:
        root = pathlib.Path("/mnt/fake/deb-root")
        shutil.rmtree(root, ignore_errors=True)
        (root / "DEBIAN").mkdir(parents=True)
        (root / "DEBIAN/control").write_text(
            f"Package: {package}\nVersion: {control_version or version}\nArchitecture: amd64\nMaintainer: t <t@example.invalid>\nDescription: t\n")
        built = pathlib.Path("/mnt/fake/built.deb")
        made = subprocess.run(["dpkg-deb", "--build", "--root-owner-group", str(root), str(built)], capture_output=True, text=True,
                              env=dict(os.environ, TMPDIR="/mnt/fake"))
        if made.returncode:
            sys.exit(f"dpkg-deb: {made.stderr}")
        debs[key] = built.read_bytes()
    return debs[key]


def own(path):
    os.lchown(path, USER, USER)


def write_as_user(path, data):
    path.write_bytes(data)
    own(path)


def place(signer, version=NEW, package=None, folder=None, listed=None):
    """A good download for `version`, as the service would leave it. Returns its folder."""
    package = package or deb(version)
    shutil.rmtree(UPDATES, ignore_errors=True)
    folder = UPDATES / (folder or version)
    folder.mkdir(parents=True)
    for path in [HOME / ".config", HOME / ".config/mesh-kvm", UPDATES, folder]:
        own(path)
    data = manifest(listed or version, package)
    write_as_user(folder / "release.json", data)
    write_as_user(folder / "release.json.sig", signer.sign(data))
    write_as_user(folder / f"mesh-kvm_{version}_amd64.deb", package)
    return folder


def helper_env(signer, **extra):
    env = {"PKEXEC_UID": str(USER), "MESH_KVM_TEST_RELEASE_KEY": signer.public, "MESH_KVM_TEST_VERSION": RUNNING}
    env.update(extra)
    return {k: v for k, v in env.items() if v is not None}


def run(helper, args, env, timeout=10, **kwargs):
    RECORD.unlink(missing_ok=True)
    return subprocess.run([helper, *args], env=env, capture_output=True, text=True, timeout=timeout, **kwargs)


def apt_was_called():
    return RECORD.exists()


def refused(name, result, says=""):
    check(name, result.returncode == 2 and result.stdout.startswith("refused:") and says in result.stdout and not apt_was_called(),
          f"code {result.returncode}, apt called: {apt_was_called()}, said: {result.stdout!r} {result.stderr!r}")


def installed(name, result):
    check(name, result.returncode == 0 and result.stdout.strip() == "done" and apt_was_called(),
          f"code {result.returncode}, said: {result.stdout!r} {result.stderr!r}")


def record():
    head, _, env = RECORD.read_text().partition("---env---\n")
    lines = dict(line.split("=", 1) for line in head.splitlines())
    return lines, dict(line.split("=", 1) for line in env.splitlines() if "=" in line)


def tampering(helper, signer):
    env = helper_env(signer)
    package = f"mesh-kvm_{NEW}_amd64.deb"

    place(signer)
    result = run(helper, [NEW], env)
    installed("a good release installs", result)
    lines, _ = record()
    check("apt gets the checked copy, not the user's file", f"{CACHE}/staging/{package}" in lines["args"], lines)
    check("the copy apt gets is root's and private", lines["owner"] == "0 600", lines)
    check("the copy apt gets is the good package", lines["sha"] == hashlib.sha256(deb(NEW)).hexdigest(), lines)
    check("install is non-interactive through apt-get install -y", lines["args"].startswith("install -y --no-remove"), lines)
    check("the package stays as the way back", (CACHE / "kept" / package).exists())
    check("nothing stays in staging", not (CACHE / "staging").exists())

    folder = place(signer)
    write_as_user(folder / package, deb(NEW) + b"something added")
    refused("a tampered package is refused", run(helper, [NEW], env), "doesn't match")
    check("a refused copy is thrown away", not (CACHE / "staging").exists())

    folder = place(signer)
    write_as_user(folder / "release.json", manifest(NEW, b"evil"))
    write_as_user(folder / package, b"evil")
    refused("a tampered list of files is refused", run(helper, [NEW], env), "signature")

    folder = place(signer)
    write_as_user(folder / "release.json.sig", Signer().sign((folder / "release.json").read_bytes()))
    refused("a signature from another key is refused", run(helper, [NEW], env), "signature")

    folder = place(signer)
    write_as_user(folder / "release.json.sig", b"\0" * 64)
    refused("a blank signature is refused", run(helper, [NEW], env), "signature")

    folder = place(signer, version="0.6.0")
    refused("an old signed release is refused", run(helper, ["0.6.0"], env), "isn't newer")
    folder = place(signer, version=RUNNING)
    refused("the running version is refused", run(helper, [RUNNING], env), "isn't newer")

    place(signer, package=deb(NEW, package="something-else"))
    refused("a signed package of another name is refused", run(helper, [NEW], env), "isn't mesh-kvm")
    place(signer, package=deb(NEW, control_version="0.6.0"))
    refused("a signed package that says it's another version is refused", run(helper, [NEW], env), "isn't mesh-kvm")
    place(signer, package=b"signed, but no package at all")
    refused("a signed file that isn't a package is refused", run(helper, [NEW], env), "isn't mesh-kvm")

    place(signer, version=NEW, folder=NEW, listed="0.8.0")
    refused("a signed release for another version is refused", run(helper, [NEW], env))
    folder = place(signer, version="0.6.0", folder=NEW)
    (folder / "mesh-kvm_0.6.0_amd64.deb").rename(folder / package)
    refused("an old signed release moved into the new folder is refused", run(helper, [NEW], env), "isn't newer")


def symlinks(helper, signer):
    env = helper_env(signer)
    package = f"mesh-kvm_{NEW}_amd64.deb"
    secret = pathlib.Path("/home/root-only")
    secret.write_text("the root password")
    secret.chmod(0o600)

    for name in ["release.json", "release.json.sig", package]:
        folder = place(signer)
        (folder / name).unlink()
        (folder / name).symlink_to(secret)
        own(folder / name)
        result = run(helper, [NEW], env)
        refused(f"a symlink for {name} is refused", result)
        check(f"nothing of the target leaks through {name}", "root password" not in result.stdout + result.stderr)

    for step in [HOME / ".config", HOME / ".config/mesh-kvm", UPDATES, UPDATES / NEW]:
        place(signer)
        moved = step.with_name(step.name + "-moved")
        step.rename(moved)
        step.symlink_to(moved.name)
        own(step)
        refused(f"a symlink for {step.relative_to(HOME)} is refused", run(helper, [NEW], env))
        step.unlink()
        moved.rename(step)

    folder = place(signer)
    (folder / package).unlink()
    os.link(secret, folder / package)
    refused("a hard link to a root file is refused", run(helper, [NEW], env))
    (folder / package).unlink()

    folder = place(signer)
    (folder / package).unlink()
    os.mkfifo(folder / package)
    own(folder / package)
    refused("a pipe is refused without hanging", run(helper, [NEW], env), "plain file")

    folder = place(signer)
    (folder / package).unlink()
    write_as_user(folder / package, b"")
    os.truncate(folder / package, 300 * 1024 * 1024)
    refused("a file too big is refused before it's copied", run(helper, [NEW], env), "too big")

    folder = place(signer)
    os.chown(folder / package, 0, 0)
    refused("a file that isn't the user's is refused", run(helper, [NEW], env), "belong")

    folder = place(signer)
    os.chown(folder, 0, 0)
    refused("a folder that isn't the user's is refused", run(helper, [NEW], env), "belong")

    folder = place(signer)
    secret_copy = folder / package
    secret_copy.chmod(0o000)
    os.chown(secret_copy, USER, USER)
    os.chmod(UPDATES / NEW, 0o000)
    refused("a folder the user can't read stays closed, root or not", run(helper, [NEW], env))
    os.chmod(UPDATES / NEW, 0o755)


def arguments(helper, signer):
    env = helper_env(signer)
    place(signer)
    for args in [[], [NEW, NEW], ["../../../etc/shadow"], [f"{NEW}/../{NEW}"], ["-o", "x=y"], ["--version"], [""],
                 [f"{NEW}\n"], [f" {NEW}"], ["0.07.0"], ["v0.7.0"]]:
        refused(f"the arguments {args!r} are refused", run(helper, args, env))


def users(helper, signer):
    place(signer)
    for name, uid in [("missing", None), ("empty", ""), ("words", "bread"), ("root", "0"), ("negative", "-1"),
                      ("with a space", " 1000"), ("hex", "0x3e8"), ("unknown", "4242"), ("huge", "99999999999")]:
        refused(f"PKEXEC_UID that's {name} is refused", run(helper, [NEW], helper_env(signer, PKEXEC_UID=uid)))

    def as_user():
        os.setgid(USER)
        os.setuid(USER)

    result = run(helper, [NEW], helper_env(signer), preexec_fn=as_user)
    refused("running it without root does nothing", result, "root")

    # The home comes from the list of users, whatever the environment says.
    decoy = pathlib.Path("/mnt/fake/decoy")
    shutil.copytree(HOME, decoy, symlinks=True, dirs_exist_ok=True)
    shutil.rmtree(UPDATES)
    env = helper_env(signer, HOME=str(decoy), XDG_CONFIG_HOME=str(decoy / ".config"))
    refused("HOME can't point somewhere else", run(helper, [NEW], env))


def environment(helper, signer):
    place(signer)
    poison = {
        "APT_CONFIG": "/mnt/fake/evil.conf",
        "PATH": "/mnt/fake/evil-bin",
        "DPKG_ADMINDIR": "/mnt/fake",
        "PERL5OPT": "-Mevil",
        "IFS": "x",
        "LD_LIBRARY_PATH": "/mnt/fake",
        "HOME": "/mnt/fake",
    }
    result = run(helper, [NEW], helper_env(signer, **poison))
    installed("a poisoned environment changes nothing", result)
    _, seen = record()
    check("apt sees only a clean environment", set(seen) - {"PWD", "_", "SHLVL", "OLDPWD"} == {"PATH", "DEBIAN_FRONTEND", "LC_ALL", "HOME"}, seen)
    check("apt's PATH is the system's", seen.get("PATH") == "/usr/sbin:/usr/bin:/sbin:/bin", seen)
    check("apt's HOME is root's", seen.get("HOME") == "/root", seen)

    # A planted apt-get on the caller's PATH is never run.
    evil = pathlib.Path("/mnt/fake/evil-bin")
    evil.mkdir(exist_ok=True)
    (evil / "apt-get").write_text("#!/bin/sh\ntouch /mnt/fake/evil-ran\n")
    (evil / "apt-get").chmod(0o755)
    place(signer)
    run(helper, [NEW], helper_env(signer, PATH=str(evil)))
    check("apt-get comes from a fixed path", not pathlib.Path("/mnt/fake/evil-ran").exists())

    # What the caller had set up doesn't reach apt.
    place(signer)
    read_end, write_end = os.pipe()
    os.set_inheritable(write_end, True)

    def set_up():
        os.umask(0)
        os.chdir("/mnt/fake")
        for number in (signal.SIGTERM, signal.SIGHUP, signal.SIGALRM, signal.SIGCHLD):
            signal.signal(number, signal.SIG_IGN)
        signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM, signal.SIGINT, signal.SIGALRM})
        resource.setrlimit(resource.RLIMIT_STACK, (65536, resource.getrlimit(resource.RLIMIT_STACK)[1]))

    passed_pipe = f"pipe:[{os.fstat(write_end).st_ino}]"
    result = run(helper, [NEW], helper_env(signer), pass_fds=[write_end], preexec_fn=set_up)
    os.close(read_end)
    os.close(write_end)
    installed("a caller's open file, umask and folder don't matter", result)
    lines, _ = record()
    open_files = pathlib.Path("/mnt/fake/apt-fds").read_text()
    check("no open file reaches apt", passed_pipe not in open_files, open_files)
    check("apt runs with the normal umask", lines["umask"].lstrip("0") == "22", lines)
    check("apt runs from /", lines["cwd"] == "/", lines)
    check("signals the caller ignored are back to normal", int(lines["sigign"], 16) == 0, lines)
    check("signals the caller blocked are back to normal", int(lines["sigblk"], 16) == 0, lines)
    check("a tiny stack limit doesn't reach apt", lines["stack"] == "8192", lines)
    check("apt isn't in the caller's session", int(lines["session"]) != os.getsid(0), lines)

    # The helper closes what it's given, so a closed stdout can't become something it opens.
    place(signer)
    result = subprocess.run(f"exec {helper} {NEW} >&- 2>&- <&-", shell=True, env=helper_env(signer), timeout=10)
    check("closed standard files are fine", result.returncode == 0, result)

    place(signer)
    script = f"ulimit -Sf 1; exec {helper} {NEW}"
    result = subprocess.run(script, shell=True, env=helper_env(signer), capture_output=True, text=True, timeout=10)
    lines, _ = record()
    check("a tiny file size limit doesn't reach apt", lines["fsize"] == "unlimited", lines)


def races(helper, signer):
    """A program of the user's swaps the package for another for as long as this runs."""
    env = helper_env(signer)
    package = f"mesh-kvm_{NEW}_amd64.deb"
    real = deb(NEW)
    good = hashlib.sha256(real).hexdigest()
    folder = place(signer)
    stop = threading.Event()

    def swap():
        target = folder / package
        spare = folder / "spare"
        while not stop.is_set():
            write_as_user(spare, b"evil package")
            os.replace(spare, target)
            write_as_user(spare, real)
            os.replace(spare, target)

    swapper = threading.Thread(target=swap)
    swapper.start()
    apt_saw = set()
    installs = refusals = 0
    try:
        # Both have to happen for this to have shown anything.
        for _ in range(400):
            result = run(helper, [NEW], env)
            if apt_was_called():
                installs += 1
                apt_saw.add(record()[0]["sha"])
            elif result.returncode == 2:
                refusals += 1
            if installs >= 5 and refusals >= 5:
                break
    finally:
        stop.set()
        swapper.join()
    check("swapping the package mid-run never gets anything else to apt", apt_saw <= {good}, apt_saw)
    check("the swapping really did land on both sides of a copy", installs >= 5 and refusals >= 5, f"{installs} installed, {refusals} refused")

    # Two at once, as a way to catch one half way.
    place(signer)
    with open(CACHE / "lock", "a") as lock:
        import fcntl
        fcntl.flock(lock, fcntl.LOCK_EX)
        refused("a second update waits its turn by being refused", run(helper, [NEW], env), "Another update")


def planted_cache(helper, signer):
    env = helper_env(signer)
    place(signer)
    moved = CACHE.with_name("mesh-kvm-moved")
    CACHE.rename(moved)
    CACHE.symlink_to(moved.name)
    refused("a symlink for the root folder is refused", run(helper, [NEW], env), "safe")
    CACHE.unlink()
    moved.rename(CACHE)
    os.chmod(CACHE, 0o777)
    refused("a root folder others can write to is refused", run(helper, [NEW], env), "safe")
    os.chmod(CACHE, 0o700)
    os.chown(CACHE, USER, USER)
    refused("a root folder that's the user's is refused", run(helper, [NEW], env), "safe")
    os.chown(CACHE, 0, 0)


def apt_failing(helper, signer):
    env = helper_env(signer)
    shutil.rmtree(CACHE / "kept", ignore_errors=True)
    place(signer)
    ANSWER.write_text("Reading package lists...\nE: Sub-process /usr/bin/dpkg returned an error code (1)\nW: Unsandboxed\n")
    result = run(helper, [NEW], env)
    check("apt failing comes back with its error", result.returncode == 3 and "apt: E: Sub-process" in result.stdout, result)
    check("with nothing kept there's no way back to give", "back:" not in result.stdout, result)
    check("a failed package isn't kept", not (CACHE / "kept").exists() or not list((CACHE / "kept").iterdir()))
    ANSWER.unlink()

    place(signer)
    run(helper, [NEW], env)
    place(signer, version="0.7.1")
    ANSWER.write_text("E: No.\n")
    result = run(helper, ["0.7.1"], env)
    check("apt failing gives the kept package as the way back", f"back: sudo apt-get install --reinstall --allow-downgrades {CACHE}/kept/mesh-kvm_{NEW}_amd64.deb" in result.stdout, result)
    ANSWER.unlink()

    for version in ["0.7.1", "0.7.2"]:
        place(signer, version=version)
        run(helper, [version], env)
    kept = sorted(p.name for p in (CACHE / "kept").iterdir())
    check("the last two packages are kept", kept == ["mesh-kvm_0.7.1_amd64.deb", "mesh-kvm_0.7.2_amd64.deb"], kept)


def shipped_build(shipped, signer):
    """A build with no hooks has only the real release key, and takes nothing from outside."""
    place(signer)
    result = run(shipped, [NEW], helper_env(signer))
    # The test key isn't the release key, so what it signed is turned away.
    refused("a shipped build ignores a key from the environment", result, "signature is wrong")
    data = pathlib.Path(shipped).read_bytes()
    check("a shipped build has no trace of the hooks", b"MESH_KVM_TEST" not in data)
    for word in [b"eframe", b"egui", b"winit", b"clipboard"]:
        check(f"the helper holds no {word.decode()}", word not in data)


def inside(helper, shipped):
    programs = [pathlib.Path(p).read_bytes() if p else None for p in (helper, shipped)]
    for mount in [["-t", "tmpfs", "tmpfs", "/home"], ["-t", "tmpfs", "tmpfs", "/var/cache"], ["-t", "tmpfs", "tmpfs", "/mnt"]]:
        subprocess.run(["mount", *mount], check=True)
    fake = pathlib.Path("/mnt/fake")
    fake.mkdir()
    # Copied in, as the mounts cover where they were.
    helper, shipped = (fake / "apply-update-test"), (fake / "apply-update-shipped" if shipped else None)
    for path, data in zip((helper, shipped), programs):
        if path:
            path.write_bytes(data)
            path.chmod(0o755)
    apt = fake / "apt-get"
    apt.write_text(FAKE_APT)
    apt.chmod(0o755)
    subprocess.run(["mount", "--bind", str(apt), "/usr/bin/apt-get"], check=True)
    passwd = fake / "passwd"
    passwd.write_text(f"root:x:0:0:root:/root:/bin/sh\ntester:x:{USER}:{USER}::{HOME}:/bin/sh\n")
    subprocess.run(["mount", "--bind", str(passwd), "/etc/passwd"], check=True)
    HOME.mkdir()
    own(HOME)

    signer = Signer()
    place(signer)
    run(helper, [NEW], helper_env(signer))  # Leaves the root folder made.
    for section in [tampering, symlinks, arguments, users, environment, races, planted_cache, apt_failing]:
        print(f"== {section.__name__}", flush=True)
        section(helper, signer)
    if shipped:
        print("== shipped build", flush=True)
        shipped_build(shipped, signer)
    print(f"{passed} passed, {failures} failed")
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    if os.environ.get("DEB_HELPER_INSIDE"):
        args = sys.argv[1:]
        inside(args[0], args[1] if len(args) > 1 else None)
    programs = [str(pathlib.Path(p).resolve()) for p in sys.argv[1:]]
    if not programs:
        sys.exit(__doc__)
    command = ["timeout", "-k", "5", "300", "unshare", "--user", "--map-root-user", "--map-auto", "--mount", "--pid", "--fork", "--mount-proc", "--kill-child",
               sys.executable, "-I", str(pathlib.Path(__file__).resolve()), *programs]
    sys.exit(subprocess.run(command, env=dict(os.environ, DEB_HELPER_INSIDE="1")).returncode)
