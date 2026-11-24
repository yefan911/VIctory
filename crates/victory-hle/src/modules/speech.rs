//! Accessibility: speech synthesis for the title's narration. No voice is
//! installed here, and every stream is valid WAVE audio of no length, so
//! narration is silent. Closed captions report off with the default colors,
//! high contrast is off, and speech to text takes its strings and ids without
//! an overlay to draw them on.
// shortcut: silent speech. The host's engine (SAPI through COM, or WinRT
// speech synthesis) needs COM bindings this crate has only in diagnostics
// builds, plus voice enumeration and SSML the title sends (slot 0xa8); the
// title then plays the empty stream and goes on.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::xasync::{E_INVALIDARG, E_NOT_SUFFICIENT_BUFFER};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, S_OK, add_ref, missing, put, text,
};

/// Closed captions, high contrast, speech to text and synthesis.
pub const PROVIDER: Guid = Guid::new(
    0x3e24_1977,
    0x6237,
    0x41e9,
    [0x85, 0x59, 0x10, 0x2c, 0x2d, 0x99, 0x83, 0xf1],
);
/// Unchanged since 2020.
pub const IID: Guid = Guid::new(
    0xd722_b373,
    0x8c4d,
    0x4692,
    [0x9e, 0x51, 0xd6, 0xad, 0x9b, 0x37, 0xaa, 0x7d],
);

pub static OBJECT: Object = Object::new("accessibility", &VTABLE);
pub const INTERFACES: &[Interface] = &[(PROVIDER, IID, &OBJECT)];

/// A WAVE file of 16 kHz mono 16-bit PCM with no samples: RIFF size 36,
/// a 16-byte format chunk (PCM, 1 channel, 16,000 Hz, 32,000 bytes/s,
/// block 2, 16 bits), an empty data chunk.
const SILENCE: [u8; 44] =
    *b"RIFF\x24\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80\x3e\0\0\0\x7d\0\0\x02\0\x10\0data\0\0\0\0";

/// Synthesizers and streams hold nothing, so their handles are fixed
/// addresses and closing them is free.
static SYNTHESIZER: u8 = 0;
static STREAM: u8 = 0;

type This = *const Object;

/// `XClosedCaptionProperties`, as `XAccessibility.h` declares it: three
/// `XColor`s, the edge and style enums, the scale, then the flag.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClosedCaptionProperties {
    pub background: u32,
    pub font: u32,
    pub window: u32,
    pub edge: u32,
    pub style: u32,
    pub scale: f32,
    pub enabled: bool,
}

// Twenty-four bytes of fields and a flag in the twenty-fifth, padded to the
// four-byte alignment of the lot: what C++ lays out for the same struct.
const _: () = assert!(size_of::<ClosedCaptionProperties>() == 28);
const _: () = assert!(align_of::<ClosedCaptionProperties>() == 4);

/// What [`captions_enabled`] was last told; the overlay shows nothing here
/// either way, so this only has to round-trip through [`caption_properties`].
static CAPTIONS: AtomicBool = AtomicBool::new(false);

/// Off, with the default colors and scale a console that was never asked
/// about captions reports.
const CAPTIONS_OFF: ClosedCaptionProperties = ClosedCaptionProperties {
    background: 0,
    font: 0,
    window: 0,
    edge: 0,
    style: 0,
    scale: 1.0,
    enabled: false,
};

extern "system" fn caption_properties(_: This, out: *mut ClosedCaptionProperties) -> HRESULT {
    let properties = ClosedCaptionProperties {
        enabled: CAPTIONS.load(Ordering::Relaxed),
        ..CAPTIONS_OFF
    };
    put(out, properties)
}
/// The title tells us captions are on; there is nothing to show them in.
extern "system" fn captions_enabled(_: This, enabled: bool) -> HRESULT {
    CAPTIONS.store(enabled, Ordering::Relaxed);
    tracing::debug!(enabled, "closed captions");
    S_OK
}

/// `XHighContrastMode::Off`.
extern "system" fn high_contrast(_: This, out: *mut u32) -> HRESULT {
    put(out, 0u32)
}

/// No voice is installed, so the callback never runs.
extern "system" fn enumerate_voices(
    _: This,
    _context: *mut c_void,
    _callback: *const c_void,
) -> HRESULT {
    S_OK
}
extern "system" fn create(_: This, out: *mut *const u8) -> HRESULT {
    put(out, &raw const SYNTHESIZER)
}
extern "system" fn close(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_default_voice(_: This, _: *const u8) -> HRESULT {
    S_OK
}
extern "system" fn set_custom_voice(_: This, _: *const u8, _voice: *const c_char) -> HRESULT {
    S_OK
}
extern "system" fn create_stream(
    _: This,
    _: *const u8,
    words: *const c_char,
    out: *mut *const u8,
) -> HRESULT {
    tracing::debug!(text = text(words).unwrap_or_default(), "speech (silent)");
    put(out, &raw const STREAM)
}
extern "system" fn stream_size(_: This, _: *const u8, out: *mut usize) -> HRESULT {
    put(out, SILENCE.len())
}
extern "system" fn stream_data(
    _: This,
    _: *const u8,
    size: usize,
    buffer: *mut u8,
    used: *mut usize,
) -> HRESULT {
    if buffer.is_null() || size < SILENCE.len() {
        return E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe { buffer.copy_from_nonoverlapping(SILENCE.as_ptr(), SILENCE.len()) };
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(SILENCE.len()) };
    }
    S_OK
}

/// Hypothesis ids: handed out one per begin, never zero, never reused.
static HYPOTHESIS: AtomicU32 = AtomicU32::new(1);

/// The overlay sits wherever the title says; nothing is drawn on it.
extern "system" fn position_hint(_: This, position: u32) -> HRESULT {
    tracing::debug!(position, "speech to text position (nothing to show)");
    S_OK
}
extern "system" fn send_string(
    _: This,
    speaker: *const c_char,
    content: *const c_char,
    kind: u32,
) -> HRESULT {
    tracing::debug!(
        speaker = text(speaker).unwrap_or_default(),
        content = text(content).unwrap_or_default(),
        kind,
        "speech to text (nothing to show)"
    );
    S_OK
}
extern "system" fn begin_hypothesis(
    _: This,
    speaker: *const c_char,
    content: *const c_char,
    kind: u32,
    out: *mut u32,
) -> HRESULT {
    tracing::debug!(
        speaker = text(speaker).unwrap_or_default(),
        content = text(content).unwrap_or_default(),
        kind,
        "speech to text hypothesis begins (nothing to show)"
    );
    if out.is_null() {
        return E_INVALIDARG;
    }
    put(out, HYPOTHESIS.fetch_add(1, Ordering::Relaxed))
}
extern "system" fn update_hypothesis(_: This, id: u32, content: *const c_char) -> HRESULT {
    tracing::debug!(
        id,
        content = text(content).unwrap_or_default(),
        "speech to text hypothesis updated (nothing to show)"
    );
    S_OK
}
extern "system" fn finalize_hypothesis(_: This, id: u32, content: *const c_char) -> HRESULT {
    tracing::debug!(
        id,
        content = text(content).unwrap_or_default(),
        "speech to text hypothesis finalized (nothing to show)"
    );
    S_OK
}
extern "system" fn cancel_hypothesis(_: This, id: u32) -> HRESULT {
    tracing::debug!(id, "speech to text hypothesis cancelled");
    S_OK
}

static VTABLE: Methods<22> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    caption_properties as *const (),  // 0x18 closed captions
    captions_enabled as *const (),    // 0x20
    high_contrast as *const (),       // 0x28
    position_hint as *const (),       // 0x30 speech to text
    send_string as *const (),         // 0x38
    enumerate_voices as *const (),    // 0x40
    create as *const (),              // 0x48
    close as *const (),               // 0x50
    set_default_voice as *const (),   // 0x58
    set_custom_voice as *const (),    // 0x60
    create_stream as *const (),       // 0x68 from text
    close as *const (),               // 0x70 a stream
    stream_size as *const (),         // 0x78
    stream_data as *const (),         // 0x80
    begin_hypothesis as *const (),    // 0x88 speech to text
    update_hypothesis as *const (),   // 0x90
    finalize_hypothesis as *const (), // 0x98
    cancel_hypothesis as *const (),   // 0xa0
    create_stream as *const (),       // 0xa8 from SSML
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xgameruntime::method;

    #[test]
    fn streams_are_empty_wave_files() {
        let this: This = &OBJECT;
        type Create = extern "system" fn(This, *mut *const u8) -> HRESULT;
        type Stream = extern "system" fn(This, *const u8, *const c_char, *mut *const u8) -> HRESULT;
        type Size = extern "system" fn(This, *const u8, *mut usize) -> HRESULT;
        type Data = extern "system" fn(This, *const u8, usize, *mut u8, *mut usize) -> HRESULT;
        let (mut synth, mut stream, mut size) = (std::ptr::null(), std::ptr::null(), 0);
        assert_eq!(method::<Create>(&OBJECT, 0x48)(this, &mut synth), S_OK);
        let say = method::<Stream>(&OBJECT, 0x68);
        assert_eq!(say(this, synth, c"Hello".as_ptr(), &mut stream), S_OK);
        assert_eq!(method::<Size>(&OBJECT, 0x78)(this, stream, &mut size), S_OK);
        let mut wav = vec![0u8; size];
        let mut used = 0;
        let data = method::<Data>(&OBJECT, 0x80);
        assert_eq!(data(this, stream, size, wav.as_mut_ptr(), &mut used), S_OK);
        assert_eq!(
            (&wav[..4], &wav[8..16], &wav[36..40]),
            (&b"RIFF"[..], &b"WAVEfmt "[..], &b"data"[..])
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(used, 44);
    }

    /// The properties are the defaults `XAccessibility.h` describes, and the
    /// flag is the only part of them the title can change.
    #[test]
    fn captions_are_off_with_the_default_colors() {
        let this: This = &OBJECT;
        type Get = extern "system" fn(This, *mut ClosedCaptionProperties) -> HRESULT;
        type SetEnabled = extern "system" fn(This, bool) -> HRESULT;
        let get = method::<Get>(&OBJECT, 0x18);
        let set = method::<SetEnabled>(&OBJECT, 0x20);

        // Three colors, two enums, the scale, then the flag and its padding.
        assert_eq!(size_of::<ClosedCaptionProperties>(), 28);
        assert_eq!(align_of::<ClosedCaptionProperties>(), 4);
        assert_eq!(std::mem::offset_of!(ClosedCaptionProperties, scale), 20);
        assert_eq!(std::mem::offset_of!(ClosedCaptionProperties, enabled), 24);

        assert_eq!(get(this, std::ptr::null_mut()), E_INVALIDARG);
        // Values no default equals, so the call has to write through.
        let mut properties = ClosedCaptionProperties {
            background: 0xffff_ffff,
            font: 0xffff_ffff,
            window: 0xffff_ffff,
            edge: 0xffff_ffff,
            style: 0xffff_ffff,
            scale: 0.0,
            enabled: true,
        };
        assert_eq!(get(this, &mut properties), S_OK);
        assert_eq!(properties, CAPTIONS_OFF, "zeroed colors, scale 1.0");
        assert_eq!(properties.scale, 1.0);

        assert_eq!(set(this, true), S_OK);
        assert_eq!(get(this, &mut properties), S_OK);
        assert!(properties.enabled, "the flag the title set comes back");
        assert_eq!(set(this, false), S_OK);
        assert_eq!(get(this, &mut properties), S_OK);
        assert!(!properties.enabled, "and setting it back clears it again");
    }

    /// `XHighContrastMode::Off`, which is all a console without a theme has.
    #[test]
    fn high_contrast_is_off() {
        let this: This = &OBJECT;
        type Mode = extern "system" fn(This, *mut u32) -> HRESULT;
        let mode = method::<Mode>(&OBJECT, 0x28);
        assert_eq!(mode(this, std::ptr::null_mut()), E_INVALIDARG);
        let mut contrast = u32::MAX;
        assert_eq!(mode(this, &mut contrast), S_OK);
        assert_eq!(contrast, 0, "XHighContrastMode::Off");
    }

    /// Every speech-to-text call is accepted; none of them draws anything.
    #[test]
    fn speech_to_text_takes_its_strings_and_hands_out_ids() {
        let this: This = &OBJECT;
        type Position = extern "system" fn(This, u32) -> HRESULT;
        type Send = extern "system" fn(This, *const c_char, *const c_char, u32) -> HRESULT;
        type Begin =
            extern "system" fn(This, *const c_char, *const c_char, u32, *mut u32) -> HRESULT;
        type String = extern "system" fn(This, u32, *const c_char) -> HRESULT;
        type Cancel = extern "system" fn(This, u32) -> HRESULT;

        let (speaker, content) = (c"Ada".as_ptr(), c"all clear".as_ptr());
        assert_eq!(method::<Position>(&OBJECT, 0x30)(this, 1), S_OK);
        assert_eq!(
            method::<Send>(&OBJECT, 0x38)(this, speaker, content, 0),
            S_OK
        );
        // Null strings are logged as empty rather than read through.
        assert_eq!(
            method::<Send>(&OBJECT, 0x38)(this, std::ptr::null(), std::ptr::null(), 0),
            S_OK
        );

        let begin = method::<Begin>(&OBJECT, 0x88);
        assert_eq!(
            begin(this, speaker, content, 0, std::ptr::null_mut()),
            E_INVALIDARG
        );
        let (mut first, mut second) = (0, 0);
        assert_eq!(begin(this, speaker, content, 0, &mut first), S_OK);
        assert_ne!(first, 0, "an id the title can keep");
        assert_eq!(begin(this, speaker, content, 0, &mut second), S_OK);
        assert!(second > first, "one id per hypothesis, never reused");

        // Update and finalize take the same id and a string, cancel just the id.
        assert_eq!(method::<String>(&OBJECT, 0x90)(this, first, content), S_OK);
        assert_eq!(
            method::<String>(&OBJECT, 0x90)(this, first, std::ptr::null()),
            S_OK
        );
        assert_eq!(method::<String>(&OBJECT, 0x98)(this, first, content), S_OK);
        assert_eq!(method::<Cancel>(&OBJECT, 0xa0)(this, second), S_OK);
    }
}
