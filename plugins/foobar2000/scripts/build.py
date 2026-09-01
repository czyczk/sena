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
    return pathlib.Path(r.stdout.strip())


def ensure_rust_target(target: str):
    if rust_target_libdir(target) is None:
        raise ToolError(
            f"Rust target '{target}' std is not installed; run: rustup target add {target}"
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


def build_windows_plugin(arches=WINDOWS_ARCHES, vs_pref="auto"):
    """Build Windows plugin DLLs; per-arch failures are reported and skipped
    (keep going with the arches that can build; e.g. missing target std
    libs for one arch must not abort the rest)."""
    require_cargo()
    sdk = foobar_sdk()
    vs = pick_vs(vs_pref)
    print(f"Visual Studio: {vs}")
    # Keep MSBuild/cl intermediates on the Windows side (MSVC lowercases UNC
    # paths and WSL is case-sensitive); copy final DLLs back with Python.
    wtmp = windows_temp_dir() / "sena-fb2k" / "windows"
    wtmp_win = lin_to_win(wtmp) if is_wsl() else str(wtmp)
    libs_win = f"{wtmp_win}\\libs"
    failures = []
    built = []
    for arch in arches:
        try:
            _build_windows_arch(arch, vs, sdk, wtmp, wtmp_win, libs_win)
            built.append(arch)
            print(f"OK: windows arch {arch}")
        except Exception as e:
            failures.append((arch, str(e)))
            print(f"WARN: windows arch {arch} failed, skipping: {e}", file=sys.stderr)
    print("Windows plugin artifacts written to", DIST / "windows")
    _report_failures("windows", failures, built)


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


def _report_failures(what, failures, built=None):
    if not failures:
        return
    for arch, err in failures:
        print(f"SKIPPED ({what}): {arch} -> {err}", file=sys.stderr)
    print(f"built {what} arches: {built or 'none'}; skipped: {[a for a, _ in failures]}", file=sys.stderr)

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
    for srcname in ["input_sena.cpp", "main.cpp", "dynamic_bitrate_helper.cpp"]:
        obj = outroot / "sdk-objs" / f"{srcname}.o"
        run([str(clang), "-target", triple] + common + ["-c", str(PLUGIN / srcname), "-o", str(obj)])
        plug_objs.append(obj)
    rust = build_rust_lib(rust_targets[arch])
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
def build_mac_plugin(arches=MAC_ARCHES):
    require_cargo()
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
    failures = []
    common = ["-isysroot", str(sysroot), "-stdlib=libc++", "-std=gnu++20",
              "-fobjc-arc", "-DNDEBUG=1", "-O2",
              "-I", str(sdk), "-I", str(sdk / "foobar2000"),
              "-I", str(sdk / "foobar2000" / "helpers"),
              "-I", str(PLUGIN)]
    slices = []
    for arch in arches:
        try:
            bundle = _build_mac_slice(arch, sdk, sysroot, clang, ld, ar, ranlib, tmp,
                                      triples, rust_targets, common, proj_sources)
            slices.append(bundle)
            print(f"OK: mac arch {arch}")
        except Exception as e:
            failures.append((arch, str(e)))
            print(f"WARN: mac arch {arch} failed, skipping: {e}", file=sys.stderr)
    if not slices:
        raise ToolError(f"mac: no arch built; failures: {failures}")
    # (per-arch WARN lines were already printed above)
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
            cpu = 0x0100000C if arches[i] == "arm64" else 0x01000007
            sub = 0 if arches[i] == "arm64" else 3
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
</dict></plist>''')
    print("macOS component written to", bundle_dir)


# ---------------------------------------------------------------- package
def make_package():
    pkg = DIST / "foo_input_sena-0.1.0.fb2k-component"
    pkg.unlink(missing_ok=True)
    with zipfile.ZipFile(pkg, "w", zipfile.ZIP_DEFLATED) as z:
        for arch in WINDOWS_ARCHES:
            src = DIST / "windows" / arch / "foo_input_sena.dll"
            if not src.exists():
                continue
            # .fb2k-component layout: legacy x86 payload lives at the archive
            # root; x64 and arm64ec payloads live in their own directories.
            arc = "foo_input_sena.dll" if arch == "x86" else f"{arch}/foo_input_sena.dll"
            z.write(src, arc)
        mac = DIST / "mac" / "foo_input_sena.component"
        if mac.exists():
            for f in sorted(mac.rglob("*")):
                if f.is_file():
                    z.write(f, "mac/foo_input_sena.component/" + f.relative_to(mac).as_posix())
    print("package:", pkg, pkg.stat().st_size, "bytes")
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


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("command", choices=["doctor", "rust-libs", "windows", "mac", "package", "install", "all"])
    ap.add_argument("--arch", action="append", choices=["x86", "x64", "arm64ec", "arm64", "x86_64"])
    ap.add_argument("--vs", choices=["auto", "2022", "2026"], default="auto")
    ap.add_argument("--debug", action="store_true")
    args = ap.parse_args()
    if args.command == "doctor":
        doctor()
    elif args.command == "rust-libs":
        targets = args.arch or ["windows-x64", "windows-arm64ec", "mac-arm64", "mac-x64"]
        for t in targets:
            build_rust_lib(RUST_TARGETS[t], release=not args.debug)
    elif args.command == "windows":
        arches = [a for a in (args.arch or ["x64", "arm64ec"]) if a in WINDOWS_ARCHES]
        build_windows_plugin(arches, args.vs)
    elif args.command == "mac":
        arches = [a for a in (args.arch or ["arm64", "x86_64"]) if a in MAC_ARCHES]
        build_mac_plugin(arches)
    elif args.command == "package":
        make_package()
    elif args.command == "install":
        install_component()
    elif args.command == "all":
        # Per-arch failures are tolerated inside each builder (they report
        # and skip), so `all` keeps building what it can and still packages.
        build_windows_plugin(WINDOWS_ARCHES, args.vs)
        build_mac_plugin(MAC_ARCHES)
        make_package()


if __name__ == "__main__":
    main()
