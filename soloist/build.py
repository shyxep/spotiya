"""Rebuild the public compatibility package without downloading Spotify Soloist."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request
import package

RECIPE = Path(__file__).resolve().parent
PROJECT = RECIPE.parent
CACHE = Path(os.environ.get('SPOTIYA_CACHE', Path.home()/'.cache/spotiya'))
SDK = Path(os.environ.get('SPOTIYA_TOOLCHAIN', CACHE/'gcc-linaro-6.5.0-2018.12-x86_64_arm-linux-gnueabihf'))
ALSA = Path(os.environ.get('SPOTIYA_ALSA_SYSROOT', CACHE/'alsa-sysroot-glibc223'))


def main():
    downloads = CACHE/'soloist-downloads'
    downloads.mkdir(parents=True, exist_ok=True)
    sources = json.loads((RECIPE/'sources.json').read_text())
    for name, entry in sources.items():
        path = downloads/name
        if not path.exists() or hashlib.sha256(path.read_bytes()).hexdigest() != entry['sha256']:
            part = path.with_suffix('.part')
            with urllib.request.urlopen(entry['url'], timeout=60) as response, part.open('wb') as out:
                shutil.copyfileobj(response, out)
            if hashlib.sha256(part.read_bytes()).hexdigest() != entry['sha256']:
                part.unlink()
                raise ValueError(f'Checksum mismatch: {name}')
            part.replace(path)
    output = CACHE/'soloist-compat-build'
    output.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='soloist-build-', dir=CACHE) as temp:
        temp = Path(temp)
        sysroot = temp/'sysroot'
        shutil.copytree(SDK/'arm-linux-gnueabihf/libc', sysroot, symlinks=True)
        # Multiarch packages override the SDK C headers/runtime used to link apulse.
        for name in sources:
            if name.endswith('.deb'):
                members = subprocess.check_output(['ar', 't', str(downloads/name)], text=True).splitlines()
                data = next(m for m in members if m.startswith('data.tar.'))
                compressed = temp/data
                with compressed.open('wb') as out:
                    subprocess.run(['ar', 'p', str(downloads/name), data], stdout=out, check=True)
                subprocess.run(['tar', '-xf', str(compressed), '-C', str(sysroot)], check=True)
        with tarfile.open(downloads/'apulse.tar.gz') as tar:
            tar.extractall(temp, filter='data')
        apulse = temp/'apulse-5d654cecd18474b4e0d885e774bc41fcbbc9818b'
        subprocess.run(['bash', str(RECIPE/'build-apulse.sh'), str(apulse), str(SDK), str(sysroot), str(ALSA), str(output)], check=True)
        runtime = temp/'runtime'
        (runtime/'lib').mkdir(parents=True)
        libs = sysroot/'lib/arm-linux-gnueabihf'
        shutil.copyfile(libs/'ld-linux-armhf.so.3', runtime/'ld.so')
        for name in package.FILES:
            if not name.startswith('lib/'):
                continue
            library = name[4:]
            source = (output/'libpulse.so.0' if library == 'libpulse.so.0' else
                      SDK/'arm-linux-gnueabihf/libc/lib/libatomic.so.1' if library == 'libatomic.so.1' else libs/library)
            shutil.copyfile(source, runtime/name)
        # Station Mini 1 extras: GLib and PCRE that apulse needs but the speaker lacks, and a
        # preloaded shim (see soloist/shim.c). Kept out of lib/ so other stations are unaffected.
        (runtime/'mini1').mkdir()
        usr_libs = sysroot/'usr/lib/arm-linux-gnueabihf'
        shutil.copyfile(usr_libs/'libglib-2.0.so.0', runtime/'mini1/libglib-2.0.so.0')
        shutil.copyfile(libs/'libpcre.so.3', runtime/'mini1/libpcre.so.3')
        subprocess.run([str(SDK/'bin/arm-linux-gnueabihf-gcc'), '-shared', '-fPIC', '-O2', '-Wall', '-Wextra', '-DSRC_FIX',
                        '-o', str(runtime/'mini1/libldexp.so'), str(RECIPE/'shim.c'), '-lm', '-ldl'], check=True)
        for binary in [runtime/'ld.so', *(runtime/'lib').iterdir(), *(runtime/'mini1').iterdir()]:
            subprocess.run([str(SDK/'bin/arm-linux-gnueabihf-strip'), '--strip-unneeded', str(binary)], check=True)
            binary.chmod(0o755)
        licenses = runtime/'licenses'
        licenses.mkdir()
        shutil.copyfile(sysroot/'usr/share/doc/libc6/copyright', licenses/'glibc-copyright')
        shutil.copyfile(apulse/'LICENSE.MIT', licenses/'apulse-MIT')
        shutil.copyfile(sysroot/'usr/share/doc/libglib2.0-0/copyright', licenses/'glib-copyright')
        shutil.copyfile(sysroot/'usr/share/doc/libpcre3/copyright', licenses/'pcre3-copyright')
        shutil.copyfile(PROJECT/'LICENSE', licenses/'spotiya-license')
        for name in ['COPYING.RUNTIME', 'COPYING3']:
            shutil.copyfile(downloads/name, licenses/('gcc-'+name))
        (licenses/'sources.txt').write_text(
            'glibc 2.38-1ubuntu6.3: https://old-releases.ubuntu.com/ubuntu/pool/main/g/glibc/\n'
            'apulse: https://github.com/i-rinat/apulse/tree/5d654cecd18474b4e0d885e774bc41fcbbc9818b\n'
            'glib2.0 2.62.4-1~ubuntu19.10.2 (LGPL-2.1+, Station Mini 1 only): https://old-releases.ubuntu.com/ubuntu/pool/main/g/glib2.0/\n'
            'pcre3 8.39-12 (BSD, Station Mini 1 only): https://old-releases.ubuntu.com/ubuntu/pool/main/p/pcre3/\n'
            'libatomic: GCC 6.5.0, GCC Runtime Library Exception 3.1; toolchain pinned by scripts/bootstrap-arm.sh\n'
            'https://developer.arm.com/downloads/-/gnu-a/6-5-2018-12\n')
        package.package(runtime, PROJECT/'dist')
        # Keep only the reproducible compatibility runtime for local smoke tests.
        final = output/'runtime'
        if final.exists(): shutil.rmtree(final)
        shutil.copytree(runtime, final)


if __name__ == '__main__':
    main()
