// Soloist shim for Mini 1 (preloaded by /data/opt/solo/run.sh): shim.c plus a fade-in
// after silence on the audio apulse writes to ALSA (see fade.h).
// Built with -DGAP_LOG it also logs short runs of digital silence inside sound (apulse
// fills silence when Soloist is late with data), with the time since the sound started.
// Built with -DXRUN_LOG it logs ALSA recovery calls, errors and long pauses between writes.
#define _GNU_SOURCE
#include <dlfcn.h>
#include <math.h>
#include <stdio.h>
#include <string.h>
#include <sys/time.h>
#include "fade.h"
double __ldexp(double x, int e) { return ldexp(x, e); }
double __frexp(double x, int *e) { return frexp(x, e); }
typedef int (*rate_fn)(void*, void*, unsigned int*, int*);
static unsigned int rate;
int snd_pcm_hw_params_set_rate_near(void *pcm, void *p, unsigned int *val, int *dir) {
    static rate_fn real;
    if (!real) real = (rate_fn)dlvsym(RTLD_NEXT, "snd_pcm_hw_params_set_rate_near", "ALSA_0.9.0rc4");
    if (!real) real = (rate_fn)dlsym(RTLD_NEXT, "__snd_pcm_hw_params_set_rate_near");
    int r = real ? real(pcm, p, val, dir) : -38;
    if (r == 0 && val) rate = *val;
    return r;
}

// apulse writes one stream; its format, channels and rate come from the hw_params calls
// it makes before writing.
#define FORMAT_FLOAT_LE 14
static int format = -1;
static unsigned int channels;
typedef int (*fmt_fn)(void *, void *, int);
int snd_pcm_hw_params_set_format(void *pcm, void *p, int val) {
    static fmt_fn real;
    if (!real) real = (fmt_fn)dlsym(RTLD_NEXT, "snd_pcm_hw_params_set_format");
    int r = real(pcm, p, val);
    format = r == 0 ? val : -1;
    return r;
}
typedef int (*ch_fn)(void *, void *, unsigned int);
int snd_pcm_hw_params_set_channels(void *pcm, void *p, unsigned int val) {
    static ch_fn real;
    if (!real) real = (ch_fn)dlsym(RTLD_NEXT, "snd_pcm_hw_params_set_channels");
    int r = real(pcm, p, val);
    channels = r == 0 ? val : 0;
    return r;
}

#ifdef GAP_LOG
// Frames since the current sound started (-1 in silence) and the current silent run.
static long sound_frames = -1, zero_run;
static void gap_scan(const float *s, unsigned long frames) {
    for (unsigned long f = 0; f < frames; f++) {
        int silent = 1;
        for (unsigned int c = 0; c < channels; c++)
            if (s[f * channels + c] != 0.0f) silent = 0;
        if (silent) {
            zero_run++;
            if (sound_frames >= 0 && zero_run >= (long)rate * FADE_SILENCE_MS / 1000) sound_frames = -1;
            continue;
        }
        if (sound_frames < 0) {
            struct timeval tv;
            gettimeofday(&tv, NULL);
            fprintf(stderr, "%ld.%03ld [shim] sound starts\n", (long)tv.tv_sec, (long)tv.tv_usec / 1000);
            sound_frames = 0;
        } else {
            if (zero_run >= (long)rate / 1000 && sound_frames < 10L * rate)
                fprintf(stderr, "[shim] gap %.1f ms at %.3f s into the sound\n", zero_run * 1000.0 / rate,
                        (double)sound_frames / rate);
            sound_frames += zero_run;
        }
        sound_frames++;
        zero_run = 0;
    }
}
#endif

#ifdef XRUN_LOG
// Diagnostic: timestamps of ALSA recovery calls, errors and long pauses between writes,
// to see whether apulse falls behind (underruns) while Soloist starts a session.
static double now_s(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return tv.tv_sec + tv.tv_usec / 1e6;
}
#define WRAP(name, ...)                                                       \
    typedef int (*name##_fn)(void *, ##__VA_ARGS__);                          \
    static name##_fn real_##name;                                             \
    static name##_fn get_##name(void) {                                       \
        if (!real_##name) real_##name = (name##_fn)dlsym(RTLD_NEXT, #name);   \
        return real_##name;                                                   \
    }
WRAP(snd_pcm_recover, int, int)
int snd_pcm_recover(void *pcm, int err, int silent) {
    fprintf(stderr, "%.3f [shim] recover err=%d\n", now_s(), err);
    return get_snd_pcm_recover()(pcm, err, silent);
}
WRAP(snd_pcm_prepare)
int snd_pcm_prepare(void *pcm) {
    fprintf(stderr, "%.3f [shim] prepare\n", now_s());
    return get_snd_pcm_prepare()(pcm);
}
WRAP(snd_pcm_start)
int snd_pcm_start(void *pcm) {
    fprintf(stderr, "%.3f [shim] start\n", now_s());
    return get_snd_pcm_start()(pcm);
}
typedef long (*avail_fn)(void *);
long snd_pcm_avail(void *pcm) {
    static avail_fn real;
    if (!real) real = (avail_fn)dlsym(RTLD_NEXT, "snd_pcm_avail");
    long r = real(pcm);
    if (r < 0) fprintf(stderr, "%.3f [shim] avail error %ld\n", now_s(), r);
    return r;
}
#endif

#ifdef SRC_FIX
// Click at the start of every new output stream. alsa-plugins rate_samplerate.c converts a
// period with src_process() and, when libsamplerate returns fewer frames than requested (its
// filter delay on the first period after open/reset), writes them at dst + (dst_frames -
// output_frames_gen) and leaves the leading frames of dst untouched. Those frames keep old heap
// contents, which in a long-running player is old audio: a loud 0.2-0.5 ms burst = the click.
// Fix: complete the output ourselves, real frames at the end, silence in front, and report a
// full period, so the plugin writes every frame. The libsamplerate 0.1.x ABI is used as-is.
typedef struct {
    const float *data_in;
    float *data_out;
    long input_frames, output_frames;
    long input_frames_used, output_frames_gen;
    int end_of_input;
    double src_ratio;
} src_data;
typedef int (*src_process_fn)(void *, src_data *);
int src_process(void *state, src_data *data) {
    static src_process_fn real;
    static int logged;
    if (!real) {
        void *h = dlopen("libsamplerate.so.0", RTLD_NOW | RTLD_NOLOAD);
        if (h) real = (src_process_fn)dlsym(h, "src_process");
        if (!real) real = (src_process_fn)dlsym(RTLD_NEXT, "src_process");
        if (!real) return 1;  // libsamplerate: "malloc failed"; never expected here
    }
    int r = real(state, data);
    if (r == 0 && data && data->data_out && data->output_frames_gen >= 0 &&
        data->output_frames_gen < data->output_frames) {
        // Interleaved floats per frame. SRC_STATE is opaque, so use the stream's channel count
        // seen in apulse's hw_params (the rate plugin converts the same channels), else stereo.
        long ch = channels ? (long)channels : 2;
        long missing = data->output_frames - data->output_frames_gen;
        float *out = data->data_out;
        memmove(out + missing * ch, out, (size_t)data->output_frames_gen * ch * sizeof(float));
        memset(out, 0, (size_t)missing * ch * sizeof(float));
        data->output_frames_gen = data->output_frames;
        if (logged < 5) {
            logged++;
            fprintf(stderr, "[shim] src_process: padded %ld leading frames with silence\n", missing);
        }
    }
    return r;
}
#endif

typedef long (*writei_fn)(void *, const void *, unsigned long);
static long write_faded(void *pcm, const void *buf, unsigned long frames) {
    static writei_fn real;
    static struct fade st = FADE_INIT;
    static float copy[8192];  // apulse writes at most 16 KiB per call
    if (!real) real = (writei_fn)dlsym(RTLD_NEXT, "snd_pcm_writei");
    if (format != FORMAT_FLOAT_LE || channels == 0 || channels > 8 || rate < 8000 || !buf ||
        frames * channels > sizeof copy / sizeof copy[0])
        return real(pcm, buf, frames);
#ifdef GAP_LOG
    gap_scan(buf, frames);
#endif
    memcpy(copy, buf, frames * channels * sizeof(float));
    if (fade_process(&st, copy, frames, channels, rate))
        fprintf(stderr, "[shim] fade-in after silence\n");
    return real(pcm, copy, frames);
}
long snd_pcm_writei(void *pcm, const void *buf, unsigned long frames) {
#ifdef XRUN_LOG
    static double last;
    double t = now_s();
    if (last > 0 && t - last > 0.05)
        fprintf(stderr, "%.3f [shim] write pause %.0f ms\n", t, (t - last) * 1000);
    long r = write_faded(pcm, buf, frames);
    if (r < 0) fprintf(stderr, "%.3f [shim] writei error %ld\n", now_s(), r);
    last = now_s();
    return r;
#else
    return write_faded(pcm, buf, frames);
#endif
}
