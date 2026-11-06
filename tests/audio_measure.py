#!/usr/bin/env python3
"""Measures the audio path for real: a test signal through the real sender, UDP and receiver, with
`tc netem` adding loss and jitter, and reports quality, added delay and bandwidth at each bitrate.

Needs its own network namespace, as netem goes on the loopback device:

    unshare -rn bash -c 'ip link set lo up && python3 tests/audio_measure.py path/to/test-binary'

The test binary is what `cargo test --release --no-run` prints. Set AUDIO_LOSS and AUDIO_JITTER_MS to
change the conditions.
"""
import json
import os
import subprocess
import sys
import tempfile

import numpy as np

RATE = 48000
SECONDS = float(os.environ.get("AUDIO_SECONDS", "20"))
LOSS = os.environ.get("AUDIO_LOSS", "5%")
JITTER_MS = os.environ.get("AUDIO_JITTER_MS", "8")
BITRATES = [int(b) for b in os.environ.get("AUDIO_BITRATES", "64,96,128,256,0").split(",")]
TC = "/usr/sbin/tc"
BLOCK = RATE // 5  # Short enough to follow the buffer's delay as it changes.
SEGMENT = RATE // 100


def test_signal():
    """Something like music: notes with overtones that never repeat, over quiet noise, in stereo.
    A repeating signal would make the delay impossible to tell."""
    rng = np.random.default_rng(7)
    total = int(RATE * SECONDS)
    left, right = np.zeros(total), np.zeros(total)
    at = 0
    while at < total:
        length = int(RATE * rng.uniform(0.08, 0.25))
        t = np.arange(length) / RATE
        fade = np.minimum(1, np.minimum(t, t[::-1]) / 0.006)
        note = np.zeros(length)
        for _ in range(3):
            root = rng.uniform(140, 900)
            for k in range(1, 7):
                note += np.sin(2 * np.pi * root * k * t + rng.uniform(0, 6.28)) / k ** 1.3
        note *= fade * rng.uniform(0.3, 1)
        pan = rng.uniform(0.2, 0.8)
        end = min(at + length, total)
        left[at:end] += note[:end - at] * pan
        right[at:end] += note[:end - at] * (1 - pan)
        at = end
    noise = np.convolve(rng.standard_normal(total), np.ones(3) / 3, "same") * 0.01
    scale = 0.5 / max(np.abs(left).max(), np.abs(right).max())
    return np.stack([left * scale + noise, right * scale + noise], 1).astype("<f4")


def delay_of(reference, output):
    """Lag of output behind reference, by cross-correlation through an FFT."""
    size = 1 << (len(reference) + len(output)).bit_length()
    product = np.fft.rfft(output, size) * np.conj(np.fft.rfft(reference, size))
    return int(np.argmax(np.fft.irfft(product, size)[: len(output)]))


def band_levels(segment):
    """Levels in about a third of an octave at a time, in dB: what the ear goes by."""
    spectrum = np.abs(np.fft.rfft(segment * np.hanning(len(segment)))) ** 2
    return 10 * np.log10(np.array([spectrum[a:b].sum() + 1e-9 for a, b in BANDS]))


EDGES = np.unique(np.geomspace(4, SEGMENT * 2 // 2 * 0.85, 22).astype(int))
BANDS = list(zip(EDGES[:-1], EDGES[1:]))


def analyse(signal, output):
    """Follows the delay through the run, then compares each 20 ms against the original."""
    ref, out = signal[:, 0].astype(float), output[:, 0].astype(float)
    overall = delay_of(ref[RATE:RATE * 4], out[:RATE * 5])
    overall -= RATE  # The piece of the reference started a second in.
    delays, snrs, errors = [], [], []
    search = RATE // 5
    for start in range(RATE * 3, len(ref) - BLOCK - search * 2, BLOCK):
        reference = ref[start:start + BLOCK]
        low = max(start + overall - search, 0)
        window = out[low:low + 2 * search + BLOCK]
        if len(window) < 2 * search + BLOCK:
            continue
        lag = low + delay_of(reference, window) - start
        delays.append(lag)
        for at in range(start, start + BLOCK, SEGMENT * 2):
            r = ref[at:at + SEGMENT * 2]
            o = out[at + lag:at + lag + SEGMENT * 2]
            if len(o) == len(r) and np.abs(r).max() > 1e-3:
                snrs.append(10 * np.log10(np.sum(r ** 2) / (np.sum((r - o) ** 2) + 1e-12) + 1e-12))
                errors.append(np.mean(np.abs(band_levels(r) - band_levels(o))))
    snrs = np.clip(np.array(snrs), -10, 60)
    errors = np.array(errors)
    delays = np.array(delays) * 1000 / RATE
    return {
        "delay_median_ms": float(np.median(delays)),
        "delay_p95_ms": float(np.percentile(delays, 95)),
        "snr_mean_db": float(np.mean(snrs)),
        "spectrum_error_db": float(np.mean(errors)),
        "spectrum_error_p99_db": float(np.percentile(errors, 99)),
        "bad_pct": float(np.mean(errors > 6) * 100),
    }


def run(binary, kbps, work, signal):
    input_file = os.path.join(work, "in.raw")
    output_file = os.path.join(work, "out.raw")
    signal.tofile(input_file)
    environment = dict(os.environ, MK_INPUT_FILE=input_file, MK_OUTPUT_FILE=output_file, MK_KBPS=str(kbps),
                       HOME=work)
    done = subprocess.run([binary, "--ignored", "--exact", "audio::live::packet_path", "--nocapture"],
                          env=environment, capture_output=True, text=True, timeout=SECONDS + 60)
    lines = [line for line in done.stdout.splitlines() if "MEASURED " in line]
    if not lines:
        print(done.stdout, done.stderr)
        raise SystemExit("the test didn't report")
    stats = json.loads(lines[0].split("MEASURED ", 1)[1])
    output = np.fromfile(output_file, "<f4").reshape(-1, 2)
    stats.update(analyse(signal, output))
    packets = stats["tx_packets"]
    stats["kbit_s"] = (stats["tx_bytes"] + packets * 28) * 8 / SECONDS / 1000
    return stats


def main():
    binary = sys.argv[1]
    network = ["netem", "delay", "5ms", f"{JITTER_MS}ms", "distribution", "normal", "loss", LOSS]
    if os.environ.get("AUDIO_CLEAN"):
        network = None
    if network:
        subprocess.run([TC, "qdisc", "replace", "dev", "lo", "root", *network], check=True)
    print(f"loss {LOSS}, delay 5 ms +/- {JITTER_MS} ms" if network else "clean link", flush=True)
    print("kbps     wire kbit/s  delay ms (p95)  buffered ms  SNR dB  spectrum err dB (p99)  bad  repaired  made up  late  trims")
    signal = test_signal()
    with tempfile.TemporaryDirectory(prefix="mk-audio-") as work:
        for kbps in BITRATES:
            s = run(binary, kbps, work, signal)
            due = max(s["received"] + s["concealed"], 1)
            print(f"{kbps or 'PCM':<8} {s['kbit_s']:>10.0f}  {s['delay_median_ms']:>6.1f} ({s['delay_p95_ms']:>5.1f})"
                  f"  {s['buffered_mean_ms']:>10.1f}  {s['snr_mean_db']:>6.1f}"
                  f"  {s['spectrum_error_db']:>10.2f} ({s['spectrum_error_p99_db']:>5.1f})  {s['bad_pct']:>4.1f}%"
                  f"  {s['repaired'] * 100 / due:>7.2f}%  {s['concealed'] * 100 / due:>6.2f}%  {s['late']:>4}"
                  f"  {s['trimmed']:>5}", flush=True)


if __name__ == "__main__":
    main()
