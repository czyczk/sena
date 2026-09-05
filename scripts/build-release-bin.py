#!/usr/bin/env python3
"""Build release binaries for a workspace package (senaenc/senadec) for one
or more supported targets.

Targets (aliases or full Rust triples):
  windows-x86       i686-pc-windows-msvc
  windows-x64       x86_64-pc-windows-msvc
  windows-arm64     aarch64-pc-windows-msvc
  windows-arm64ec   arm64ec-pc-windows-msvc
  macos-x64         x86_64-apple-darwin
  macos-arm64       aarch64-apple-darwin
  macos-universal   both macOS slices lipo'd into one binary
  linux-x64         x86_64-unknown-linux-gnu
  linux-arm64       aarch64-unknown-linux-gnu

Windows linker selection:
  auto            WSL/Linux: real MSVC link.exe via VS if a VS instance is
                  found (prefers 2022, then 2026), else cargo-xwin.
                  Native Windows: cargo's normal MSVC discovery.
  xwin            force cargo-xwin (WSL/Linux only).
  vs              force Visual Studio MSVC link.exe (native Windows or WSL).

--vs auto|2022|2026 selects the Visual Studio instance when VS is used.

Output goes to --out (default: build/<pkg> in the repository root; /build/
is git-ignored, so the binaries never dirty the working tree). Relative
--out paths are resolved against the repo root. An explicit output path on
a read-only mount (e.g. the WSL Linux ~/temp on this box) is detected and,
when WSL interop is available, the files are delivered to the equivalent
Windows profile temp directory (normally C:\\Users\\<you>\\temp) and the
real location is printed.

Every requested target's rust-std is checked up front, before any
compilation; a target whose std is missing is skipped immediately.
Per-target build failures are then tolerated the same way: the failing
target is skipped, the rest is still built and delivered, and the final
summary names the failed targets plus the exact `just` recipe that retries
only them. The run exits non-zero only when nothing was delivered at all.
"""

import argparse
import glob
import importlib.util
import os
import platform
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PKG = "senaenc"
STAGE_KIND = "release-bin"
STAGE = ROOT / "build" / "senaenc-release"  # rebuilt per pkg in main()
# (STAGE initial value is replaced by main() with the per-pkg path)

TARGETS = {
    "windows-x86": "i686-pc-windows-msvc",
    "windows-x64": "x86_64-pc-windows-msvc",
    "windows-arm64": "aarch64-pc-windows-msvc",
    "windows-arm64ec": "arm64ec-pc-windows-msvc",
    "macos-x64": "x86_64-apple-darwin",
    "macos-arm64": "aarch64-apple-darwin",
    "linux-x64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
}
RAW_TRIPLES = set(TARGETS.values())
UNIVERSAL_ALIAS = "macos-universal"


def run(cmd, env=None, cwd=None):
    print("+", " ".join(str(c) for c in cmd))
    return subprocess.run(cmd, cwd=cwd, env=env, check=True)


def which(name):
    p = shutil.which(name)
    return Path(p) if p else None


def is_wsl():
    if sys.platform != "linux":
        return False
    return (Path("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
            or Path("/mnt/c/Windows/System32/cmd.exe").exists())


def win_exe(name):
    p = which(name)
    if p:
        return p
    if is_wsl():
        cand = Path("/mnt/c/Windows/System32") / name
        if cand.exists():
            return cand
    return None


def load_fb_build():
    spec = importlib.util.spec_from_file_location(
        "fb2k_build", ROOT / "plugins" / "foobar2000" / "scripts" / "build.py"
    )
    if spec is None or spec.loader is None:
        raise SystemExit("error: plugins/foobar2000/scripts/build.py not found")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def wslpath_w(p):
    return subprocess.check_output(["wslpath", "-w", str(p)], text=True).strip()


def cargo_env():
    env = os.environ.copy()
    env.setdefault("CARGO_HOME", str(ROOT / ".cargo-home"))
    toolchain_bin = ROOT / ".toolchains" / "stable" / "bin"
    if toolchain_bin.exists():
        env["PATH"] = str(toolchain_bin) + os.pathsep + env.get("PATH", "")
    toolchain_lib = ROOT / ".toolchains" / "stable" / "lib"
    if toolchain_lib.exists():
        env["LD_LIBRARY_PATH"] = str(toolchain_lib) + os.pathsep + env.get("LD_LIBRARY_PATH", "")
    env.pop("CARGO_TARGET_DIR", None)
    return env


def rust_std_present(triple):
    env = cargo_env()
    r = subprocess.run(
        ["rustc", "--print", "target-libdir", "--target", triple],
        env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    return r.returncode == 0 and Path(r.stdout.strip()).exists()


def ensure_rust_target(triple):
    if rust_std_present(triple):
        return
    raise SystemExit(
        f"error: Rust standard library for {triple} is not installed.\n"
        f"       Install it with: rustup target add {triple}"
    )


# ---------------------------------------------------------------- VS / MSVC
def ms_arch(triple):
    return {
        "i686-pc-windows-msvc": "x86",
        "x86_64-pc-windows-msvc": "x64",
        "aarch64-pc-windows-msvc": "arm64",
        "arm64ec-pc-windows-msvc": "arm64ec",
    }[triple]


def win_path_from_linux(p: Path) -> str:
    s = str(p)
    if s.startswith("/mnt/c"):
        return "C:" + s[len("/mnt/c"):].replace("/", "\\")
    return wslpath_w(p)


def latest_dir(pattern):
    hits = sorted(glob.glob(pattern), reverse=True)
    if not hits:
        return None
    return Path(hits[0])


def locate_msvc(vs: Path, triple: str):
    tools = latest_dir(str(vs / "VC" / "Tools" / "MSVC" / "*"))
    if tools is None:
        return None
    arch = ms_arch(triple)
    bin_host = tools / "bin" / "Hostx64"
    link_dir = "arm64" if arch == "arm64ec" else arch
    link = bin_host / link_dir / "link.exe"
    if not link.exists():
        return None
    lib_arch = "arm64ec" if arch == "arm64ec" else arch
    vs_lib = tools / "lib" / lib_arch
    # ARM64EC links against the ARM64 CRT import libraries (msvcrt.lib etc.)
    # in addition to the arm64ec object stubs (chkstk_arm64ec.obj, ...).
    extra_libs = [tools / "lib" / "arm64"] if arch == "arm64ec" else []
    kits = latest_dir("/mnt/c/Program Files (x86)/Windows Kits/10/Lib/10.*")
    kit_arch = "arm64" if arch == "arm64ec" else arch
    ucrt = kits / "ucrt" / kit_arch if kits else None
    um = kits / "um" / kit_arch if kits else None
    if not vs_lib.exists() or ucrt is None or not ucrt.exists() or um is None or not um.exists():
        return None
    return {
        "link": link,
        "libs": [vs_lib] + extra_libs + [ucrt, um],
    }


def write_msvc_wrapper(triple, link: Path, libs: list[Path]):
    STAGE.mkdir(parents=True, exist_ok=True)
    wrapper = STAGE / f"msvc-linker-{triple}.py"
    libpaths = ";".join(win_path_from_linux(p) for p in libs)
    link_win = win_path_from_linux(link)
    wrapper.write_text(
        "#!/usr/bin/env python3\n"
        "import os, subprocess, sys\n"
        "link = os.environ['SENA_MSVC_LINK']\n"
        "if link.lower().startswith('c:\\\\'):\n"
        "    link = '/mnt/c' + link[2:].replace('\\\\', '/')\n"
        "libs = os.environ['SENA_MSVC_LIBPATHS'].split(';')\n"
        "args = [link]\n"
        "for lib in libs:\n"
        "    if lib.strip():\n"
        "        args.append('/LIBPATH:' + lib)\n"
        "def conv(s):\n"
        "    for opt in ('/OUT:', '/IMPLIB:', '/LIBPATH:', '/PDB:'):\n"
        "        if s.upper().startswith(opt.upper()):\n"
        "            tail = s[len(opt):]\n"
        "            return opt + conv(tail) if tail.startswith(('/home/', '/tmp/', '/mnt/')) else s\n"
        "    if s.startswith(('/home/', '/tmp/', '/mnt/')):\n"
        "        if s.startswith('/mnt/c'):\n"
        "            return 'C:' + s[len('/mnt/c'):].replace('/', '\\\\')\n"
        "        import subprocess as sp\n"
        "        return sp.check_output(['wslpath', '-w', s], text=True).strip()\n"
        "    return s\n"
        "args += [conv(a) for a in sys.argv[1:]]\n"
        "sys.exit(subprocess.run(args).returncode)\n"
    )
    wrapper.chmod(0o755)
    return wrapper, link_win, libpaths


def vs_mode_env(env, triple, year):
    fb = load_fb_build()
    instances = fb.vs_instances()
    if not instances:
        raise SystemExit("error: no Visual Studio instance with VC tools found")
    vs = fb.pick_vs(year)
    print(f"Visual Studio: {vs}")
    msvc = locate_msvc(vs, triple)
    if msvc is None:
        raise SystemExit(
            f"error: Visual Studio at {vs} has no usable MSVC/link.exe for {triple}"
        )
    if sys.platform == "win32":
        link = msvc["link"]
        env["CARGO_TARGET_" + triple.replace("-", "_").upper() + "_LINKER"] = str(link)
        env["LIB"] = ";".join(str(p) for p in msvc["libs"])
        return env
    if not is_wsl():
        raise SystemExit(f"error: VS-mode cross linking for {triple} is only supported from Windows or WSL")
    wrapper, link_win, libpaths = write_msvc_wrapper(triple, msvc["link"], msvc["libs"])
    env["SENA_MSVC_LINK"] = link_win
    env["SENA_MSVC_LIBPATHS"] = libpaths
    env["LIB"] = libpaths
    env["CARGO_TARGET_" + triple.replace("-", "_").upper() + "_LINKER"] = str(wrapper)
    return env


def build_windows(triple, args):
    ensure_rust_target(triple)
    env = cargo_env()
    mode = args.linker
    vs_year = args.vs
    if mode == "auto":
        if sys.platform == "win32":
            mode = "cargo"
        elif is_wsl():
            fb = load_fb_build()
            have_vs = bool(fb.vs_instances())
            mode = "vs" if have_vs else "xwin"
        else:
            mode = "xwin"
    if mode == "vs":
        env = vs_mode_env(env, triple, vs_year)
        run(["cargo", "build", "-p", PKG, "--target", triple, "--release"], env=env)
    elif mode == "xwin":
        if is_wsl() or sys.platform == "linux":
            if which("cargo-xwin") is None:
                raise SystemExit("error: cargo-xwin is required for Windows cross builds (or use --linker vs)")
            cache = ROOT / ".cache" / "cargo-xwin"
            cache.mkdir(parents=True, exist_ok=True)
            env["XWIN_CACHE_DIR"] = str(cache)
            run(["cargo", "xwin", "build", "-p", PKG, "--target", triple, "--release"], env=env)
        else:
            raise SystemExit("error: cargo-xwin mode is only supported from Linux/WSL")
    elif mode == "cargo":
        run(["cargo", "build", "-p", PKG, "--target", triple, "--release"], env=env)
    else:
        raise SystemExit("error: --linker must be auto, xwin, vs or cargo")
    src = ROOT / "target" / triple / "release" / (PKG + ".exe")
    if not src.exists():
        raise SystemExit(f"error: cargo did not produce {src}")
    return src


# ---------------------------------------------------------------- macOS
def mac_link_env(triple):
    fb = load_fb_build()
    sdk = fb.mac_sdk_path()
    env = cargo_env()
    env["SDKROOT"] = str(sdk)
    arch = "x86_64" if triple.startswith("x86_64") else "aarch64"
    clang_triple = f"{arch}-apple-macos11"
    if sys.platform == "linux":
        ld = fb.ld64_for_mac()
        link_args = (
            f"-C link-arg=--target={clang_triple} "
            f"-C link-arg=--sysroot={sdk} "
            f"-C link-arg=-fuse-ld={ld} "
            "-C link-arg=-Wl,-platform_version,macos,11.0,11.0"
        )
    else:
        link_args = f"-C link-arg=--target={clang_triple} -C link-arg=--sysroot={sdk}"
    key = "CARGO_TARGET_" + triple.replace("-", "_").upper() + "_RUSTFLAGS"
    env[key] = link_args
    env["CARGO_TARGET_" + triple.replace("-", "_").upper() + "_LINKER"] = "clang"
    return env


def build_macos(triple, args):
    ensure_rust_target(triple)
    env = mac_link_env(triple)
    run(["cargo", "build", "-p", PKG, "--target", triple, "--release"], env=env)
    src = ROOT / "target" / triple / "release" / PKG
    if not src.exists():
        raise SystemExit(f"error: cargo did not produce {src}")
    stripped = STAGE / f"{PKG}-{triple}.stripped"
    stripped.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(src, stripped)
    llvm_strip = which("llvm-strip")
    if llvm_strip is not None and sys.platform != "darwin":
        subprocess.run([str(llvm_strip), str(stripped)], check=False)
    elif sys.platform == "darwin" and which("strip"):
        subprocess.run(["strip", str(stripped)], check=False)
    return stripped


def fat_macho(slices):
    data = [Path(p).read_bytes() for p in slices]
    arches = ["x86_64", "arm64"]
    align = 1 << 14
    hdr = bytearray()
    hdr += (0xCAFEBABE).to_bytes(4, "big")
    hdr += len(slices).to_bytes(4, "big")
    offs = []
    pos = 8 + 20 * len(slices)
    pos = (pos + align - 1) & ~(align - 1)
    for i, d in enumerate(data):
        cpu = 0x0100000C if arches[i] == "arm64" else 0x01000007
        sub = 0 if arches[i] == "arm64" else 3
        offs.append(pos)
        hdr += cpu.to_bytes(4, "big") + sub.to_bytes(4, "big")
        hdr += pos.to_bytes(4, "big") + len(d).to_bytes(4, "big")
        hdr += (14).to_bytes(4, "big")
        pos = (pos + len(d) + align - 1) & ~(align - 1)
    out = bytes(hdr) + bytes(offs[0] - len(hdr))
    for i, (d, off) in enumerate(zip(data, offs)):
        out += d
        nxt = offs[i + 1] if i + 1 < len(offs) else len(out)
        out += bytes(max(0, nxt - len(out)))
    return out


# ---------------------------------------------------------------- Linux
def build_linux(triple, args):
    ensure_rust_target(triple)
    env = cargo_env()
    run(["cargo", "build", "-p", PKG, "--target", triple, "--release"], env=env)
    src = ROOT / "target" / triple / "release" / PKG
    if not src.exists():
        raise SystemExit(f"error: cargo did not produce {src}")
    return src


def output_name(kind, triple):
    if kind == "windows":
        return f"{PKG}-{triple}.exe"
    return f"{PKG}-{triple}"


# ---------------------------------------------------------------- delivery
def cmd_quiet(cmd):
    # cmd.exe writes its current-directory notice in the OEM codepage (GBK on
    # this box); decode lossily. `cd /d C:\` keeps stdout clean.
    return subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, encoding="utf-8", errors="replace")


def deliver(src: Path, out: Path, name: str):
    src = Path(src)
    try:
        out.mkdir(parents=True, exist_ok=True)
        probe = out / f".write-test-{os.getpid()}"
        probe.write_text("x")
        probe.unlink()
        dst = out / name
        shutil.copy2(src, dst)
        print(f"wrote {dst}")
        return dst
    except OSError:
        pass

    if not is_wsl():
        raise SystemExit(f"error: output directory {out} is not writable")

    cmd = win_exe("cmd.exe")
    if cmd is None:
        raise SystemExit(f"error: output directory {out} is not writable and no WSL cmd.exe interop is available")
    r = cmd_quiet([str(cmd), "/d", "/c", 'cd /d C:\\ && echo %USERPROFILE%'])
    if r.returncode != 0:
        raise SystemExit("error: could not query the Windows user profile for read-only ~/temp fallback")
    profile = r.stdout.strip().splitlines()[-1].strip() if r.stdout.strip() else ""
    win_out = None
    home = Path.home()
    if str(out) == str(home / "temp"):
        win_out = f"{profile}\\temp"
    elif str(out).startswith(str(home) + os.sep):
        rel = str(out)[len(str(home)) + 1:]
        win_out = f"{profile}\\{rel.replace('/', '\\')}"
    elif str(out).startswith("/mnt/c"):
        win_out = "C:" + str(out)[len("/mnt/c"):].replace("/", "\\")
    if win_out is None:
        raise SystemExit(f"error: output directory {out} is not writable and has no Windows equivalent")
    drive = win_out[:2]
    rel = win_out[3:].lstrip("\\")
    src_unc = wslpath_w(src)
    ps = Path("/mnt/c/Windows/System32/WindowsPowerShell/v1.0/powershell.exe")
    if ps.exists():
        mk = subprocess.run(
            [str(ps), "-NoProfile", "-Command",
             f"New-Item -ItemType Directory -Force -Path '{win_out}' | Out-Null; exit $LASTEXITCODE"],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            encoding="utf-8", errors="replace", timeout=120,
        )
        if mk.returncode == 0:
            cp = subprocess.run(
                [str(ps), "-NoProfile", "-Command",
                 f"Copy-Item -LiteralPath '{src_unc}' -Destination '{win_out}\\' -Force; exit $LASTEXITCODE"],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                encoding="utf-8", errors="replace", timeout=120,
            )
            if cp.returncode == 0:
                print(f"wrote {win_out}\\{name} (Linux ~/temp is on a read-only root mount; used the Windows equivalent)")
                return f"{win_out}\\{name}"
    mk = subprocess.run(
        [str(cmd), "/d", "/c", f'cd /d {drive}\\ && if not exist "{rel}" mkdir "{rel}"'],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        encoding="utf-8", errors="replace",
    )
    if mk.returncode != 0:
        raise SystemExit(f"error: could not create {win_out} via cmd.exe")
    cp = subprocess.run(
        [str(cmd), "/d", "/c", f'cd /d {drive}\\ && copy /Y {src_unc} "{win_out}\\"'],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
        encoding="utf-8", errors="replace",
    )
    if cp.returncode != 0:
        raise SystemExit(f"error: could not copy {name} to {win_out}: {cp.stderr.strip()}")
    print(f"wrote {win_out}\\{name} (Linux ~/temp is on a read-only root mount; used the Windows equivalent)")
    return f"{win_out}\\{name}"


def verify(src: Path, triple: str | None):
    b = Path(src).read_bytes()[:4]
    if triple and triple.endswith("windows-msvc"):
        ok = b[:2] == b"MZ"
        print(f"verify {src}: PE {'ok' if ok else 'BAD'} ({triple})")
        return
    if b == b"\xca\xfe\xba\xbe":
        n = int.from_bytes(Path(src).read_bytes()[4:8], "big")
        print(f"verify {src}: Mach-O universal ({n} slices) ok")
        return
    if b in (b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xca\xfe\xba\xbe"):
        print(f"verify {src}: Mach-O thin ok")
        return
    if b[:4] == b"\x7fELF":
        print(f"verify {src}: ELF ok")
        return
    print(f"verify {src}: unknown format {b.hex()}")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--pkg", choices=["senaenc", "senadec"], default="senaenc",
                    help="workspace package to build (default: senaenc)")
    ap.add_argument("--target", action="append", help="target alias or full Rust triple (repeatable)")
    ap.add_argument("target_args", nargs="*", help="target aliases/triples as positional arguments (justfile style)")
    ap.add_argument("--out", default=None,
                    help="output directory (default: build/<pkg> in the repository; relative paths resolve against the repo root)")
    ap.add_argument("--vs", choices=["auto", "2022", "2026"], default="auto",
                    help="Visual Studio instance preference when VS linking is used")
    ap.add_argument("--linker", choices=["auto", "xwin", "vs", "cargo"], default="auto",
                    help="Windows linker mode (default: auto = best available)")
    ap.add_argument("--debug", action="store_true", help="build debug instead of release")
    args = ap.parse_args()
    global PKG, STAGE
    PKG = args.pkg
    STAGE = ROOT / "build" / f"{PKG}-release"
    if args.out is None:
        args.out = str(ROOT / "build" / PKG)

    requested = (args.target or []) + (args.target_args or [])
    if not requested:
        requested = ["windows-x64", "windows-arm64", UNIVERSAL_ALIAS]
    build_triples = []
    want_universal = False
    for t in requested:
        if t == UNIVERSAL_ALIAS:
            want_universal = True
            for m in ("macos-x64", "macos-arm64"):
                if TARGETS[m] not in build_triples:
                    build_triples.append(TARGETS[m])
            continue
        triple = TARGETS.get(t, t)
        if triple not in RAW_TRIPLES:
            ap.error(f"unsupported target {t!r}; supported: {sorted(TARGETS) + [UNIVERSAL_ALIAS]} or a full triple")
        if triple not in build_triples:
            build_triples.append(triple)

    STAGE.mkdir(parents=True, exist_ok=True)
    out = Path(os.path.expanduser(args.out))
    if not out.is_absolute():
        out = ROOT / out
    out = out.resolve()
    delivered = []
    failures = []  # (requested-name, one-line reason)
    mac_slices = []

    # Preflight: check every requested target's rust-std BEFORE compiling
    # anything, so a target whose std is missing is skipped immediately
    # instead of after the other targets have already burned build time.
    alias_of = {v: k for k, v in TARGETS.items()}
    for t in [t for t in build_triples if not rust_std_present(t)]:
        build_triples.remove(t)
        failures.append((alias_of.get(t, t),
                         f"Rust std not installed; fix: rustup target add {t}"))
        print(f"SKIP {t} (preflight): {failures[-1][1]}", file=sys.stderr)
    if not build_triples:
        print("error: no requested target can be built (all missing rust-std)", file=sys.stderr)
        sys.exit(1)

    def record_failure(name, exc):
        if isinstance(exc, SystemExit):
            msg = str(exc.code)
        elif isinstance(exc, subprocess.CalledProcessError):
            msg = f"build command failed with exit code {exc.returncode}"
        else:
            msg = str(exc)
        failures.append((name, msg.strip().splitlines()[0] if msg.strip() else "failed"))
        print(f"WARN: {name} failed, continuing with the rest: {failures[-1][1]}",
              file=sys.stderr)

    for triple in build_triples:
        name = alias_of.get(triple, triple)
        if args.debug:
            print(f"warning: debug builds are not implemented; building release", file=sys.stderr)
        try:
            if triple.endswith("pc-windows-msvc"):
                src = build_windows(triple, args)
                out_name = output_name("windows", triple)
                staged = STAGE / out_name
                shutil.copy2(src, staged)
                verify(staged, triple)
                delivered.append(deliver(staged, out, out_name))
            elif triple.endswith("apple-darwin"):
                src = build_macos(triple, args)
                mac_slices.append((triple, src))
                if not want_universal:
                    out_name = output_name("mac", triple)
                    staged = STAGE / out_name
                    shutil.copy2(src, staged)
                    verify(staged, None)
                    delivered.append(deliver(staged, out, out_name))
            elif triple.endswith("unknown-linux-gnu"):
                src = build_linux(triple, args)
                out_name = output_name("linux", triple)
                staged = STAGE / out_name
                shutil.copy2(src, staged)
                verify(staged, None)
                delivered.append(deliver(staged, out, out_name))
            else:
                ap.error(f"unsupported target {triple!r}")
        except (SystemExit, Exception) as e:
            record_failure(name, e)

    if want_universal:
        x86 = next((p for t, p in mac_slices if t.startswith("x86_64")), None)
        arm = next((p for t, p in mac_slices if t.startswith("aarch64")), None)
        if x86 is None or arm is None:
            missing = "x86_64" if x86 is None else "aarch64"
            record_failure(UNIVERSAL_ALIAS,
                           f"macos-universal needs both slices; the {missing} slice did not build")
        else:
            try:
                if sys.platform == "darwin" and which("lipo"):
                    staged = STAGE / f"{PKG}-universal-apple-darwin"
                    run(["lipo", "-create", "-output", str(staged), str(x86), str(arm)])
                else:
                    staged = STAGE / f"{PKG}-universal-apple-darwin"
                    staged.write_bytes(fat_macho([x86, arm]))
                    staged.chmod(0o755)
                verify(staged, None)
                delivered.append(deliver(staged, out, f"{PKG}-universal-apple-darwin"))
            except (SystemExit, Exception) as e:
                record_failure(UNIVERSAL_ALIAS, e)

    print("\nDone. Delivered:")
    for d in delivered:
        print(" ", d)
    if failures:
        recipe = "senaenc" if PKG == "senaenc" else "senadec-bin"
        print("\nFailed targets:", file=sys.stderr)
        for t, msg in failures:
            print(f"  {t}: {msg}", file=sys.stderr)
        retry = " ".join(t for t, _ in failures)
        print(f"\nretry just the failed targets (the rest is already built):\n"
              f"  just {recipe} \"{retry}\"", file=sys.stderr)
    if not delivered:
        sys.exit(1)


if __name__ == "__main__":
    main()
