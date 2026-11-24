//! The console's audio unit (`xapu.dll`): Opus packets or float PCM in,
//! float PCM out, optionally resampled with a pitch change.
//!
//! A title connects once for one kind of processing — decode Opus, decode
//! and convert, or convert PCM — gets a block of memory for its packets and
//! output, and then queues per-stream commands (activate, process, reset,
//! deactivate) and dequeues one result per command. We do each command when
//! it is queued, so its result is ready at once; the order of results is the
//! order of commands. The rate conversion itself lives in
//! [`super::apu_resample`], which is testable on every platform.
#![allow(non_snake_case)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::alloc::Layout;
use std::collections::VecDeque;
use std::ffi::c_void;
use std::sync::Mutex;

use opus_pure::OpusDecoder;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, SetEvent};

use super::apu_resample::Resampler;
use super::xasync::E_INVALIDARG;
use super::xgameruntime::{HRESULT, S_OK};
use crate::{HleModule, hle_export};

const E_NOTIMPL: HRESULT = 0x8000_4001_u32 as i32;
const E_OUTOFMEMORY: HRESULT = 0x8007_000E_u32 as i32;
const QUEUE_FULL: HRESULT = 0x8AC8_1000_u32 as i32;
/// No result yet, which titles poll for.
pub(crate) const PENDING_RESULTS: HRESULT = 0x8AC8_1001_u32 as i32;
const INVALID_COMMAND: HRESULT = 0x8AC8_1002_u32 as i32;
const INVALID_STREAM: HRESULT = 0x8AC8_1003_u32 as i32;
const ALREADY_ACTIVE: HRESULT = 0x8AC8_1005_u32 as i32;
const BAD_PACKET: HRESULT = 0x8AC8_1008_u32 as i32;

/// XApuProcessType: decode, decode and resample, resample float PCM.
const DECODE_OPUS: u32 = 1;
const DECODE_CONVERT_OPUS: u32 = 2;
const CONVERT_PCM: u32 = 3;
/// The output lengths the hardware accepts when converting.
const OUTPUT_FRAMES: [u32; 5] = [256, 480, 512, 960, 1024];

const ACTIVATE: u32 = 1;
const DEACTIVATE: u32 = 2;
const PROCESS: u32 = 3;
const NOTIFY: u32 = 4;
const RESET: u32 = 5;

/// Command options: signal the connection's event, write out a partial frame.
const SIGNAL: u32 = 1;
const FLUSH: u32 = 2;
/// Connect options (XApuConnectOptions).
const DEINTERLEAVED_INPUT: u32 = 2;
const DEINTERLEAVED_OUTPUT: u32 = 4;
const RESULT_EVENT: u32 = 8;

/// The longest Opus packet duration at 48 kHz (120 ms).
const MAX_FRAMES: usize = 5760;

#[repr(C)]
pub struct ConnectInput {
    process: u32,
    streams: u32,
    commands_per_stream: u32,
    memory: u32,
    frames_per_output: u32,
    options: u32,
}

#[repr(C)]
pub struct ConnectOutput {
    memory: *mut c_void,
    result_event: HANDLE,
}

/// What every command starts with, and what its result echoes.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct CommandId {
    kind: u32,
    stream: u32,
    sequence: u32,
    options: u32,
    context: u64,
}

#[repr(C, packed)]
struct Activate {
    id: CommandId,
    channels: u32,
    init_length: u32,
    init: *const c_void,
    start_pitch: f32,
    processing_length: u32,
    processing: *mut c_void,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Process {
    id: CommandId,
    pitch_ramp: f32,
    target_pitch: f32,
    first_frame: u32,
    frame_count: u32,
    input_length: u32,
    input: *const u8,
    output_length: u32,
    output: *mut u8,
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct ApuResult {
    id: CommandId,
    state: u32,
    state_data: u64,
    usec: u64,
    error: HRESULT,
    frames: u32,
    output_length: u32,
    output: *mut u8,
    pitch: f32,
    input_length: u32,
    input: *const u8,
}

// SAFETY: the raw pointers are title addresses carried through, never
// dereferenced after the result is made.
unsafe impl Send for ApuResult {}

pub struct Connection {
    memory: *mut u8,
    layout: Layout,
    event: HANDLE,
    process: u32,
    /// The deinterleave midpoint of a converter's output buffer.
    frames_per_output: u32,
    options: u32,
    per_stream: u32,
    state: Mutex<State>,
}

/// The decoders and converters a stream owns: a decode connection needs a
/// decoder, a convert connection a resampler, decode-convert both.
struct Stream {
    decoder: Option<OpusDecoder>,
    resampler: Option<Resampler>,
    start_pitch: f32,
}

struct State {
    streams: Vec<Option<Stream>>,
    /// Results not yet dequeued, per stream.
    queued: Vec<u32>,
    results: VecDeque<ApuResult>,
    commands: u32,
    dequeued: u32,
}

fn lock(c: &Connection) -> std::sync::MutexGuard<'_, State> {
    c.state.lock().unwrap_or_else(|e| e.into_inner())
}

pub extern "system" fn XApuConnect(
    input: *const ConnectInput,
    output: *mut ConnectOutput,
    handle: *mut *mut Connection,
) -> HRESULT {
    if input.is_null() || output.is_null() || handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: checked non-null.
    let input = unsafe { &*input };
    if !(1..=500).contains(&input.streams)
        || !(1..=16).contains(&input.commands_per_stream)
        || input.memory == 0
    {
        return E_INVALIDARG;
    }
    if !matches!(
        input.process,
        DECODE_OPUS | DECODE_CONVERT_OPUS | CONVERT_PCM
    ) {
        tracing::warn!(
            process = input.process,
            "audio decoder: unknown processing kind"
        );
        return E_NOTIMPL;
    }
    // Plain decoding writes what the packet holds; a converter fills a
    // buffer of one of the durations the hardware lists.
    if input.process == DECODE_OPUS {
        if input.frames_per_output != 0 {
            return E_INVALIDARG;
        }
    } else if !OUTPUT_FRAMES.contains(&input.frames_per_output) {
        return E_INVALIDARG;
    }
    let Ok(layout) = Layout::from_size_align(input.memory as usize, 16) else {
        return E_INVALIDARG;
    };
    // shortcut: a converter's per-stream processing buffer is a host Vec in
    // `Resampler`, not a slice carved out of this title-visible block; the
    // block still holds the commands' inputs and outputs as on the console.
    // SAFETY: non-zero size.
    let memory = unsafe { std::alloc::alloc_zeroed(layout) };
    if memory.is_null() {
        return E_OUTOFMEMORY;
    }
    let event = if input.options & RESULT_EVENT != 0 {
        // SAFETY: plain auto-reset event, no name.
        unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) }
    } else {
        std::ptr::null_mut()
    };
    let streams = input.streams as usize;
    tracing::info!(
        streams,
        memory = input.memory,
        options = input.options,
        "audio decoder connected"
    );
    let connection = Box::new(Connection {
        memory,
        layout,
        event,
        process: input.process,
        frames_per_output: input.frames_per_output,
        options: input.options,
        per_stream: input.commands_per_stream,
        state: Mutex::new(State {
            streams: (0..streams).map(|_| None).collect(),
            queued: vec![0; streams],
            results: VecDeque::new(),
            commands: 0,
            dequeued: 0,
        }),
    });
    // SAFETY: checked non-null.
    unsafe {
        output.write(ConnectOutput {
            memory: memory.cast(),
            result_event: event,
        });
        handle.write(Box::into_raw(connection));
    }
    S_OK
}

pub extern "system" fn XApuDisconnect(handle: *mut Connection) -> HRESULT {
    if handle.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: made by `XApuConnect`; disconnected once.
    let connection = unsafe { Box::from_raw(handle) };
    if !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { CloseHandle(connection.event) };
    }
    // SAFETY: allocated in `XApuConnect` with this layout.
    unsafe { std::alloc::dealloc(connection.memory, connection.layout) };
    S_OK
}

pub extern "system" fn XApuEnqueueCommand(
    handle: *mut Connection,
    command: *const CommandId,
    total: *mut u32,
) -> HRESULT {
    if handle.is_null() || command.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XApuConnect`; every command starts with an id.
    let (connection, id) = unsafe { (&*handle, command.read_unaligned()) };
    let mut state = lock(connection);
    let stream = id.stream as usize;
    if stream >= state.streams.len() {
        return INVALID_STREAM;
    }
    if state.queued[stream] >= connection.per_stream {
        return QUEUE_FULL;
    }
    let mut result = ApuResult {
        id,
        state: 0,
        state_data: 0,
        usec: 0,
        error: S_OK,
        frames: 0,
        output_length: 0,
        output: std::ptr::null_mut(),
        pitch: 1.0,
        input_length: 0,
        input: std::ptr::null(),
    };
    let stream_state = &mut state.streams[stream];
    match id.kind {
        ACTIVATE => {
            // SAFETY: an activate command is this struct.
            let activate = unsafe { command.cast::<Activate>().read_unaligned() };
            let channels = activate.channels as usize;
            tracing::debug!(
                stream,
                channels,
                process = connection.process,
                "APU stream activated"
            );
            if stream_state.is_some() {
                result.error = ALREADY_ACTIVE;
            } else if !(1..=2).contains(&channels) {
                result.error = E_INVALIDARG;
            } else {
                let decoder = match connection.process {
                    CONVERT_PCM => None,
                    _ => match OpusDecoder::new(48_000, channels) {
                        Ok(d) => Some(d),
                        Err(_) => {
                            result.error = E_INVALIDARG;
                            None
                        }
                    },
                };
                let resampler = match connection.process {
                    DECODE_OPUS => None,
                    _ => Some(Resampler::new(channels, activate.start_pitch)),
                };
                if result.error == S_OK {
                    *stream_state = Some(Stream {
                        decoder,
                        resampler,
                        start_pitch: activate.start_pitch,
                    });
                }
            }
        }
        DEACTIVATE => *stream_state = None,
        RESET => {
            if let Some(stream) = stream_state {
                let pitch = stream.start_pitch;
                if let Some(d) = &mut stream.decoder {
                    let _ = d.reset_state();
                }
                if let Some(r) = &mut stream.resampler {
                    r.reset(pitch);
                }
            }
        }
        NOTIFY => {}
        PROCESS => {
            // SAFETY: a process command is this struct.
            let command = unsafe { command.cast::<Process>().read_unaligned() };
            match stream_state {
                Some(stream) => {
                    match (
                        connection.process,
                        &mut stream.decoder,
                        &mut stream.resampler,
                    ) {
                        (DECODE_OPUS, Some(d), _) => {
                            process(d, connection.options, &command, &mut result)
                        }
                        (DECODE_CONVERT_OPUS, Some(d), Some(r)) => process_convert(
                            Some(d),
                            r,
                            connection.frames_per_output,
                            connection.options,
                            &command,
                            &mut result,
                        ),
                        (CONVERT_PCM, _, Some(r)) => process_convert(
                            None,
                            r,
                            connection.frames_per_output,
                            connection.options,
                            &command,
                            &mut result,
                        ),
                        _ => result.error = INVALID_STREAM,
                    }
                    if let Some(r) = &stream.resampler {
                        result.pitch = r.pitch();
                    }
                }
                None => result.error = INVALID_STREAM,
            }
        }
        _ => return INVALID_COMMAND,
    }
    state.queued[stream] += 1;
    state.commands = state.commands.wrapping_add(1);
    state.results.push_back(result);
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.commands) };
    }
    if id.options & SIGNAL != 0 && !connection.event.is_null() {
        // SAFETY: our event.
        unsafe { SetEvent(connection.event) };
    }
    S_OK
}

/// Decodes one packet into the command's output buffer, keeping the frames
/// from `first_frame` on, at most `frame_count` of them.
fn process(decoder: &mut OpusDecoder, options: u32, command: &Process, result: &mut ApuResult) {
    let channels = decoder.channels();
    let (input, input_length) = (command.input, command.input_length);
    let (output, output_length) = (command.output, command.output_length);
    result.input = input;
    result.input_length = input_length;
    result.output = output;
    if input.is_null() || output.is_null() || input_length == 0 {
        result.error = E_INVALIDARG;
        return;
    }
    // SAFETY: the title's packet in its connection memory.
    let packet = unsafe { std::slice::from_raw_parts(input, input_length as usize) };
    let mut pcm = vec![0f32; MAX_FRAMES * channels];
    let decoded = match decoder.decode(packet, MAX_FRAMES, &mut pcm) {
        Ok(n) => n,
        Err(_) => {
            result.error = BAD_PACKET;
            return;
        }
    };
    let first = (command.first_frame as usize).min(decoded);
    let frames = (decoded - first)
        .min(command.frame_count as usize)
        .min(output_length as usize / (4 * channels));
    let kept = &pcm[first * channels..(first + frames) * channels];
    // SAFETY: the title's output buffer holds `output_length` bytes.
    let out = unsafe { std::slice::from_raw_parts_mut(output.cast::<f32>(), frames * channels) };
    if options & DEINTERLEAVED_OUTPUT != 0 && channels == 2 {
        for (i, frame) in kept.as_chunks::<2>().0.iter().enumerate() {
            (out[i], out[frames + i]) = (frame[0], frame[1]);
        }
    } else {
        out.copy_from_slice(kept);
    }
    result.frames = frames as u32;
    result.output_length = (frames * channels * 4) as u32;
}

/// Decodes (when the connection decodes) and resamples one command's input
/// into float PCM at the pitch the title asked for.
fn process_convert(
    mut decoder: Option<&mut OpusDecoder>,
    resampler: &mut Resampler,
    max_frames: u32,
    options: u32,
    command: &Process,
    result: &mut ApuResult,
) {
    let channels = resampler.channels();
    let (input, input_length) = (command.input, command.input_length);
    let (output, output_length) = (command.output, command.output_length);
    result.input = input;
    result.input_length = input_length;
    result.output = output;
    if input.is_null() || output.is_null() || input_length == 0 {
        result.error = E_INVALIDARG;
        return;
    }
    // SAFETY: the title's buffer in its connection memory.
    let bytes = unsafe { std::slice::from_raw_parts(input, input_length as usize) };
    let pcm;
    if let Some(decoder) = decoder.as_deref_mut() {
        let mut decoded = vec![0f32; MAX_FRAMES * channels];
        let n = match decoder.decode(bytes, MAX_FRAMES, &mut decoded) {
            Ok(n) => n,
            Err(_) => {
                result.error = BAD_PACKET;
                return;
            }
        };
        let first = (command.first_frame as usize).min(n);
        let frames = (n - first).min(command.frame_count as usize);
        pcm = decoded[first * channels..(first + frames) * channels].to_vec();
    } else {
        // ConvertPCM's frameCount is required and fixes the input length.
        let frames = command.frame_count as usize;
        let minimum = if channels == 2 { 8 } else { 4 };
        let multiple = if channels == 2 { 8 } else { 4 };
        if frames < minimum
            || frames > 960
            || frames % multiple != 0
            || input_length as usize != frames * channels * 4
        {
            result.error = E_INVALIDARG;
            return;
        }
        let samples: Vec<f32> = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        pcm = if options & DEINTERLEAVED_INPUT != 0 && channels == 2 {
            let frames = samples.len() / 2;
            let mut interleaved = vec![0f32; samples.len()];
            for (i, frame) in interleaved.as_chunks_mut::<2>().0.iter_mut().enumerate() {
                *frame = [samples[i], samples[frames + i]];
            }
            interleaved
        } else {
            samples
        };
    }
    let room = output_length as usize / (4 * channels);
    let mut converted = vec![0f32; room * channels];
    let frames = resampler.convert(
        &pcm,
        &mut converted,
        command.target_pitch,
        command.pitch_ramp,
        options & FLUSH != 0,
    );
    if options & DEINTERLEAVED_OUTPUT != 0 && channels == 2 {
        // The converter's midpoint is `maxFrameCountPerOutput`, even when
        // fewer frames were written (the docs spell this out).
        let midpoint = max_frames as usize;
        let len = midpoint + frames;
        if len * 4 > output_length as usize {
            result.error = E_INVALIDARG;
            return;
        }
        // SAFETY: the title's output buffer holds `output_length` bytes.
        let out = unsafe { std::slice::from_raw_parts_mut(output.cast::<f32>(), len) };
        for (i, frame) in converted[..frames * 2]
            .as_chunks::<2>()
            .0
            .iter()
            .enumerate()
        {
            out[i] = frame[0];
            out[midpoint + i] = frame[1];
        }
    } else {
        // SAFETY: the title's output buffer holds `output_length` bytes.
        let out =
            unsafe { std::slice::from_raw_parts_mut(output.cast::<f32>(), frames * channels) };
        out.copy_from_slice(&converted[..frames * channels]);
    }
    result.frames = frames as u32;
    result.output_length = (frames * channels * 4) as u32;
    result.pitch = resampler.pitch();
}

pub extern "system" fn XApuDequeueResult(handle: *mut Connection, out: *mut ApuResult) -> HRESULT {
    if handle.is_null() || out.is_null() {
        return E_INVALIDARG;
    }
    // SAFETY: a connection from `XApuConnect`.
    let mut state = lock(unsafe { &*handle });
    let Some(result) = state.results.pop_front() else {
        return PENDING_RESULTS;
    };
    let stream = result.id.stream as usize;
    state.queued[stream] -= 1;
    state.dequeued = state.dequeued.wrapping_add(1);
    // SAFETY: the title passes room for one result.
    unsafe { out.write_unaligned(result) };
    S_OK
}

pub extern "system" fn XApuGetResultCount(handle: *mut Connection, total: *mut u32) -> u32 {
    if handle.is_null() {
        return 0;
    }
    // SAFETY: a connection from `XApuConnect`.
    let state = lock(unsafe { &*handle });
    if !total.is_null() {
        // SAFETY: optional out parameter.
        unsafe { total.write(state.dequeued) };
    }
    state.results.len() as u32
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "apu",
        dlls: &["xapu.dll"],
        exports: vec![
            hle_export!(XApuConnect),
            hle_export!(XApuDisconnect),
            hle_export!(XApuEnqueueCommand),
            hle_export!(XApuDequeueResult),
            hle_export!(XApuGetResultCount),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opus_pure::{Application, OpusEncoder};

    fn id(kind: u32, stream: u32) -> CommandId {
        CommandId {
            kind,
            stream,
            sequence: 7,
            options: 0,
            context: 0x1234,
        }
    }

    #[test]
    fn decodes_opus_as_the_title_drives_it() {
        assert_eq!(size_of::<ApuResult>(), 0x50);
        // 256 mono/stereo streams, two commands each, as the title connects.
        let input = ConnectInput {
            process: DECODE_OPUS,
            streams: 256,
            commands_per_stream: 2,
            memory: 0x10_4000,
            frames_per_output: 0,
            options: 1,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        let base = output.memory.cast::<u8>();

        let activate = Activate {
            id: id(ACTIVATE, 3),
            channels: 2,
            init_length: 0,
            init: std::ptr::null(),
            start_pitch: 1.0,
            processing_length: 0,
            processing: std::ptr::null_mut(),
        };
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const activate).cast(), std::ptr::null_mut()),
            S_OK
        );

        // A 20 ms stereo packet of a 440 Hz tone.
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| ((i / 2) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();
        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
        let process = Process {
            id: id(PROCESS, 3),
            pitch_ramp: 0.0,
            target_pitch: 1.0,
            first_frame: 0,
            frame_count: 960,
            input_length: n as u32,
            input: base,
            output_length: 0xf00 * 2,
            output: base.wrapping_add(0x14000),
        };
        let mut total = 0;
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
            S_OK
        );
        assert_eq!(total, 2);
        // Two results queued for stream 3: its queue is full.
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const process).cast(), &mut total),
            QUEUE_FULL
        );
        assert_eq!(XApuGetResultCount(handle, std::ptr::null_mut()), 2);

        let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let first = unsafe { result.assume_init() };
        let (kind, error, context) = (first.id.kind, first.error, first.id.context);
        assert_eq!((kind, error, context), (ACTIVATE, S_OK, 0x1234));
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let decoded = unsafe { result.assume_init() };
        let (kind, error, frames, length) = (
            decoded.id.kind,
            decoded.error,
            decoded.frames,
            decoded.output_length,
        );
        assert_eq!((kind, error, frames, length), (PROCESS, S_OK, 960, 960 * 8));
        // SAFETY: the decoder wrote 960 stereo frames there.
        let pcm =
            unsafe { std::slice::from_raw_parts(base.wrapping_add(0x14000).cast::<f32>(), 1920) };
        let energy: f32 = pcm.iter().map(|s| s * s).sum::<f32>() / 1920.0;
        assert!(energy > 0.01, "decoded a tone, energy {energy}");
        assert_eq!(
            XApuDequeueResult(handle, result.as_mut_ptr()),
            PENDING_RESULTS
        );
        let mut dequeued = 0;
        assert_eq!(XApuGetResultCount(handle, &mut dequeued), 0);
        assert_eq!(dequeued, 2);

        // A process on a stream that was never activated fails in its result.
        let mut stray = process;
        stray.id.stream = 4;
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const stray).cast(), &mut total),
            S_OK
        );
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let error = unsafe { result.assume_init() }.error;
        assert_eq!(error, INVALID_STREAM);
        assert_eq!(XApuDisconnect(handle), S_OK);
    }

    fn connect(process: u32, frames: u32) -> (*mut Connection, *mut u8) {
        let input = ConnectInput {
            process,
            streams: 1,
            commands_per_stream: 2,
            memory: 0x10_0000,
            frames_per_output: frames,
            options: 0,
        };
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        assert_eq!(XApuConnect(&input, &mut output, &mut handle), S_OK);
        (handle, output.memory.cast::<u8>())
    }

    fn activate(handle: *mut Connection, channels: u32, pitch: f32) {
        let command = Activate {
            id: id(ACTIVATE, 0),
            channels,
            init_length: 0,
            init: std::ptr::null(),
            start_pitch: pitch,
            processing_length: 0,
            processing: std::ptr::null_mut(),
        };
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const command).cast(), std::ptr::null_mut()),
            S_OK
        );
    }

    #[test]
    fn connects_for_both_conversion_kinds() {
        let mut output = ConnectOutput {
            memory: std::ptr::null_mut(),
            result_event: std::ptr::null_mut(),
        };
        let mut handle = std::ptr::null_mut();
        fn connect_as(
            input: &ConnectInput,
            output: &mut ConnectOutput,
            handle: &mut *mut Connection,
        ) -> HRESULT {
            XApuConnect(input, output, handle)
        }
        let input = |process, frames| ConnectInput {
            process,
            streams: 1,
            commands_per_stream: 1,
            memory: 0x1_0000,
            frames_per_output: frames,
            options: 0,
        };
        assert_eq!(
            connect_as(&input(DECODE_OPUS, 0), &mut output, &mut handle),
            S_OK
        );
        assert_eq!(XApuDisconnect(handle), S_OK);
        for frames in OUTPUT_FRAMES {
            for process in [DECODE_CONVERT_OPUS, CONVERT_PCM] {
                assert_eq!(
                    connect_as(&input(process, frames), &mut output, &mut handle),
                    S_OK
                );
                assert_eq!(XApuDisconnect(handle), S_OK);
            }
        }
        assert_eq!(
            connect_as(&input(DECODE_OPUS, 256), &mut output, &mut handle),
            E_INVALIDARG
        );
        assert_eq!(
            connect_as(&input(DECODE_CONVERT_OPUS, 0), &mut output, &mut handle),
            E_INVALIDARG
        );
        assert_eq!(
            connect_as(&input(CONVERT_PCM, 7), &mut output, &mut handle),
            E_INVALIDARG
        );
        assert_eq!(
            connect_as(&input(9, 256), &mut output, &mut handle),
            E_NOTIMPL
        );
    }

    #[test]
    fn converts_float_pcm_at_the_target_pitch() {
        let (handle, base) = connect(CONVERT_PCM, 256);
        activate(handle, 1, 1.0);
        let samples: Vec<f32> = (0..16).map(|i| i as f32).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len()) };
        let command = Process {
            id: id(PROCESS, 0),
            pitch_ramp: 0.0,
            target_pitch: 2.0,
            first_frame: 0,
            frame_count: 16,
            input_length: bytes.len() as u32,
            input: base,
            output_length: 256 * 4,
            output: base.wrapping_add(0x1000),
        };
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const command).cast(), std::ptr::null_mut()),
            S_OK
        );
        let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let first = unsafe { result.assume_init() };
        let first_kind = first.id.kind;
        assert_eq!(first_kind, ACTIVATE);
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let result = unsafe { result.assume_init() };
        let (error, frames, pitch, output_length) = (
            result.error,
            result.frames,
            result.pitch,
            result.output_length,
        );
        assert_eq!((error, frames, pitch, output_length), (S_OK, 8, 2.0, 8 * 4));
        // SAFETY: the converter wrote eight mono frames there.
        let pcm = unsafe { std::slice::from_raw_parts(base.wrapping_add(0x1000).cast::<f32>(), 8) };
        assert_eq!(pcm, [0.0, 2.0, 4.0, 6.0, 8.0, 10.0, 12.0, 14.0]);
        assert_eq!(XApuDisconnect(handle), S_OK);
    }

    #[test]
    fn decodes_and_converts_an_opus_stream() {
        let (handle, base) = connect(DECODE_CONVERT_OPUS, 960);
        activate(handle, 2, 1.0);
        let tone: Vec<f32> = (0..960 * 2)
            .map(|i| ((i / 2) as f32 * 440.0 * std::f32::consts::TAU / 48_000.0).sin() * 0.5)
            .collect();
        let mut encoder = OpusEncoder::new(48_000, 2, Application::Audio).unwrap();
        let mut packet = [0u8; 1500];
        let n = encoder.encode(&tone, 960, &mut packet).unwrap();
        // SAFETY: inside the connection memory.
        unsafe { std::ptr::copy_nonoverlapping(packet.as_ptr(), base, n) };
        let command = Process {
            id: id(PROCESS, 0),
            pitch_ramp: 0.0,
            target_pitch: 0.5,
            first_frame: 0,
            frame_count: 960,
            input_length: n as u32,
            input: base,
            output_length: 2048 * 2 * 4,
            output: base.wrapping_add(0x1000),
        };
        assert_eq!(
            XApuEnqueueCommand(handle, (&raw const command).cast(), std::ptr::null_mut()),
            S_OK
        );
        let mut result = std::mem::MaybeUninit::<ApuResult>::zeroed();
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let first = unsafe { result.assume_init() };
        let first_kind = first.id.kind;
        assert_eq!(first_kind, ACTIVATE);
        assert_eq!(XApuDequeueResult(handle, result.as_mut_ptr()), S_OK);
        // SAFETY: written above.
        let result = unsafe { result.assume_init() };
        let (error, frames, pitch, output_length) = (
            result.error,
            result.frames,
            result.pitch,
            result.output_length,
        );
        assert_eq!(error, S_OK);
        assert!(frames > 960, "a half pitch writes more frames");
        assert_eq!(pitch, 0.5);
        assert_eq!(output_length, frames * 2 * 4);
        // SAFETY: the converter wrote `frames` stereo frames there.
        let pcm = unsafe {
            std::slice::from_raw_parts(base.wrapping_add(0x1000).cast::<f32>(), frames as usize * 2)
        };
        let energy: f32 = pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32;
        assert!(energy > 0.01, "converted a tone, energy {energy}");
        assert_eq!(XApuDisconnect(handle), S_OK);
    }
}
