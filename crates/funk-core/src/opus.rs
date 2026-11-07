//! Minimal safe wrapper around `unsafe-libopus` (libopus translated to Rust).
//! Pure Rust: no C toolchain, no dynamic linking, identical on every platform.

use unsafe_libopus as ffi;

#[derive(Debug, thiserror::Error)]
#[error("opus error {0}")]
pub struct OpusError(pub i32);

fn check(code: i32) -> Result<i32, OpusError> {
    if code < 0 { Err(OpusError(code)) } else { Ok(code) }
}

/// 48 kHz mono voice encoder.
pub struct Encoder(*mut ffi::OpusEncoder);

// The raw state is only touched through `&mut self`, so moving it between threads is sound.
unsafe impl Send for Encoder {}

impl Encoder {
    pub fn new_voip_48k_mono(bitrate: i32) -> Result<Self, OpusError> {
        let mut err = 0;
        // SAFETY: plain constructor call; the result is checked before use.
        let st = unsafe { ffi::opus_encoder_create(48_000, 1, ffi::OPUS_APPLICATION_VOIP, &mut err) };
        check(err)?;
        if st.is_null() {
            return Err(OpusError(ffi::OPUS_ALLOC_FAIL));
        }
        let enc = Encoder(st);
        // SAFETY: `st` is a valid encoder owned by `enc`.
        unsafe {
            check(ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_BITRATE_REQUEST, bitrate))?;
            check(ffi::opus_encoder_ctl!(st, ffi::OPUS_SET_VBR_REQUEST, 1))?;
        }
        Ok(enc)
    }

    /// Encode one frame (e.g. 960 samples = 20 ms). Returns the packet length.
    pub fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, OpusError> {
        // SAFETY: pointers and lengths come from live slices.
        let n = unsafe { ffi::opus_encode(self.0, pcm.as_ptr(), pcm.len() as i32, out.as_mut_ptr(), out.len() as i32) };
        check(n).map(|n| n as usize)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_encoder_create, freed exactly once.
        unsafe { ffi::opus_encoder_destroy(self.0) }
    }
}

/// 48 kHz mono decoder.
pub struct Decoder(*mut ffi::OpusDecoder);

unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new_48k_mono() -> Result<Self, OpusError> {
        let mut err = 0;
        // SAFETY: plain constructor call; the result is checked before use.
        let st = unsafe { ffi::opus_decoder_create(48_000, 1, &mut err) };
        check(err)?;
        if st.is_null() {
            return Err(OpusError(ffi::OPUS_ALLOC_FAIL));
        }
        Ok(Decoder(st))
    }

    /// Decode a packet, or conceal one lost frame when `data` is `None` (fills `pcm.len()` samples).
    pub fn decode(&mut self, data: Option<&[u8]>, pcm: &mut [i16]) -> Result<usize, OpusError> {
        let (ptr, len) = match data {
            Some(d) => (d.as_ptr(), d.len() as i32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: pointers and lengths come from live slices (or null/0 for concealment).
        let n = unsafe { ffi::opus_decode(self.0, ptr, len, pcm.as_mut_ptr(), pcm.len() as i32, 0) };
        check(n).map(|n| n as usize)
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_decoder_create, freed exactly once.
        unsafe { ffi::opus_decoder_destroy(self.0) }
    }
}
