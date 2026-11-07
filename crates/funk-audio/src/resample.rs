//! Streaming linear resampler for mono f32 audio. Good enough for narrowband
//! voice; keeps state across blocks so chunk boundaries are seamless.

pub struct Resampler {
    /// Input samples consumed per output sample (`in_rate / out_rate`).
    step: f64,
    pos: f64,
    prev: f32,
    buf: Vec<f32>,
}

impl Resampler {
    pub fn new(in_rate: u32, out_rate: u32) -> Self {
        Self { step: in_rate as f64 / out_rate as f64, pos: 0.0, prev: 0.0, buf: Vec::new() }
    }

    pub fn is_identity(&self) -> bool {
        (self.step - 1.0).abs() < f64::EPSILON
    }

    /// Append the resampled version of `input` to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.is_identity() {
            out.extend_from_slice(input);
            return;
        }
        if input.is_empty() {
            return;
        }
        // Index 0 is the last sample of the previous block.
        self.buf.clear();
        self.buf.push(self.prev);
        self.buf.extend_from_slice(input);
        let last = self.buf.len() - 1;
        while self.pos < last as f64 {
            let i = self.pos as usize;
            let frac = (self.pos - i as f64) as f32;
            out.push(self.buf[i] * (1.0 - frac) + self.buf[i + 1] * frac);
            self.pos += self.step;
        }
        self.pos -= last as f64;
        self.prev = self.buf[last];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(rate: u32, hz: f32, n: usize) -> Vec<f32> {
        (0..n).map(|i| (i as f32 * hz * std::f32::consts::TAU / rate as f32).sin()).collect()
    }

    fn crossings(x: &[f32]) -> usize {
        x.windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    #[test]
    fn identity_passes_through() {
        let mut r = Resampler::new(48_000, 48_000);
        let mut out = vec![];
        r.process(&[0.1, 0.2, 0.3], &mut out);
        assert_eq!(out, [0.1, 0.2, 0.3]);
    }

    #[test]
    fn converts_length_and_keeps_pitch_across_chunks() {
        for (a, b) in [(44_100u32, 48_000u32), (48_000, 44_100), (16_000, 48_000), (48_000, 16_000)] {
            let input = sine(a, 440.0, a as usize); // one second
            let mut r = Resampler::new(a, b);
            let mut out = vec![];
            for chunk in input.chunks(333) {
                r.process(chunk, &mut out);
            }
            let want = b as i64;
            assert!((out.len() as i64 - want).abs() <= 2, "{a}->{b}: got {} samples", out.len());
            let c = crossings(&out) as i64;
            assert!((c - 440).abs() <= 2, "{a}->{b}: {c} cycles");
        }
    }
}
