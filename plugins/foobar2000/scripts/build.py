#!/usr/bin/env python3
"""One-click Sena foobar2000 component build.

Supported host views:
  * Linux / WSL  -> all targets when the optional tools are present
                    (cargo-xwin for Windows Rust libs, MSBuild.exe from WSL
                    for the Windows plugin, clang + ld64.lld + a MacOSX SDK
                    for the macOS plugin).
  * Windows       -> Windows targets with locally installed MSVC.
  * macOS         -> macOS targets with Xcode CLT / Apple clang.

Minimum versions are enforced by `doctor`.

Every build command runs a per-scope preflight BEFORE compiling anything;
a piece whose toolchain is incomplete is skipped while the rest continues.
Runs end with a summary naming the gaps and the exact catch-up recipes
(build the missing piece, then re-run `package` to re-zip dist/ without
rebuilding). `package --scope <what>` produces a scope-named component
(e.g. foo_input_sena-<ver>-windows-x64.fb2k-component).
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

ROOT = pathlib.Path(__file__).resolve().parents[3]
PLUGIN = ROOT / "plugins" / "foobar2000" / "foo_input_sena"
SCRIPTS = PLUGIN / "scripts"
DIST = PLUGIN / "dist"
BUILD = ROOT / "build" / "foobar2000"
RUST_MIN = (1, 88)
PKG_VERSION = "0.1.0"
MACOS_SDK_URL = "https://github.com/phracker/MacOSX-SDKs/releases/download/11.3/MacOSX11.3.sdk.tar.xz"

WINDOWS_ARCHES = ("x86", "x64", "arm64ec")
MAC_ARCHES = ("arm64", "x86_64")


def host_os() -> str:
    if sys.platform == "win32":
        return "windows"
    if sys.platform == "darwin":
        return "macos"
    return "linux"


def is_wsl() -> bool:
    try:
        rel = (pathlib.Path("/proc/sys/kernel/osrelease").read_text()
               if pathlib.Path("/proc/sys/kernel/osrelease").exists() else "")
        return "microsoft" in rel.lower()
    except OSError:
        return False


def run(cmd, cwd=None, env=None, capture=False):
    print("+", " ".join(str(c) for c in cmd))
    if capture:
        return subprocess.run(cmd, cwd=cwd, env=env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return subprocess.run(cmd, cwd=cwd, env=env, check=True)


def which(name: str) -> pathlib.Path | None:
    p = shutil.which(name)
    return pathlib.Path(p) if p else None


def win_exe(name: str) -> pathlib.Path | None:
    """Locate a Windows executable from WSL or native Windows."""
    p = which(name)
    if p:
        return p
    if is_wsl():
        for base in [pathlib.Path("/mnt/c/Windows/System32/WindowsPowerShell/v1.0"),
                     pathlib.Path("/mnt/c/Windows/System32")]:
            cand = base / name
            if cand.exists():
                return cand
    return None


def win_to_lin(p: str) -> pathlib.Path | None:
    """Windows path -> WSL path (only meaningful under WSL)."""
    if not is_wsl():
        return None
    r = subprocess.run(["wslpath", "-u", p], text=True, stdout=subprocess.PIPE)
    if r.returncode == 0:
        return pathlib.Path(r.stdout.strip())
    return None


def lin_to_win(p: pathlib.Path) -> str:
    r"""WSL path -> Windows path (\wsl.localhost\Ubuntu\...)."""
    if is_wsl():
        r = subprocess.run(["wslpath", "-w", str(p)], text=True, stdout=subprocess.PIPE)
        if r.returncode == 0:
            return r.stdout.strip()
    return str(p)


def windows_temp_dir() -> pathlib.Path:
    if host_os() == "windows":
        return pathlib.Path(tempfile.gettempdir())
    ps = win_exe("powershell.exe")
    if ps is None:
        return pathlib.Path(tempfile.gettempdir())
    r = subprocess.run([str(ps), "-NoProfile", "-Command", "$env:TEMP"],
                       text=True, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    if r.returncode == 0 and r.stdout.strip():
        return win_to_lin(r.stdout.strip()) or pathlib.Path(tempfile.gettempdir())
    return pathlib.Path(tempfile.gettempdir())

def win_ensure_dir(win_path: str):
    """Create a Windows-visible directory from WSL."""
    ps = win_exe("powershell.exe")
    if ps is None:
        raise ToolError("powershell.exe not found")
    subprocess.run([str(ps), "-NoProfile", "-Command",
                    f"New-Item -ItemType Directory -Force -Path '{win_path}' | Out-Null"],
                   check=True)


def win_remove_dir(win_path: str):
    ps = win_exe("powershell.exe")
    if ps is None:
        raise ToolError("powershell.exe not found")
    subprocess.run([str(ps), "-NoProfile", "-Command",
                    f"Remove-Item -Recurse -Force -ErrorAction SilentlyContinue '{win_path}'"],
                   check=False)


def win_copy(src_win: str, dst_win: str):
    ps = win_exe("powershell.exe")
    if ps is None:
        raise ToolError("powershell.exe not found")
    subprocess.run([str(ps), "-NoProfile", "-Command",
                    f"Copy-Item -LiteralPath '{src_win}' -Destination '{dst_win}' -Force"],
                   check=True)



class ToolError(RuntimeError):
    pass


def require_cargo() -> pathlib.Path:
    cargo = which("cargo")
    if cargo is None:
        raise ToolError("cargo not found; install Rust >= 1.88 first")
    r = subprocess.run([str(cargo), "--version"], text=True, stdout=subprocess.PIPE)
    m = re.search(r"cargo (\d+)\.(\d+)", r.stdout)
    if not m:
        raise ToolError(f"cannot parse cargo version: {r.stdout!r}")
    ver = (int(m.group(1)), int(m.group(2)))
    if ver < RUST_MIN:
        raise ToolError(f"cargo {ver[0]}.{ver[1]} too old; need >= {RUST_MIN[0]}.{RUST_MIN[1]}")
    print(f"cargo: {r.stdout.strip()}")
    return pathlib.Path(cargo)


def rust_target_libdir(target: str) -> pathlib.Path | None:
    r = subprocess.run(["rustc", "--print", "target-libdir", "--target", target],
                       text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if r.returncode != 0:
        return None
    # rustc prints the path even when the std libs are missing; the real
    # check is whether the directory exists.
    p = pathlib.Path(r.stdout.strip())
    return p if p.exists() else None


#: All rust targets the plugin build needs (windows + macos).
ALL_RUST_TARGETS = [
    "i686-pc-windows-msvc",
    "x86_64-pc-windows-msvc",
    "arm64ec-pc-windows-msvc",
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
]


def missing_rust_targets() -> list[str]:
    return [t for t in ALL_RUST_TARGETS if rust_target_libdir(t) is None]


def rust_target_hint() -> str:
    """The exact rustup command installing every missing target std."""
    missing = missing_rust_targets()
    if not missing:
        return "all required rust targets are installed"
    return "rustup target add " + " ".join(missing)


def ensure_rust_target(target: str):
    if rust_target_libdir(target) is None:
        raise ToolError(
            f"Rust target '{target}' std libs are not installed.\n"
            f"  Install all required targets with:\n"
            f"      rustup target add {target}\n"
            f"  (or all at once: {rust_target_hint()})"
        )


def foobar_sdk() -> pathlib.Path:
    env = os.environ.get("FOOBAR_SDK")
    candidates = []
    if env:
        candidates.append(pathlib.Path(env))
    home = pathlib.Path.home()
    candidates += [
        home / "src" / "public" / "foobar2000-research" / "SDK-2025-03-07",
        home / "foobar2000" / "SDK",
        home / "src" / "foobar2000" / "SDK",
    ]
    for c in candidates:
        if (c / "foobar2000" / "SDK" / "input.h").exists():
            return c
    raise ToolError(
        "foobar2000 SDK not found; set FOOBAR_SDK=/path/to/SDK-2025-03-07 "
        "(must contain foobar2000/SDK/input.h)"
    )


# ---------------------------------------------------------------- preflight
# Per-scope readiness checks that run BEFORE anything is compiled, so a
# broken toolchain fails fast instead of wasting a build, and `all` can
# skip a broken leg while still completing the others. Every function
# returns a list of human-readable problems (empty = ready); the matching
# fix command is part of the message.

def _try(fn, *args) -> str | None:
    try:
        fn(*args)
        return None
    except Exception as e:
        return str(e)


def preflight_cargo() -> list[str]:
    p = _try(require_cargo)
    return [p] if p else []


def preflight_rust_target(target: str) -> list[str]:
    if rust_target_libdir(target) is not None:
        return []
    return [f"Rust target '{target}' std libs are not installed; fix: rustup target add {target}"]


def preflight_windows_shared(vs_pref: str) -> list[str]:
    """Toolchain shared by all Windows arches (rust stds are per-arch)."""
    problems = preflight_cargo()
    if p := _try(foobar_sdk):
        problems.append(p)
    if host_os() == "linux" and which("cargo-xwin") is None:
        problems.append("cargo-xwin not found (Windows Rust libs on Linux); fix: cargo install cargo-xwin")
    if host_os() in ("linux", "windows"):
        if p := _try(lambda: msbuild_exe(pick_vs(vs_pref))):
            problems.append(f"Visual Studio/MSBuild: {p}")
    return problems


def preflight_windows_arch(arch: str) -> list[str]:
    return preflight_rust_target(RUST_TARGETS[f"windows-{arch}"])


def preflight_mac_shared() -> list[str]:
    problems = preflight_cargo()
    if p := _try(foobar_sdk):
        problems.append(p)
    if p := _try(clang_for_mac):
        problems.append(p)
    if p := _try(mac_archiver):
        problems.append(p)
    if host_os() == "linux":
        # ld64.lld: build-time auto-provisioning needs apt-get + dpkg-deb.
        cached = ROOT / ".cache" / "tools" / "lld14" / "usr" / "lib" / "llvm-14" / "bin" / "ld64.lld"
        found = (os.environ.get("LD64_LLD") or which("ld64.lld")
                 or list(pathlib.Path("/usr/lib").glob("llvm-*/bin/ld64.lld"))
                 or cached.exists())
        if not found and (which("apt-get") is None or which("dpkg-deb") is None):
            problems.append("ld64.lld not found and auto-provisioning unavailable "
                            "(no apt-get/dpkg-deb); fix: apt-get install lld (or set LD64_LLD)")
    # The MacOSX SDK is downloaded on demand by mac_sdk_path(); that step
    # runs before any compilation, so it is not a preflight problem.
    return problems


def preflight_mac_arch(arch: str) -> list[str]:
    key = {"arm64": "mac-arm64", "x86_64": "mac-x64"}[arch]
    return preflight_rust_target(RUST_TARGETS[key])


def _preflight_or_raise(scope: str, problems: list[str]):
    if problems:
        raise ToolError(f"{scope} preflight failed:\n  - " + "\n  - ".join(problems))


# ---------------------------------------------------------------- report
#: Gap piece -> the just recipe that rebuilds exactly that piece.
CATCHUP_RECIPES = {
    "windows-x86": "just senadec-plugin-fb2k-windows-x86",
    "windows-x64": "just senadec-plugin-fb2k-windows-x64",
    "windows-arm64ec": "just senadec-plugin-fb2k-windows-arm64ec",
    "mac-arm64": "just senadec-plugin-fb2k-mac",
    "mac-x86_64": "just senadec-plugin-fb2k-mac",
    "mac": "just senadec-plugin-fb2k-mac",
}


class Report:
    """Collects per-piece outcomes and prints the end-of-run summary:
    what was built, what was skipped/failed (with the reason), which
    package(s) were written, and the exact catch-up recipes for the gaps."""

    def __init__(self):
        self.entries: list[list[str]] = []   # [piece, status, detail]
        self.packages: list[tuple] = []      # (path, included, missing)

    def add(self, piece: str, status: str, detail: str = ""):
        for e in self.entries:
            if e[0] == piece:
                e[1], e[2] = status, detail
                return
        self.entries.append([piece, status, detail])

    def status_of(self, piece: str) -> str | None:
        for e in self.entries:
            if e[0] == piece:
                return e[1]
        return None

    def add_package(self, path, included: list[str], missing: list[str]):
        self.packages.append((path, included, missing))

    def gaps(self) -> list[list[str]]:
        return [e for e in self.entries if e[1] in ("skipped", "failed")]

    def any_delivery(self) -> bool:
        return bool(self.packages) or any(e[1] == "ok" for e in self.entries)

    def summary(self):
        print("=" * 72)
        print("summary")
        print("=" * 72)
        for piece, status, detail in self.entries:
            mark = {"ok": "OK     ", "skipped": "SKIPPED", "failed": "FAILED "}.get(status, status)
            line = f"  {mark}  {piece}"
            if status == "ok" and detail:
                line += f"  -> {detail}"
            print(line)
            if status in ("skipped", "failed") and detail:
                for ln in str(detail).splitlines():
                    print(f"          {ln}")
        reused = []
        for path, included, missing in self.packages:
            line = f"  PACKAGE {path}"
            if missing:
                line += f"  (INCOMPLETE - missing: {', '.join(missing)})"
            print(line)
            print(f"          contains: {', '.join(included)}")
            reused += [p for p in included if self.status_of(p) != "ok"]
        if reused:
            print(f"  note: {', '.join(sorted(set(reused)))} come(s) from artifacts already "
                  f"present in dist/ (this command did not rebuild them)")
        gaps = self.gaps()
        if gaps:
            print("-" * 72)
            print("catch-up: build the missing piece(s), then re-package:")
            seen = set()
            for piece, _, _ in gaps:
                r = CATCHUP_RECIPES.get(piece)
                if r and r not in seen:
                    seen.add(r)
                    print(f"  {r}")
            print("then re-package everything already in dist/ (nothing is rebuilt):")
            print("  just senadec-plugin-fb2k-package")
        print("=" * 72)


# ---------------------------------------------------------------- Rust libs
RUST_TARGETS = {
    "windows-x86": "i686-pc-windows-msvc",
    "windows-x64": "x86_64-pc-windows-msvc",
    "windows-arm64ec": "arm64ec-pc-windows-msvc",
    "mac-arm64": "aarch64-apple-darwin",
    "mac-x64": "x86_64-apple-darwin",
}


def build_rust_lib(target: str, release: bool = True):
    ensure_rust_target(target)
    env = os.environ.copy()
    env.pop("CARGO_TARGET_DIR", None)
    cmd = ["cargo", "build", "-p", "sena-dec", "--target", target, "--lib"]
    if release:
        cmd.append("--release")
    if target.endswith("pc-windows-msvc") and host_os() == "linux":
        if which("cargo-xwin") is None:
            raise ToolError("cargo-xwin is required for Windows targets on Linux; run: cargo install cargo-xwin")
        cmd = ["cargo", "xwin", "build", "-p", "sena-dec", "--target", target, "--lib"]
        if release:
            cmd.append("--release")
        cache = ROOT / ".cache" / "cargo-xwin"
        cache.mkdir(parents=True, exist_ok=True)
        env["XWIN_CACHE_DIR"] = str(cache)
    run(cmd, env=env)
    profile = "release" if release else "debug"
    out = ROOT / "target" / target / profile / "sena_dec.lib" if target.endswith("msvc") \
        else ROOT / "target" / target / profile / "libsena_dec.a"
    if not out.exists():
        raise ToolError(f"missing expected artifact {out}")
    return out


# ------------------------------------------------- Windows toolchain discovery
def vswhere() -> pathlib.Path | None:
    candidates = [
        pathlib.Path("/mnt/c/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"),
        pathlib.Path("C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe"),
    ]
    if sys.platform == "win32":
        candidates.append(pathlib.Path(os.environ.get("ProgramFiles(x86)", "")) /
                          "Microsoft Visual Studio/Installer/vswhere.exe")
    for c in candidates:
        if c.exists():
            return c
    return None


def vs_instances() -> list[pathlib.Path]:
    vsw = vswhere()
    if vsw is None:
        raise ToolError("vswhere.exe not found")
    r = subprocess.run([str(vsw), "-all", "-products", "*",
                        "-requires", "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                        "-property", "installationPath"],
                       text=True, stdout=subprocess.PIPE)
    paths = []
    for line in r.stdout.splitlines():
        if not line.strip():
            continue
        p = pathlib.Path(line.strip())
        if is_wsl():
            lp = win_to_lin(str(p))
            if lp:
                paths.append(lp)
        else:
            paths.append(p)
    return paths


def pick_vs(preference: str) -> pathlib.Path:
    instances = vs_instances()
    if not instances:
        raise ToolError("Visual Studio with VC tools not found")
    # vswhere installationPath ends with .../18/Community or .../2022/Community.
    def vs_year(p: pathlib.Path) -> str:
        return p.parent.name

    # Prefer 2022 when both 2022 and 18 (2026) exist; otherwise use latest.
    if preference in ("auto", "2022"):
        for p in instances:
            if vs_year(p) == "2022":
                return p
        for p in instances:
            if vs_year(p) == "18":
                return p
    if preference == "2026":
        for p in instances:
            if vs_year(p) == "18":
                return p
    return instances[0]


def msbuild_exe(vs: pathlib.Path) -> pathlib.Path:
    cand = vs / "MSBuild" / "Current" / "Bin" / "MSBuild.exe"
    if cand.exists():
        return cand
    cand = vs / "MSBuild" / "Current" / "Bin" / "amd64" / "MSBuild.exe"
    if cand.exists():
        return cand
    raise ToolError(f"MSBuild.exe not found under {vs}")


def win_kits_lib() -> pathlib.Path:
    base = pathlib.Path("C:/Program Files (x86)/Windows Kits/10/Lib")
    if base.exists():
        vers = sorted((p.name for p in base.iterdir() if p.is_dir() and re.match(r"10\.", p.name)),
                      reverse=True)
        if vers:
            return base / vers[0]
    raise ToolError("Windows 10/11 SDK Lib not found")


def build_sdk_lib(vs: pathlib.Path, sdk: pathlib.Path, arch: str, out: pathlib.Path):
    msbuild = msbuild_exe(vs)
    proj = sdk / "foobar2000" / "SDK" / "foobar2000_SDK.vcxproj"
    # Windows project paths accepted by MSBuild when launched from WSL.
    proj_arg = lin_to_win(proj) if is_wsl() else str(proj)
    out_win = lin_to_win(out) if is_wsl() else str(out)
    int_win = lin_to_win(out.parent / "obj" / arch) if is_wsl() else str(out.parent / "obj" / arch)
    msbuild_arch = "Win32" if arch == "x86" else arch
    common = ["/p:Configuration=Release", f"/p:Platform={msbuild_arch}", "/p:PlatformToolset=v143",
              f"/p:OutDir={out_win}\\", f"/p:IntDir={int_win}\\",
              # VS18 (2026) on this host runs out of PCH virtual memory and
              # parallel MSBuild workers otherwise (C3859/C1076).
              "/m:1", "/p:UseMultiToolTask=false", "/p:PrecompiledHeader=NotUsing",
              "/v:minimal"]
    for rel in ["foobar2000/SDK/foobar2000_SDK.vcxproj",
                "pfc/pfc.vcxproj",
                "foobar2000/foobar2000_component_client/foobar2000_component_client.vcxproj"]:
        p = sdk / rel
        p_arg = lin_to_win(p) if is_wsl() else str(p)
        run([str(msbuild), p_arg] + common)
    for name in ["foobar2000_SDK.lib", "pfc.lib", "foobar2000_component_client.lib"]:
        if not (out / name).exists():
            raise ToolError(f"SDK build did not produce {name} in {out}")


def build_windows_plugin(arches=WINDOWS_ARCHES, vs_pref="auto", report: Report | None = None):
    """Build Windows plugin DLLs. Shared toolchain problems abort up front
    (before anything is compiled); per-arch problems skip just that arch
    and keep going with the rest."""
    report = report if report is not None else Report()
    _preflight_or_raise("windows", preflight_windows_shared(vs_pref))
    sdk = foobar_sdk()
    vs = pick_vs(vs_pref)
    print(f"Visual Studio: {vs}")
    # Keep MSBuild/cl intermediates on the Windows side (MSVC lowercases UNC
    # paths and WSL is case-sensitive); copy final DLLs back with Python.
    wtmp = windows_temp_dir() / "sena-fb2k" / "windows"
    wtmp_win = lin_to_win(wtmp) if is_wsl() else str(wtmp)
    libs_win = f"{wtmp_win}\\libs"
    for arch in arches:
        piece = f"windows-{arch}"
        problems = preflight_windows_arch(arch)
        if problems:
            report.add(piece, "skipped", "; ".join(problems))
            print(f"SKIP {piece} (preflight): {'; '.join(problems)}", file=sys.stderr)
            continue
        try:
            _build_windows_arch(arch, vs, sdk, wtmp, wtmp_win, libs_win)
            report.add(piece, "ok", str(DIST / "windows" / arch / "foo_input_sena.dll"))
            print(f"OK: windows arch {arch}")
        except Exception as e:
            report.add(piece, "failed", str(e))
            print(f"WARN: windows arch {arch} failed, skipping: {e}", file=sys.stderr)
    print("Windows plugin artifacts written to", DIST / "windows")
    return report


def _build_windows_arch(arch, vs, sdk, wtmp, wtmp_win, libs_win):
    lib_arch = "Win32" if arch == "x86" else arch
    arch_win = f"{libs_win}\\{lib_arch}"
    win_ensure_dir(arch_win)
    target = RUST_TARGETS[f"windows-{arch}"]
    rust = build_rust_lib(target)
    rust_win = lin_to_win(rust) if is_wsl() else str(rust)
    win_copy(rust_win, f"{arch_win}\\sena_dec.lib")
    sdkout = wtmp / "sdk" / arch
    sdk_names = ["foobar2000_SDK.lib", "pfc.lib", "foobar2000_component_client.lib"]
    if not all((sdkout / n).exists() for n in sdk_names):
        build_sdk_lib(vs, sdk, arch, sdkout)
    for name in sdk_names:
        src_win = lin_to_win(sdkout / name) if is_wsl() else str(sdkout / name)
        win_copy(src_win, f"{arch_win}\\{name}")
    if arch == "arm64ec":
        shared_name = "shared-ARM64EC.lib"
    elif arch == "x86":
        shared_name = "shared-Win32.lib"
    else:
        shared_name = f"shared-{arch}.lib"
    shared = sdk / "foobar2000" / "shared" / shared_name
    win_copy(lin_to_win(shared) if is_wsl() else str(shared), f"{arch_win}\\{shared_name}")

    outroot = wtmp / "out"
    proj = PLUGIN / "foo_input_sena.vcxproj"
    proj_arg = lin_to_win(proj) if is_wsl() else str(proj)
    out_win = lin_to_win(outroot / arch) if is_wsl() else str(outroot / arch)
    obj_win = lin_to_win(outroot.parent / "obj" / arch) if is_wsl() else str(outroot.parent / "obj" / arch)
    msbuild_arch = "Win32" if arch == "x86" else arch
    run([str(msbuild_exe(vs)), proj_arg,
         "/p:Configuration=Release", f"/p:Platform={msbuild_arch}",
         f"/p:FOOBAR_SDK={lin_to_win(sdk) if is_wsl() else sdk}",
         f"/p:SENA_LIB_DIR={libs_win}",
         f"/p:OutDir={out_win}\\",
         f"/p:IntDir={obj_win}\\",
         "/m:1", "/p:UseMultiToolTask=false", "/p:MultiProcessorCompilation=false",
         "/p:PrecompiledHeader=NotUsing", "/v:minimal"])
    dll = outroot / arch / "foo_input_sena.dll"
    if not dll.exists():
        raise ToolError(f"MSBuild did not produce {dll}")
    (DIST / "windows" / arch).mkdir(parents=True, exist_ok=True)
    shutil.copy2(dll, DIST / "windows" / arch / "foo_input_sena.dll")

# ---------------------------------------------------------------- macOS
def mac_sdk_path() -> pathlib.Path:
    env = os.environ.get("MACOSX_SDK")
    if env:
        p = pathlib.Path(env)
        if (p / "SDKSettings.plist").exists():
            return p
    if host_os() == "macos":
        r = subprocess.run(["xcrun", "--sdk", "macosx", "--show-sdk-path"],
                           text=True, stdout=subprocess.PIPE)
        if r.returncode == 0:
            return pathlib.Path(r.stdout.strip())
    cache = ROOT / ".cache" / "MacOSX11.3.sdk"
    if (cache / "SDKSettings.plist").exists():
        return cache
    print("downloading MacOSX11.3.sdk.tar.xz (51 MB) ...")
    cache.parent.mkdir(parents=True, exist_ok=True)
    tmp = cache.parent / "MacOSX11.3.sdk.tar.xz"
    if not tmp.exists() or tmp.stat().st_size < 10_000_000:
        urllib.request.urlretrieve(MACOS_SDK_URL, tmp)
    shutil.rmtree(cache, ignore_errors=True)
    with tarfile.open(tmp) as tf:
        tf.extractall(cache.parent)
    if not (cache / "SDKSettings.plist").exists():
        raise ToolError(f"extracted SDK is incomplete: {cache}")
    return cache


def clang_for_mac() -> pathlib.Path:
    if host_os() == "macos":
        r = subprocess.run(["xcrun", "-f", "clang++"], text=True, stdout=subprocess.PIPE)
        if r.returncode == 0:
            return pathlib.Path(r.stdout.strip())
    clang = which("clang++")
    if clang is None:
        raise ToolError("clang++ not found")
    return clang


def provision_ld64_linux() -> pathlib.Path | None:
    """Download lld-14 + libllvm14 as .deb files and extract without root."""
    if host_os() != "linux":
        return None
    cache = ROOT / ".cache" / "tools" / "lld14"
    target = cache / "usr" / "lib" / "llvm-14" / "bin" / "ld64.lld"
    if target.exists():
        return target
    for tool in ["apt-get", "dpkg-deb"]:
        if which(tool) is None:
            return None
    cache.mkdir(parents=True, exist_ok=True)
    for pkg in ["lld-14", "libllvm14"]:
        debs = list(cache.glob(f"{pkg}_*.deb")) + list(cache.glob(f"{pkg}.deb"))
        if not debs:
            run(["apt-get", "download", pkg], cwd=cache)
            debs = list(cache.glob(f"{pkg}_*.deb")) + list(cache.glob(f"{pkg}.deb"))
        if not debs:
            return None
        r = subprocess.run(["dpkg-deb", "-x", str(debs[0]), str(cache)],
                           stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        if r.returncode != 0:
            return None
    return target if target.exists() else None


def ld64_for_mac() -> pathlib.Path:
    env = os.environ.get("LD64_LLD")
    if env and pathlib.Path(env).exists():
        return pathlib.Path(env)
    for name in ["ld64.lld"]:
        p = which(name)
        if p:
            return p
    for base in pathlib.Path("/usr/lib").glob("llvm-*/bin/ld64.lld"):
        return base
    if host_os() == "linux":
        p = provision_ld64_linux()
        if p:
            print("provisioned", p)
            return p
    raise ToolError(
        "ld64.lld not found; install lld (e.g. `apt-get install lld` or "
        "`sudo apt-get install lld-14`) for Linux -> macOS linking"
    )


def mac_archiver() -> tuple[pathlib.Path, pathlib.Path]:
    ar = which("llvm-ar")
    if ar is None:
        ar = which("ar")
    ranlib = which("llvm-ranlib")
    if ranlib is None:
        ranlib = which("ranlib")
    if ar is None or ranlib is None:
        raise ToolError("llvm-ar/ar and llvm-ranlib/ranlib not found")
    return ar, ranlib


def mac_sources_from_xcode_project(proj: pathlib.Path, base: pathlib.Path) -> list[pathlib.Path]:
    text = proj.read_text(errors="ignore")
    start = text.find("Begin PBXFileReference section")
    end = text.find("End PBXFileReference section")
    sec = text[start:end]
    refmap = {}
    for m in re.finditer(r"([A-F0-9]{24})\s*/\*[^*]*\*/\s*=\s*\{isa = PBXFileReference;([^}]*)\};", sec):
        fid, body = m.groups()
        pm = re.search(r"path = ([^;]+);", body)
        if pm:
            refmap[fid] = pm.group(1).strip().strip('"')
    ids = set()
    for m in re.finditer(r"([A-F0-9]{24})\s*/\*[^*]+ in Sources \*/ = \{isa = PBXBuildFile; fileRef = ([A-F0-9]{24})", text):
        _, fid = m.groups()
        ids.add(fid)
    files = []
    for fid in ids:
        p = refmap.get(fid)
        if p and p.endswith((".cpp", ".mm", ".c")):
            files.append((base / p).resolve())
    return sorted(set(files))


def _build_mac_slice(arch, sdk, sysroot, clang, ld, ar, ranlib, tmp, triples, rust_targets, common, proj_sources):
    triple = triples[arch]
    # Rust staticlib FIRST: if this arch's Rust build is broken we fail fast
    # instead of wasting minutes compiling the SDK/plugin C++ for nothing.
    rust = build_rust_lib(rust_targets[arch])
    outroot = tmp / arch
    (outroot / "sdk-objs").mkdir(parents=True)
    (outroot / "libs").mkdir(parents=True)
    for libname, sources in proj_sources.items():
        objs = []
        for i, src in enumerate(sources):
            obj = outroot / "sdk-objs" / f"{libname}_{i:03d}.o"
            run([str(clang), "-target", triple] + common + ["-c", str(src), "-o", str(obj)])
            objs.append(obj)
        lib = outroot / "libs" / f"lib{libname}.a"
        run([str(ar), "rcs", str(lib)] + [str(o) for o in objs])
        run([str(ranlib), str(lib)])
    # plugin objects
    plug_objs = []
    for srcname in ["input_sena.cpp", "album_art_sena.cpp", "main.cpp", "dynamic_bitrate_helper.cpp"]:
        obj = outroot / "sdk-objs" / f"{srcname}.o"
        run([str(clang), "-target", triple] + common + ["-c", str(PLUGIN / srcname), "-o", str(obj)])
        plug_objs.append(obj)
    bundle = outroot / "foo_input_sena"
    link_cmd = [str(clang), "-target", triple, "-isysroot", str(sysroot),
               "-stdlib=libc++", "-fobjc-arc"]
    if ld is not None:
        link_cmd.append(f"-fuse-ld={ld}")
    link_cmd += ["-bundle", "-mmacosx-version-min=11.0",
                 "-Wl,-platform_version,macos,11.0,11.0",
                 "-o", str(bundle)]
    run(link_cmd + [str(o) for o in plug_objs] +
        [str(outroot / "libs" / f"lib{n}.a") for n in ["sdk", "pfc", "client", "shared"]] +
        [str(rust), "-framework", "Cocoa", "-framework", "CoreFoundation", "-framework", "Foundation"])
    return bundle
def build_mac_plugin(arches=MAC_ARCHES, report: Report | None = None):
    """Build the macOS component. Shared toolchain problems abort up front;
    per-arch problems skip just that slice."""
    report = report if report is not None else Report()
    _preflight_or_raise("mac", preflight_mac_shared())
    sdk = foobar_sdk()
    sysroot = mac_sdk_path()
    clang = clang_for_mac()
    if host_os() == "linux":
        ld = ld64_for_mac()
    else:
        ld = None
    ar, ranlib = mac_archiver()
    tmp = ROOT / "build" / "foobar2000" / "mac"
    shutil.rmtree(tmp, ignore_errors=True)
    (tmp / "objs").mkdir(parents=True)
    (tmp / "libs").mkdir(parents=True)

    proj_sources = {
        "sdk": mac_sources_from_xcode_project(sdk / "foobar2000" / "SDK" / "foobar2000_SDK.xcodeproj" / "project.pbxproj", sdk / "foobar2000" / "SDK"),
        "pfc": mac_sources_from_xcode_project(sdk / "pfc" / "pfc.xcodeproj" / "project.pbxproj", sdk / "pfc"),
        "client": mac_sources_from_xcode_project(sdk / "foobar2000" / "foobar2000_component_client" / "foobar2000_component_client.xcodeproj" / "project.pbxproj", sdk / "foobar2000" / "foobar2000_component_client"),
        "shared": mac_sources_from_xcode_project(sdk / "foobar2000" / "shared" / "shared.xcodeproj" / "project.pbxproj", sdk / "foobar2000" / "shared"),
    }
    triples = {"arm64": "arm64-apple-macos11", "x86_64": "x86_64-apple-macos11"}
    rust_targets = {"arm64": "aarch64-apple-darwin", "x86_64": "x86_64-apple-darwin"}
    common = ["-isysroot", str(sysroot), "-stdlib=libc++", "-std=gnu++20",
              "-fobjc-arc", "-DNDEBUG=1", "-O2",
              "-I", str(sdk), "-I", str(sdk / "foobar2000"),
              "-I", str(sdk / "foobar2000" / "helpers"),
              "-I", str(PLUGIN)]
    slices = []
    built_arches = []
    for arch in arches:
        piece = f"mac-{arch}"
        problems = preflight_mac_arch(arch)
        if problems:
            report.add(piece, "skipped", "; ".join(problems))
            print(f"SKIP {piece} (preflight): {'; '.join(problems)}", file=sys.stderr)
            continue
        try:
            bundle = _build_mac_slice(arch, sdk, sysroot, clang, ld, ar, ranlib, tmp,
                                      triples, rust_targets, common, proj_sources)
            slices.append(bundle)
            built_arches.append(arch)
            report.add(piece, "ok")
            print(f"OK: mac arch {arch}")
        except Exception as e:
            report.add(piece, "failed", str(e))
            print(f"WARN: mac arch {arch} failed, skipping: {e}", file=sys.stderr)
    if not slices:
        report.add("mac", "failed", "no slice built")
        raise ToolError("mac: no arch built (see per-slice warnings above)")
    # fat binary (Mach-O fat_arch entries are 20 bytes)
    if len(slices) > 1 and host_os() == "macos" and which("lipo"):
        out = tmp / "foo_input_sena"
        run(["lipo", "-create", "-output", str(out)] + [str(p) for p in slices])
        out = out.read_bytes()
    elif len(slices) > 1:
        data = [p.read_bytes() for p in slices]
        align = 1 << 14
        offs = []
        pos = 8 + 20 * len(slices)
        pos = (pos + align - 1) & ~(align - 1)
        hdr = bytearray()
        hdr += (0xCAFEBABE).to_bytes(4, "big")
        hdr += len(slices).to_bytes(4, "big")
        for i, d in enumerate(data):
            cpu = 0x0100000C if built_arches[i] == "arm64" else 0x01000007
            sub = 0 if built_arches[i] == "arm64" else 3
            offs.append(pos)
            hdr += cpu.to_bytes(4, "big") + sub.to_bytes(4, "big") + pos.to_bytes(4, "big") + len(d).to_bytes(4, "big") + (14).to_bytes(4, "big")
            pos = (pos + len(d) + align - 1) & ~(align - 1)
        out = hdr + bytes(offs[0] - len(hdr))
        for d, off in zip(data, offs):
            out += d
            nxt = offs[offs.index(off) + 1] if offs.index(off) + 1 < len(offs) else len(out)
            out += bytes(max(0, nxt - len(out)))
    else:
        out = slices[0].read_bytes()
    bundle_dir = DIST / "mac" / "foo_input_sena.component"
    shutil.rmtree(bundle_dir, ignore_errors=True)
    (bundle_dir / "Contents" / "MacOS").mkdir(parents=True)
    (bundle_dir / "Contents" / "MacOS" / "foo_input_sena").write_bytes(out)
    (bundle_dir / "Contents" / "PkgInfo").write_bytes(b"BNDL????")
    (bundle_dir / "Contents" / "Info.plist").write_text('''<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleDevelopmentRegion</key><string>en</string>
<key>CFBundleExecutable</key><string>foo_input_sena</string>
<key>CFBundleIdentifier</key><string>io.sena.foo_input_sena</string>
<key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
<key>CFBundleName</key><string>Sena input</string>
<key>CFBundlePackageType</key><string>BNDL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>CFBundleVersion</key><string>1</string>
<key>UTExportedTypeDeclarations</key>
<array>
  <dict>
    <key>UTTypeIdentifier</key><string>io.sena.audio</string>
    <key>UTTypeDescription</key><string>Sena audio</string>
    <key>UTTypeConformsTo</key><array><string>public.audio</string><string>public.data</string></array>
    <key>UTTypeTagSpecification</key>
    <dict>
      <key>public.filename-extension</key>
      <array><string>sena</string></array>
    </dict>
  </dict>
</array>
<key>CFBundleDocumentTypes</key>
<array>
  <dict>
    <key>CFBundleTypeName</key><string>Sena audio</string>
    <key>CFBundleTypeRole</key><string>Viewer</string>
    <key>LSHandlerRank</key><string>Owner</string>
    <key>CFBundleTypeExtensions</key>
    <array><string>sena</string><string>mka</string></array>
    <key>LSItemContentTypes</key>
    <array><string>io.sena.audio</string><string>org.matroska.mka</string></array>
  </dict>
</array>
</dict></plist>''')
    print("macOS component written to", bundle_dir)
    report.add("mac", "ok", str(bundle_dir))
    return report


# ---------------------------------------------------------------- package
#: Package scopes: which dist/ artifacts go into the .fb2k-component, and
#: how the resulting file is named. "all" keeps the canonical name; scoped
#: packages carry the scope in the file name.
PKG_SCOPES = ("all", "windows", "windows-x86", "windows-x64", "windows-arm64ec", "mac")


def make_package(scope: str = "all", report: Report | None = None):
    if scope not in PKG_SCOPES:
        raise ToolError(f"unknown package scope {scope!r}; supported: {', '.join(PKG_SCOPES)}")
    win_arches: tuple = ()
    want_mac = False
    if scope == "all":
        win_arches, want_mac = WINDOWS_ARCHES, True
    elif scope == "windows":
        win_arches = WINDOWS_ARCHES
    elif scope == "mac":
        want_mac = True
    else:  # windows-<arch>
        win_arches = (scope[len("windows-"):],)

    files: list[tuple[pathlib.Path, str]] = []
    included: list[str] = []
    missing: list[str] = []
    for arch in win_arches:
        src = DIST / "windows" / arch / "foo_input_sena.dll"
        if src.exists():
            # .fb2k-component layout: legacy x86 payload lives at the archive
            # root; x64 and arm64ec payloads live in their own directories.
            arc = "foo_input_sena.dll" if arch == "x86" else f"{arch}/foo_input_sena.dll"
            files.append((src, arc))
            included.append(f"windows-{arch}")
        else:
            missing.append(f"windows-{arch}")
    if want_mac:
        mac = DIST / "mac" / "foo_input_sena.component"
        if mac.exists():
            for f in sorted(mac.rglob("*")):
                if f.is_file():
                    files.append((f, "mac/foo_input_sena.component/" + f.relative_to(mac).as_posix()))
            included.append("mac")
        else:
            missing.append("mac")

    if scope != "all" and missing:
        hints = "; ".join(f"build it first with {CATCHUP_RECIPES.get(m, m)}" for m in missing)
        raise ToolError(f"scope {scope!r} has no built artifacts for: {', '.join(missing)}; {hints}")
    if not files:
        raise ToolError(f"nothing to package: {DIST} has no built artifacts "
                        f"(build some first, e.g. just senadec-plugin-fb2k-windows)")

    name = (f"foo_input_sena-{PKG_VERSION}.fb2k-component" if scope == "all"
            else f"foo_input_sena-{PKG_VERSION}-{scope}.fb2k-component")
    pkg = DIST / name
    pkg.unlink(missing_ok=True)
    with zipfile.ZipFile(pkg, "w", zipfile.ZIP_DEFLATED) as z:
        for src, arc in files:
            z.write(src, arc)
    print(f"package: {pkg} ({pkg.stat().st_size} bytes)")
    print(f"  contents: {', '.join(included)}")
    if missing:
        print(f"  INCOMPLETE - missing: {', '.join(missing)}", file=sys.stderr)
    if report is not None:
        report.add_package(pkg, included, missing)
    return pkg


# ---------------------------------------------------------------- install/test
def find_foobar_exe() -> pathlib.Path | None:
    env = os.environ.get("FOOBAR_EXE")
    if env:
        return pathlib.Path(env)
    if host_os() == "windows":
        for base in [pathlib.Path("C:/Program Files/foobar2000/foobar2000.exe"),
                     pathlib.Path("C:/Program Files (x86)/foobar2000/foobar2000.exe")]:
            if base.exists():
                return base
    if is_wsl():
        for win in [r"C:\Program Files\foobar2000\foobar2000.exe",
                    r"C:\Program Files (x86)\foobar2000\foobar2000.exe"]:
            p = win_to_lin(win)
            if p and p.exists():
                return p
    return None


def foobar_profile_dir() -> pathlib.Path:
    """%APPDATA%\foobar2000-v2 profile directory."""
    ps = win_exe("powershell.exe")
    if ps is None:
        raise ToolError("powershell.exe not found")
    r = subprocess.run([str(ps), "-NoProfile", "-Command",
                        "[Environment]::GetFolderPath('ApplicationData')"],
                       text=True, stdout=subprocess.PIPE)
    appdata = r.stdout.strip()
    if is_wsl():
        lin = win_to_lin(appdata)
        if lin:
            return lin / "foobar2000-v2"
    return pathlib.Path(appdata) / "foobar2000-v2"


def foobar_component_dir() -> pathlib.Path:
    """Per-arch user-components directory (x64 build uses user-components-x64)."""
    exe = find_foobar_exe()
    if exe and exe.exists():
        data = exe.read_bytes()
        pe = int.from_bytes(data[0x3C:0x40], "little")
        machine = int.from_bytes(data[pe + 4:pe + 6], "little")
        arch = "ARM64EC" if machine != 0x8664 else "x64"
    else:
        arch = "x64"
    return foobar_profile_dir() / f"user-components-{arch}"

def install_component():
    exe = find_foobar_exe()
    if exe is None:
        raise ToolError("foobar2000.exe not found; set FOOBAR_EXE")
    pkg = make_package()
    if host_os() == "windows" or is_wsl():
        appdata = os.environ.get("FOOBAR_APPDATA")
        if not appdata:
            ps = win_exe("powershell.exe")
            if ps is None:
                raise ToolError("powershell.exe not found")
            r = subprocess.run([str(ps), "-NoProfile", "-Command",
                                "[Environment]::GetFolderPath('ApplicationData')"],
                               text=True, stdout=subprocess.PIPE)
            appdata = r.stdout.strip()
        target = foobar_component_dir()
        dll = DIST / "windows" / ("arm64ec" if "ARM64EC" in target.name else "x64") / "foo_input_sena.dll"
        if is_wsl():
            ps = win_exe("powershell.exe")
            if ps:
                subprocess.run([str(ps), "-NoProfile", "-Command",
                                "Get-Process foobar2000 -ErrorAction SilentlyContinue | Stop-Process -Force"],
                               check=False)
            target_win = lin_to_win(target)
            win_ensure_dir(target_win + "\\foo_input_sena")
            win_copy(lin_to_win(dll), target_win + "\\foo_input_sena\\foo_input_sena.dll")
            print(f"installed {dll.name} to {target_win}\\foo_input_sena; restart foobar2000")
        else:
            (target / "foo_input_sena").mkdir(parents=True, exist_ok=True)
            shutil.copy2(dll, target / "foo_input_sena" / "foo_input_sena.dll")
            print(f"installed {dll.name} to {target / 'foo_input_sena'}; restart foobar2000")
        print(f"foobar2000 exe: {exe}")
    else:
        raise ToolError("automatic install only implemented on Windows/WSL")


def doctor():
    require_cargo()
    missing = missing_rust_targets()
    if missing:
        print("missing rust target std libs:", ", ".join(missing))
        print("  install with:");
        print("      rustup target add " + " ".join(missing))
    else:
        print("rust target std libs: all present")
    sdk = foobar_sdk()
    print("foobar SDK:", sdk)
    print("host:", host_os(), "WSL:", is_wsl())
    if host_os() == "linux":
        print("cargo-xwin:", which("cargo-xwin"))
        try:
            print("MacOSX SDK:", mac_sdk_path())
        except Exception as e:
            print("MacOSX SDK: unavailable:", e)
        try:
            print("ld64.lld:", ld64_for_mac())
        except Exception as e:
            print("ld64.lld: unavailable:", e)
        try:
            print("MSBuild:", msbuild_exe(pick_vs("auto")))
        except Exception as e:
            print("MSBuild: unavailable:", e)
    if host_os() == "windows":
        print("VS:", pick_vs("auto"))
    print("foobar2000:", find_foobar_exe())


def cmd_check(args) -> int:
    """Scoped preflight: report whether the requested pieces can build,
    without compiling anything. Exit 1 when any problem is found."""
    arches = args.arch or []
    win = [a for a in arches if a in WINDOWS_ARCHES]
    mac = [a for a in arches if a in MAC_ARCHES]
    if not arches:
        win, mac = list(WINDOWS_ARCHES), list(MAC_ARCHES)
    problems = 0
    if win:
        shared = preflight_windows_shared(args.vs)
        for p in shared:
            problems += 1
            print(f"[problem] windows (shared): {p}")
        if not shared:
            print("[ok] windows shared toolchain")
        for a in win:
            ps = preflight_windows_arch(a)
            if ps:
                problems += len(ps)
                for p in ps:
                    print(f"[problem] windows-{a}: {p}")
            else:
                print(f"[ok] windows-{a}")
    if mac:
        shared = preflight_mac_shared()
        for p in shared:
            problems += 1
            print(f"[problem] mac (shared): {p}")
        if not shared:
            print("[ok] mac shared toolchain")
        for a in mac:
            ps = preflight_mac_arch(a)
            if ps:
                problems += len(ps)
                for p in ps:
                    print(f"[problem] mac-{a}: {p}")
            else:
                print(f"[ok] mac-{a}")
    if not win and not mac:
        print("nothing to check for the given --arch selection")
    print(f"check: {'ready' if problems == 0 else f'{problems} problem(s)'}" )
    return 0 if problems == 0 else 1


def cmd_all(args) -> int:
    """Build every piece, tolerating per-leg and per-arch failures, then
    package whatever ended up in dist/ and finish with a summary naming the
    gaps and the recipes that fill them."""
    report = Report()
    legs = [
        ("windows", [f"windows-{a}" for a in WINDOWS_ARCHES],
         lambda: build_windows_plugin(WINDOWS_ARCHES, args.vs, report)),
        ("mac", ["mac"],
         lambda: build_mac_plugin(MAC_ARCHES, report)),
    ]
    for leg, pieces, fn in legs:
        try:
            fn()
        except Exception as e:
            # A whole leg aborted (shared preflight or zero slices): mark the
            # pieces that never got a per-piece result and keep going.
            for p in pieces:
                if report.status_of(p) is None:
                    report.add(p, "failed", str(e))
            print(f"WARN: {leg} leg aborted: {e}", file=sys.stderr)
    try:
        make_package("all", report)
    except ToolError as e:
        print(f"WARN: package failed: {e}", file=sys.stderr)
    report.summary()
    return 0 if report.any_delivery() else 1


#: `rust-libs --arch` accepts the short arch names (x64, ...) as well as the
#: full RUST_TARGETS keys (windows-x64, ...).
RUST_LIB_ALIASES = {
    "x86": "windows-x86",
    "x64": "windows-x64",
    "arm64ec": "windows-arm64ec",
    "arm64": "mac-arm64",
    "x86_64": "mac-x64",
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("command", choices=["doctor", "check", "rust-libs", "windows", "mac", "package", "install", "all"])
    ap.add_argument("--arch", action="append", choices=["x86", "x64", "arm64ec", "arm64", "x86_64"])
    ap.add_argument("--scope", choices=list(PKG_SCOPES), default=None,
                    help="package scope: which dist/ artifacts go into the .fb2k-component "
                         "(default for `package`: all)")
    ap.add_argument("--vs", choices=["auto", "2022", "2026"], default="auto")
    ap.add_argument("--debug", action="store_true")
    args = ap.parse_args()
    if args.command == "doctor":
        doctor()
    elif args.command == "check":
        sys.exit(cmd_check(args))
    elif args.command == "rust-libs":
        targets = [RUST_LIB_ALIASES.get(a, a) for a in args.arch] if args.arch else \
            ["windows-x64", "windows-arm64ec", "mac-arm64", "mac-x64"]
        problems = preflight_cargo()
        for t in targets:
            problems += preflight_rust_target(RUST_TARGETS[t])
        _preflight_or_raise("rust-libs", problems)
        for t in targets:
            build_rust_lib(RUST_TARGETS[t], release=not args.debug)
    elif args.command == "windows":
        arches = [a for a in (args.arch or ["x64", "arm64ec"]) if a in WINDOWS_ARCHES]
        report = build_windows_plugin(arches, args.vs)
        report.summary()
        if not any(report.status_of(f"windows-{a}") == "ok" for a in arches):
            sys.exit(1)
    elif args.command == "mac":
        arches = [a for a in (args.arch or ["arm64", "x86_64"]) if a in MAC_ARCHES]
        report = build_mac_plugin(arches)
        report.summary()
    elif args.command == "package":
        report = Report()
        make_package(args.scope or "all", report)
        report.summary()
    elif args.command == "install":
        install_component()
    elif args.command == "all":
        # Per-arch failures are tolerated inside each builder (they report
        # and skip), and a whole leg aborting (broken shared toolchain) only
        # skips that leg: `all` always completes what it can, packages what
        # is present in dist/ and ends with the catch-up summary.
        sys.exit(cmd_all(args))


if __name__ == "__main__":
    try:
        main()
    except ToolError as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
