// Fade-in after silence for interleaved float audio, shared by the shim and its test.
// Soloist ramps sound in itself after a pause, but its first start of a session begins
// mid-waveform: a step from silence that the speaker plays as a click.
#include <math.h>

#define FADE_SILENCE_MS 100  // silence this long ends a sound; the next sound fades in
#ifndef FADE_MS
#define FADE_MS 15  // build with -DFADE_MS=N to try another length
#endif
#ifndef FADE_DELAY_MS
#define FADE_DELAY_MS 0  // diagnostic: silence the start of each sound this long before the fade
#endif

struct fade {
    long silent_run;  // frames of digital silence so far
    long pos;         // frames into the current fade, -1 when none
};
#define FADE_INIT {1L << 30, -1}

// Applies the fade in place; returns the number of fades started in this chunk.
static int fade_process(struct fade *st, float *s, unsigned long frames, unsigned int ch,
                        unsigned int rate) {
    long len = (long)rate * FADE_MS / 1000, delay = (long)rate * FADE_DELAY_MS / 1000;
    int started = 0;
    for (unsigned long f = 0; f < frames; f++) {
        float *frame = s + f * ch;
        if (st->pos < 0) {
            float peak = 0;
            for (unsigned int c = 0; c < ch; c++)
                if (fabsf(frame[c]) > peak) peak = fabsf(frame[c]);
            if (peak < 1e-6f) {
                st->silent_run++;
                continue;
            }
            if (st->silent_run >= (long)rate * FADE_SILENCE_MS / 1000) {
                st->pos = 0;
                started++;
            }
            st->silent_run = 0;
            if (st->pos < 0) continue;
        }
        float gain = st->pos < delay ? 0.0f : 0.5f - 0.5f * cosf((float)M_PI * (st->pos - delay) / len);
        for (unsigned int c = 0; c < ch; c++) frame[c] *= gain;
        if (++st->pos >= delay + len) st->pos = -1;
    }
    return started;
}
