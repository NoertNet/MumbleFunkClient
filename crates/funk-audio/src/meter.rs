//! Tiny spectrum meter for the equalizer display: 16 log-spaced bands over the speech range.
//!
//! A 1024-point FFT (own radix-2, no dependency) gives the power per bin; each band sums the
//! power of the bins inside it, so tones anywhere in the band and broadband speech both register.

use std::f32::consts::TAU;

pub const BANDS: usize = 16;
const FFT_N: usize = 1024;
const LOW_HZ: f32 = 160.0;
const HIGH_HZ: f32 = 5600.0;
/// Band level (dBFS of an equivalent sine) mapped to 0 and 255. Speech bands sit around -55..-30 dB.
const FLOOR_DB: f32 = -78.0;
const CEIL_DB: f32 = -24.0;

pub struct Meter {
    /// Bin ranges `[lo, hi)` per band.
    ranges: [(usize, usize); BANDS],
    twiddle: Vec<(f32, f32)>,
}

impl Meter {
    pub fn new(sample_rate: u32) -> Self {
        let bin_hz = sample_rate as f32 / FFT_N as f32;
        let ratio = (HIGH_HZ / LOW_HZ).powf(1.0 / BANDS as f32);
        let mut ranges = [(0, 0); BANDS];
        for (i, r) in ranges.iter_mut().enumerate() {
            let lo = ((LOW_HZ * ratio.powi(i as i32)) / bin_hz).ceil() as usize;
            let hi = ((LOW_HZ * ratio.powi(i as i32 + 1)) / bin_hz).ceil() as usize;
            *r = (lo, hi.max(lo + 1));
        }
        let twiddle = (0..FFT_N / 2).map(|k| (TAU * k as f32 / FFT_N as f32).cos_sin()).collect();
        Self { ranges, twiddle }
    }

    /// Band levels 0..=255 for a block of mono i16 samples (Hann windowed; the last 1024 samples
    /// are used if the block is longer, shorter blocks are zero padded).
    pub fn analyze(&self, pcm: &[i16]) -> [u8; BANDS] {
        let mut out = [0u8; BANDS];
        let block = &pcm[pcm.len().saturating_sub(FFT_N)..];
        let n = block.len();
        if n < 64 || block.iter().all(|&s| s.unsigned_abs() < 8) {
            return out; // silence is the common case while idle; skip the work
        }
        let mut re = [0.0f32; FFT_N];
        let mut im = [0.0f32; FFT_N];
        let mut window_energy = 0.0f32;
        for (i, &x) in block.iter().enumerate() {
            let w = 0.5 - 0.5 * (TAU * i as f32 / (n as f32 - 1.0)).cos();
            re[i] = x as f32 / 32768.0 * w;
            window_energy += w * w;
        }
        self.fft(&mut re, &mut im);
        for (band, &(lo, hi)) in self.ranges.iter().enumerate() {
            let power: f32 = (lo..hi.min(FFT_N / 2)).map(|k| re[k] * re[k] + im[k] * im[k]).sum();
            // Parseval: a sine of amplitude A leaves N*A^2/4*sum(w^2) in the positive half spectrum.
            let amp = 2.0 * (power / (FFT_N as f32 * window_energy)).sqrt();
            let db = 20.0 * (amp + 1e-9).log10();
            out[band] = (((db - FLOOR_DB) / (CEIL_DB - FLOOR_DB)).clamp(0.0, 1.0) * 255.0) as u8;
        }
        out
    }

    /// In-place iterative radix-2 FFT.
    fn fft(&self, re: &mut [f32; FFT_N], im: &mut [f32; FFT_N]) {
        let bits = FFT_N.trailing_zeros();
        for i in 0..FFT_N {
            let j = i.reverse_bits() >> (usize::BITS - bits);
            if j > i {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= FFT_N {
            let step = FFT_N / len;
            for start in (0..FFT_N).step_by(len) {
                for k in 0..len / 2 {
                    let (c, s) = self.twiddle[k * step];
                    let (a, b) = (start + k, start + k + len / 2);
                    let (tr, ti) = (re[b] * c + im[b] * s, im[b] * c - re[b] * s);
                    re[b] = re[a] - tr;
                    im[b] = im[a] - ti;
                    re[a] += tr;
                    im[a] += ti;
                }
            }
            len *= 2;
        }
    }
}

trait CosSin {
    fn cos_sin(self) -> (f32, f32);
}
impl CosSin for f32 {
    fn cos_sin(self) -> (f32, f32) {
        (self.cos(), self.sin())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amp: f32, n: usize) -> Vec<i16> {
        (0..n).map(|i| ((i as f32 * hz * TAU / 48_000.0).sin() * amp * 32767.0) as i16).collect()
    }

    fn band_of(m: &Meter, hz: f32) -> usize {
        let bin = (hz / (48_000.0 / FFT_N as f32)).round() as usize;
        m.ranges.iter().position(|&(lo, hi)| bin >= lo && bin < hi).expect("tone outside the meter range")
    }

    #[test]
    fn silence_is_flat() {
        let m = Meter::new(48_000);
        assert_eq!(m.analyze(&vec![0i16; 960]), [0u8; BANDS]);
        assert_eq!(m.analyze(&[1, -2, 3]), [0u8; BANDS], "too short");
    }

    #[test]
    fn tone_peaks_in_its_own_band_wherever_it_sits() {
        let m = Meter::new(48_000);
        for hz in [220.0f32, 300.0, 700.0, 1000.0, 1850.0, 3000.0, 4300.0, 5200.0] {
            let b = m.analyze(&sine(hz, 0.2, 960));
            let top = b.iter().enumerate().max_by_key(|(_, v)| **v).unwrap().0;
            let want = band_of(&m, hz);
            assert!(top.abs_diff(want) <= 1, "{hz} Hz: expected band {want}, got {top}: {b:?}");
            assert!(b[top] > 150, "peak too low for {hz} Hz: {b:?}");
        }
    }

    #[test]
    fn louder_means_higher() {
        let m = Meter::new(48_000);
        let quiet = m.analyze(&sine(1000.0, 0.01, 960)).into_iter().max().unwrap();
        let loud = m.analyze(&sine(1000.0, 0.3, 960)).into_iter().max().unwrap();
        assert!(loud > quiet + 60, "quiet {quiet}, loud {loud}");
    }

    #[test]
    fn broadband_noise_lights_most_bands() {
        let m = Meter::new(48_000);
        let mut seed = 12345u32;
        let noise: Vec<i16> = (0..960)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                ((seed >> 16) as i16) / 6
            })
            .collect();
        let lit = m.analyze(&noise).iter().filter(|&&v| v > 60).count();
        assert!(lit >= 12, "only {lit} of {BANDS} bands lit");
    }

    #[test]
    fn works_with_other_block_lengths() {
        let m = Meter::new(48_000);
        for n in [480usize, 960, 1920, 100] {
            let b = m.analyze(&sine(1000.0, 0.2, n));
            assert!(b.iter().any(|&v| v > 100), "len {n}: {b:?}");
        }
    }
}
