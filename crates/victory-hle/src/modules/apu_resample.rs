//! The sample-rate converter of the console's audio unit (XAPU HSRC), kept
//! host-independent so its arithmetic is covered on every platform.
//!
//! The unit consumes Opus or float PCM at 48 kHz and writes float PCM. A
//! pitch of 1 leaves the rate unchanged; a pitch of 2 plays twice as fast
//! (half as many output frames), 0.5 twice as slowly. Frames left over at
//! the end of one command carry into the next, as they do on the hardware.

// Linear interpolation between neighbouring samples. It is cheap and
// keeps the tone tests exact at whole-number pitches; a windowed-sinc
// filter would be quieter at large ratios but is not needed to fill the
// title's buffers with plausible audio.
pub(crate) struct Resampler {
    channels: usize,
    /// Interleaved input frames not yet consumed.
    pending: Vec<f32>,
    /// Position of the next output frame in `pending`, in frames.
    position: f64,
    pitch: f32,
}

impl Resampler {
    pub(crate) fn new(channels: usize, pitch: f32) -> Self {
        Self {
            channels,
            pending: Vec::new(),
            position: 0.0,
            pitch: clamp(pitch),
        }
    }

    pub(crate) fn reset(&mut self, pitch: f32) {
        self.pending.clear();
        self.position = 0.0;
        self.pitch = clamp(pitch);
    }

    pub(crate) fn pitch(&self) -> f32 {
        self.pitch
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    /// Converts `input` (interleaved frames) into `output`, returning the
    /// number of frames written. `ramp` moves `pitch` towards `target` by
    /// that much per frame. With `flush`, the last sample stands in for the
    /// one after it so the remaining input can be written out.
    pub(crate) fn convert(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        target: f32,
        ramp: f32,
        flush: bool,
    ) -> usize {
        let channels = self.channels;
        if channels == 0 || output.len() < channels {
            return 0;
        }
        let target = clamp(target);
        // A zero ramp means the target applies at once, not that the current
        // pitch is kept. `step` only handles a gradual change.
        if ramp <= 0.0 || !ramp.is_finite() {
            self.pitch = target;
        }
        self.pending.extend_from_slice(input);
        let frames = output.len() / channels;
        let mut written = 0;
        while written < frames {
            let available = self.pending.len() / channels;
            let index = self.position as usize;
            // One more sample is needed to interpolate; without it, stop
            // unless the caller asked for the last partial frame.
            if index >= available || (index + 1 >= available && !flush) {
                break;
            }
            let fraction = (self.position - index as f64) as f32;
            let (out, rest) = output[written * channels..].split_at_mut(channels);
            for (channel, sample) in out.iter_mut().enumerate() {
                let a = self.pending[index * channels + channel];
                let b = if index + 1 < available {
                    self.pending[(index + 1) * channels + channel]
                } else {
                    a
                };
                *sample = a + (b - a) * fraction;
            }
            let _ = rest;
            written += 1;
            self.position += f64::from(self.pitch);
            self.pitch = step(self.pitch, target, ramp);
        }
        // Keep the unconsumed tail, including the sample the next call
        // interpolates towards.
        let consumed = self.position as usize;
        if consumed > 0 {
            self.pending.drain(..consumed * channels);
            self.position -= consumed as f64;
        }
        written
    }
}

fn clamp(pitch: f32) -> f32 {
    if pitch.is_finite() {
        pitch.clamp(1.0 / 8.0, 8.0)
    } else {
        1.0
    }
}

/// Moves `from` towards `to` by at most `step`, without overshooting.
fn step(from: f32, to: f32, step: f32) -> f32 {
    let difference = to - from;
    if !step.is_finite() || step <= 0.0 {
        return from;
    }
    if step >= difference.abs() {
        to
    } else {
        from + step * difference.signum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(
        input: &[f32],
        channels: usize,
        pitch: f32,
        room: usize,
    ) -> (Vec<f32>, usize, Resampler) {
        let mut resampler = Resampler::new(channels, pitch);
        let mut output = vec![0.0; room * channels];
        let frames = resampler.convert(input, &mut output, pitch, 0.0, false);
        output.truncate(frames * channels);
        (output, frames, resampler)
    }

    #[test]
    fn unit_pitch_passes_samples_through() {
        let input: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let (output, frames, _) = run(&input, 1, 1.0, 64);
        // The last sample needs a successor, so it waits for the next call.
        assert_eq!(frames, 15);
        assert_eq!(output, input[..15]);
    }

    #[test]
    fn half_pitch_doubles_the_frames_and_interpolates() {
        let input = [0.0, 1.0];
        let (output, frames, _) = run(&input, 1, 0.5, 8);
        assert_eq!(frames, 2);
        assert!((output[0] - 0.0).abs() < 1e-6);
        assert!((output[1] - 0.5).abs() < 1e-6);
    }

    #[test]
    fn double_pitch_halves_the_frames() {
        let input: Vec<f32> = (0..9).map(|i| i as f32).collect();
        let (output, frames, _) = run(&input, 1, 2.0, 64);
        assert_eq!(frames, 4);
        assert_eq!(output, vec![0.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn stereo_channels_stay_paired() {
        let input = [0.0, 10.0, 1.0, 11.0, 2.0, 12.0];
        let (output, frames, _) = run(&input, 2, 1.0, 8);
        assert_eq!(frames, 2);
        assert_eq!(output, [0.0, 10.0, 1.0, 11.0]);
    }

    #[test]
    fn input_carries_across_commands() {
        let mut resampler = Resampler::new(1, 1.0);
        let mut output = [0.0; 8];
        assert_eq!(
            resampler.convert(&[0.0, 1.0], &mut output, 1.0, 0.0, false),
            1
        );
        assert_eq!(output[0], 0.0);
        // The 1 still waits for a successor; the next command supplies it.
        assert_eq!(resampler.convert(&[2.0], &mut output, 1.0, 0.0, false), 1);
        assert_eq!(output[0], 1.0);
        assert_eq!(resampler.convert(&[3.0], &mut output, 1.0, 0.0, false), 1);
        assert_eq!(output[0], 2.0);
    }

    #[test]
    fn flush_writes_the_last_partial_frame() {
        let mut resampler = Resampler::new(1, 1.0);
        let mut output = [0.0; 8];
        assert_eq!(
            resampler.convert(&[0.0, 1.0], &mut output, 1.0, 0.0, true),
            2
        );
        assert_eq!(output[..2], [0.0, 1.0]);
    }

    #[test]
    fn a_zero_ramp_applies_the_target_immediately() {
        let mut resampler = Resampler::new(1, 1.0);
        let input: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let mut output = vec![0.0; 16];
        let written = resampler.convert(&input, &mut output, 2.0, 0.0, false);
        assert_eq!(written, 8, "twice the pitch consumes twice the input per frame");
        assert_eq!(resampler.pitch(), 2.0);
        assert_eq!(output[..8], [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0]);
    }

    #[test]
    fn pitch_ramps_towards_the_target() {
        let mut resampler = Resampler::new(1, 1.0);
        assert_eq!(resampler.pitch(), 1.0);
        let input: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let mut output = vec![0.0; 32];
        resampler.convert(&input, &mut output, 2.0, 0.25, false);
        assert_eq!(resampler.pitch(), 2.0, "eight frames of a quarter ramp");
        resampler.reset(8.0);
        assert_eq!(
            resampler.pitch(),
            8.0,
            "pitch is bounded as the hardware is"
        );
        resampler.reset(f32::NAN);
        assert_eq!(resampler.pitch(), 1.0);
    }

    #[test]
    fn rejects_invalid_shapes() {
        let mut resampler = Resampler::new(0, 1.0);
        let mut output = [0.0; 8];
        assert_eq!(resampler.convert(&[1.0], &mut output, 1.0, 0.0, false), 0);
        let mut resampler = Resampler::new(2, 1.0);
        assert_eq!(
            resampler.convert(&[1.0], &mut output[..1], 1.0, 0.0, false),
            0
        );
    }
}
