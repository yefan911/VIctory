//! Debugging aid: `VICTORY_AUDIO_CAPTURE=<file.wav>` records what the title
//! plays, 48 kHz stereo 16-bit, through process loopback: our own process
//! tree only, never the rest of the host's mix. Stretches where nothing was
//! played are written as silence, so the file keeps real time.
//!
//! The recorder runs in a child copy of this executable: an audio client
//! in the title's process raises that process's timer resolution, which
//! changes how the title's own waits behave.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVE_FORMAT_PCM, WAVEFORMATEX,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};
use windows::Win32::Foundation::WAIT_OBJECT_0;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_SYNCHRONIZE, Sleep, WaitForSingleObject,
};
use windows::core::{HRESULT, IUnknown, Interface, Ref, implement};

const RATE: u32 = 48_000;
const FRAME_BYTES: usize = 4;

/// Activation completes asynchronously; the recording thread polls for it.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activated;

impl IActivateAudioInterfaceCompletionHandler_Impl for Activated_Impl {
    fn ActivateCompleted(
        &self,
        _: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

/// Names the process to record, in the recorder's environment.
const TARGET: &str = "VICTORY_AUDIO_CAPTURE_OF";

/// Starts the recorder if `VICTORY_AUDIO_CAPTURE` names a file. With this
/// module's debug log, also logs once a second what a `Sleep(1)` takes in
/// this process (the title's millisecond waits get the same).
pub fn start() {
    if tracing::enabled!(tracing::Level::DEBUG) {
        let _ = std::thread::Builder::new()
            .name("victory timing probe".into())
            .spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                    let at = Instant::now();
                    // SAFETY: plain wait.
                    unsafe { Sleep(1) };
                    let ms = at.elapsed().as_secs_f32() * 1000.0;
                    tracing::debug!(ms, "Sleep(1) took");
                }
            });
    }
    if std::env::var_os("VICTORY_AUDIO_CAPTURE").is_some()
        && let Err(e) = std::env::current_exe().and_then(|exe| {
            std::process::Command::new(exe)
                .env(TARGET, std::process::id().to_string())
                .spawn()
        })
    {
        tracing::warn!("audio capture not started: {e}");
    }
}

/// In the recorder (a child started by [`start`]): records until the
/// target exits and returns true; false in any other process.
pub fn child() -> bool {
    let (Some(pid), Some(path)) = (
        std::env::var(TARGET).ok().and_then(|p| p.parse().ok()),
        std::env::var_os("VICTORY_AUDIO_CAPTURE"),
    ) else {
        return false;
    };
    if let Err(e) = record(pid, Path::new(&path)) {
        tracing::warn!("audio capture stopped: {e}");
    }
    true
}

fn header(frames: u64) -> [u8; 44] {
    const MAX_DATA_BYTES: u32 = (u32::MAX - 36) & !(FRAME_BYTES as u32 - 1);
    let data = frames
        .saturating_mul(FRAME_BYTES as u64)
        .min(u64::from(MAX_DATA_BYTES)) as u32;
    let mut h = [0u8; 44];
    let fields: [(&[u8], usize); 11] = [
        (b"RIFF", 0),
        (&(36 + data).to_le_bytes(), 4),
        (b"WAVEfmt ", 8),
        (&16u32.to_le_bytes(), 16),
        (&1u16.to_le_bytes(), 20),
        (&2u16.to_le_bytes(), 22),
        (&RATE.to_le_bytes(), 24),
        (&(RATE * FRAME_BYTES as u32).to_le_bytes(), 28),
        (&(FRAME_BYTES as u16).to_le_bytes(), 32),
        (&16u16.to_le_bytes(), 34),
        (b"data", 36),
    ];
    for (bytes, at) in fields {
        h[at..at + bytes.len()].copy_from_slice(bytes);
    }
    h[40..].copy_from_slice(&data.to_le_bytes());
    h
}

fn silence_frames(written: u64, first: &mut Option<u64>, qpc: u64, flags: u32) -> u64 {
    if flags & windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32 != 0 {
        return 0;
    }
    let start = *first.get_or_insert_with(|| {
        let offset =
            u64::try_from(u128::from(written) * 10_000_000 / u128::from(RATE)).unwrap_or(u64::MAX);
        qpc.saturating_sub(offset)
    });
    let Some(elapsed) = qpc.checked_sub(start) else {
        return 0;
    };
    let at = (u128::from(elapsed) * u128::from(RATE) / 10_000_000) as u64;
    if at > written.saturating_add(u64::from(RATE / 500)) {
        at - written
    } else {
        0
    }
}

fn write_zeros(writer: &mut impl std::io::Write, mut bytes: u64) -> std::io::Result<()> {
    static ZEROS: [u8; 16 * 1024] = [0; 16 * 1024];
    while bytes > 0 {
        let chunk = bytes.min(ZEROS.len() as u64) as usize;
        writer.write_all(&ZEROS[..chunk])?;
        bytes -= chunk as u64;
    }
    Ok(())
}

fn record(pid: u32, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    // SAFETY: COM calls with parameters built here; buffers are used only
    // between `GetBuffer` and `ReleaseBuffer`.
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
        let target = OpenProcess(PROCESS_SYNCHRONIZE, false, pid)?;
        let params = AUDIOCLIENT_ACTIVATION_PARAMS {
            ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
            Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                    TargetProcessId: pid,
                    ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
                },
            },
        };
        /// A `PROPVARIANT` holding a blob (`VT_BLOB`).
        #[repr(C)]
        struct Blob {
            vt: u16,
            reserved: [u16; 3],
            size: u32,
            data: *const AUDIOCLIENT_ACTIVATION_PARAMS,
        }
        let blob = Blob {
            vt: 0x41,
            reserved: [0; 3],
            size: size_of_val(&params) as u32,
            data: &params,
        };
        let handler: IActivateAudioInterfaceCompletionHandler = Activated.into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some((&raw const blob).cast()),
            &handler,
        )?;
        let client = loop {
            let (mut hr, mut client) = (HRESULT(0), None::<IUnknown>);
            // Fails (`E_ILLEGAL_METHOD_CALL`) until activation completes.
            if operation.GetActivateResult(&mut hr, &mut client).is_ok() {
                hr.ok()?;
                break client.ok_or("no audio client")?.cast::<IAudioClient>()?;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * FRAME_BYTES as u32,
            nBlockAlign: FRAME_BYTES as u16,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            10_000_000,
            0,
            &format,
            None,
        )?;
        let capture: IAudioCaptureClient = client.GetService()?;
        let mut file = File::create(path)?;
        file.write_all(&header(0))?;
        client.Start()?;
        tracing::info!(path = %path.display(), "recording the title's audio");

        let (mut written, mut first) = (0u64, None);
        let mut patched = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let ended = WaitForSingleObject(target, 0) == WAIT_OBJECT_0;
            while capture.GetNextPacketSize()? > 0 {
                let (mut data, mut frames, mut flags, mut qpc) = (std::ptr::null_mut(), 0, 0, 0);
                capture.GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))?;
                // Where this packet belongs, from its time stamp (100 ns).
                let silence = silence_frames(written, &mut first, qpc, flags);
                if silence > 0 {
                    tracing::debug!(
                        ms = (u128::from(silence) * 1000 / u128::from(RATE)) as u64,
                        "audio capture: nothing played"
                    );
                    write_zeros(&mut file, silence.saturating_mul(FRAME_BYTES as u64))?;
                    written = written.saturating_add(silence);
                }
                let bytes = u64::from(frames) * FRAME_BYTES as u64;
                let packet_frames = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 {
                    write_zeros(&mut file, bytes)?;
                    u64::from(frames)
                } else if bytes > 0 && !data.is_null() {
                    let len = usize::try_from(bytes)?;
                    file.write_all(std::slice::from_raw_parts(data, len))?;
                    u64::from(frames)
                } else {
                    0
                };
                if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 && written > 0 {
                    tracing::debug!("audio capture: discontinuity");
                }
                written = written.saturating_add(packet_frames);
                capture.ReleaseBuffer(frames)?;
            }
            if ended || patched.elapsed() >= Duration::from_secs(1) {
                // Valid sizes at any time: runs end by being killed.
                file.seek(SeekFrom::Start(0))?;
                file.write_all(&header(written))?;
                file.seek(SeekFrom::End(0))?;
                patched = Instant::now();
            }
            if ended {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_caps_riff_size_without_overflow_and_stays_frame_aligned() {
        for frames in [1_073_741_820, u64::MAX] {
            let h = header(frames);
            let riff_size = u32::from_le_bytes(h[4..8].try_into().unwrap());
            let data_size = u32::from_le_bytes(h[40..44].try_into().unwrap());
            assert_eq!(data_size, 4_294_967_256);
            assert_eq!(data_size % 4, 0);
            assert_eq!(riff_size, 36 + data_size);
        }

        let normal = header(480);
        let riff_size = u32::from_le_bytes(normal[4..8].try_into().unwrap());
        let data_size = u32::from_le_bytes(normal[40..44].try_into().unwrap());
        assert_eq!(data_size, 1_920);
        assert_eq!(riff_size, 1_956);
    }

    #[test]
    fn timestamp_errors_and_backwards_qpc_do_not_underflow_or_anchor_zero() {
        let ts_err = windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR.0 as u32;
        let mut first = None;

        assert_eq!(silence_frames(0, &mut first, 0, ts_err), 0);
        assert_eq!(first, None);

        assert_eq!(silence_frames(480, &mut first, 50_000_000, 0), 0);
        assert_eq!(first, Some(49_900_000));

        assert_eq!(silence_frames(480, &mut first, 10_000_000, 0), 0);
        assert_eq!(silence_frames(480, &mut first, 90_000_000, ts_err), 0);
        assert_eq!(first, Some(49_900_000));

        assert_eq!(silence_frames(960, &mut first, 49_900_000 + 220_000, 0), 0);
        assert_eq!(
            silence_frames(960, &mut first, 49_900_000 + 10_000_000, 0),
            48_000 - 960
        );

        let mut max_first = Some(0);
        let expected = ((u128::from(u64::MAX) * u128::from(RATE)) / 10_000_000) as u64;
        assert_eq!(silence_frames(0, &mut max_first, u64::MAX, 0), expected);
    }

    #[test]
    fn write_zeros_streams_exact_byte_count() {
        let mut buf = Vec::new();
        write_zeros(&mut buf, 40_000).unwrap();
        assert_eq!(buf.len(), 40_000);
        assert!(buf.iter().all(|&b| b == 0));

        let mut empty = Vec::new();
        write_zeros(&mut empty, 0).unwrap();
        assert!(empty.is_empty());
    }
}
