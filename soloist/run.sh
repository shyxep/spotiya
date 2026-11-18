#!/bin/sh
set -eu
base=/data/opt/solo
# ARM64 compat emulation clears the exclusive monitor inside Soloist atomics.
# Use the kernel-supported hardware barriers instead of modifying Spotify code.
if [ -f /proc/sys/abi/cp15_barrier ] && [ "$(cat /proc/sys/abi/cp15_barrier)" != 2 ]; then
    echo 2 > /proc/sys/abi/cp15_barrier || {
        echo 'Soloist needs hardware CP15 barriers on this ARM64 kernel' >&2
        exit 1
    }
fi
libs="$base/lib"
if [ "${SPOTIYA_PROFILE:-}" = mini1 ]; then
    # Station Mini 1 (YNDX-0004), see soloist/README.md "Станция Мини 1".
    # Soloist's glibc 2.38 probes time64 syscalls the 4.9 kernel lacks; do not dump registers for each.
    if [ -w /proc/sys/debug/exception-trace ]; then
        echo 0 > /proc/sys/debug/exception-trace || true
    fi
    # GLib and PCRE for apulse, which the speaker does not ship.
    libs="$base/lib:$base/mini1"
    # Shim: __ldexp/__frexp aliases, the ALSA 0.9 snd_pcm_hw_params_set_rate_near, fade-in and
    # the src_process fix for the click at the start of playback.
    export LD_PRELOAD="$base/mini1/libldexp.so"
    # The speaker's dmix path; plug:default asserts in the vendor libasound.
    export APULSE_PLAYBACK_DEVICE="${APULSE_PLAYBACK_DEVICE:-dmixer_auto}"
    # The hardware Master control does not affect this path, so apulse attenuates itself.
    export SPOTIYA_SOLOIST_HARDWARE_VOLUME=0
else
    export APULSE_PLAYBACK_DEVICE="${APULSE_PLAYBACK_DEVICE:-plug:default}"
    export SPOTIYA_SOLOIST_HARDWARE_VOLUME=1
fi
export LD_LIBRARY_PATH="$libs:/system/vendor/quasar/libs:/usr/lib:/lib"
export MALLOC_ARENA_MAX=1
exec "$base/soloist" "$@"