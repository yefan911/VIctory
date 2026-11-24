//! XNetworking as an offline console answers it: the network stack is up
//! (titles wait for that before anything else) but reaches nothing, the
//! connectivity hint never changes, and the title's security allow list,
//! which comes from the service, cannot be fetched.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{c_char, c_void};

use super::xasync::{self, AsyncBlock};
use super::xgameruntime::{
    Guid, HRESULT, Interface, Methods, Object, add_ref, missing, put, text, token,
};

pub const PROVIDER: Guid = Guid::new(
    0x37e5_6907,
    0x2f10,
    0x41e8,
    [0xb7, 0x2f, 0x36, 0xed, 0xb1, 0x85, 0x33, 0x1a],
);

/// As Grand Theft Auto VI (2026) asks for it; the slots it calls (0x18-0x88)
/// take the same arguments as under the provider's own id (older titles).
pub const IID_2026: Guid = Guid::new(
    0xbf23_46b2,
    0x39af,
    0x4658,
    [0xb5, 0xea, 0x44, 0x71, 0x3c, 0x7e, 0x83, 0xb3],
);

pub static OBJECT: Object = Object::new("networking", &VTABLE);
pub const INTERFACES: &[Interface] =
    &[(PROVIDER, PROVIDER, &OBJECT), (PROVIDER, IID_2026, &OBJECT)];

/// `HRESULT_FROM_WIN32(ERROR_NETWORK_UNREACHABLE)`: what needs the service.
pub const UNREACHABLE: HRESULT = 0x8007_04CF_u32 as i32;

/// The documented default for the preferred local UDP multiplayer port.
const UDP_PORT: u16 = 3074;

/// The connectivity hint: level, cost, interface type, then four flags.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectivityHint {
    pub level: u32,
    pub cost: u32,
    pub interface_type: u32,
    pub initialized: bool,
    pub approaching_data_limit: bool,
    pub over_data_limit: bool,
    pub roaming: bool,
}

/// Initialized, no connectivity (level 1), cost unknown (0).
const OFFLINE: ConnectivityHint = ConnectivityHint {
    level: 1,
    cost: 0,
    interface_type: 0,
    initialized: true,
    approaching_data_limit: false,
    over_data_limit: false,
    roaming: false,
};

type This = *const Object;

extern "system" fn udp_port(_: This, out: *mut u16) -> HRESULT {
    put(out, UDP_PORT)
}

/// Tells our port queries apart from other calls on the same block.
static PORT: u8 = 0;

extern "system" fn udp_port_async(_: This, block: *mut AsyncBlock) -> HRESULT {
    xasync::finished_with(block, (&raw const PORT).cast(), Ok(xasync::bytes(UDP_PORT)))
}
extern "system" fn udp_port_result(_: This, block: *mut AsyncBlock, out: *mut u16) -> HRESULT {
    xasync::result(block, (&raw const PORT).cast(), out)
}

static SECURITY: u8 = 0;

/// The allow list comes from the service: unreachable offline.
extern "system" fn security_info_async(
    _: This,
    url: *const c_char,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(
        url = text(url).unwrap_or_default(),
        "security information: offline"
    );
    xasync::finished(block, (&raw const SECURITY).cast(), UNREACHABLE)
}
extern "system" fn security_info_size(_: This, block: *mut AsyncBlock, out: *mut usize) -> HRESULT {
    xasync::XAsyncGetResultSize(block, out)
}
extern "system" fn security_info_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    used: *mut usize,
    buffer: *mut u8,
    _info: *mut *const c_void,
) -> HRESULT {
    xasync::XAsyncGetResult(
        block,
        (&raw const SECURITY).cast(),
        size,
        buffer.cast(),
        used,
    )
}

/// A tag of its own: a UTF-16 call can never be mistaken for an ANSI one,
/// so a result begun for the one is refused by the other's result slot.
static SECURITY_UTF16: u8 = 0;

/// The title's NUL-terminated UTF-16 string, if any.
fn wide(s: *const u16) -> Option<String> {
    (!s.is_null()).then(|| {
        // SAFETY: the title passes a NUL-terminated string or null.
        unsafe {
            let len = (0..).take_while(|&i| *s.add(i) != 0).count();
            String::from_utf16_lossy(std::slice::from_raw_parts(s, len))
        }
    })
}

/// The same allow list, asked for by a UTF-16 URL: unreachable offline.
extern "system" fn security_info_utf16_async(
    _: This,
    url: *const u16,
    block: *mut AsyncBlock,
) -> HRESULT {
    tracing::debug!(
        url = wide(url).unwrap_or_default(),
        "security information (utf16): offline"
    );
    xasync::finished(block, (&raw const SECURITY_UTF16).cast(), UNREACHABLE)
}
extern "system" fn security_info_utf16_size(
    _: This,
    block: *mut AsyncBlock,
    out: *mut usize,
) -> HRESULT {
    xasync::XAsyncGetResultSize(block, out)
}
extern "system" fn security_info_utf16_result(
    _: This,
    block: *mut AsyncBlock,
    size: usize,
    used: *mut usize,
    buffer: *mut u8,
    _info: *mut *const c_void,
) -> HRESULT {
    xasync::XAsyncGetResult(
        block,
        (&raw const SECURITY_UTF16).cast(),
        size,
        buffer.cast(),
        used,
    )
}

/// No security information can exist offline, so no certificate passes.
extern "system" fn verify_certificate(
    _: This,
    _request: *mut c_void,
    _info: *const c_void,
) -> HRESULT {
    UNREACHABLE
}

extern "system" fn connectivity_hint(_: This, out: *mut ConnectivityHint) -> HRESULT {
    put(out, OFFLINE)
}

/// Nothing the service reports ever changes offline (the connectivity
/// hint no more than the preferred UDP port), so the callback never runs.
extern "system" fn register(
    _: This,
    _queue: *const c_void,
    _context: *mut c_void,
    _callback: *const c_void,
    out: *mut u64,
) -> HRESULT {
    put(out, token())
}
/// Nothing was ever raised, so any token is as good as another.
extern "system" fn unregister(_: This, _token: u64, _wait: bool) -> bool {
    true
}

static VTABLE: Methods<18> = Methods([
    missing::<0> as *const (),
    add_ref as *const (),
    add_ref as *const (),
    udp_port as *const (),                   // 0x18
    udp_port_async as *const (),             // 0x20
    udp_port_result as *const (),            // 0x28
    register as *const (),                   // 0x30 port changed (never)
    unregister as *const (),                 // 0x38
    security_info_async as *const (),        // 0x40
    security_info_size as *const (),         // 0x48
    security_info_result as *const (),       // 0x50
    security_info_utf16_async as *const (),  // 0x58 the same for UTF-16 URLs
    security_info_utf16_size as *const (),   // 0x60
    security_info_utf16_result as *const (), // 0x68
    verify_certificate as *const (),         // 0x70
    connectivity_hint as *const (),          // 0x78
    register as *const (),                   // 0x80 hint changed (never)
    unregister as *const (),                 // 0x88
]);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::xasync::E_INVALIDARG;
    use crate::modules::xgameruntime::{S_OK, method};

    fn block() -> AsyncBlock {
        AsyncBlock {
            queue: std::ptr::null(),
            context: std::ptr::null_mut(),
            callback: None,
            internal: [0; 4],
        }
    }

    #[test]
    fn offline_but_initialized() {
        let this: This = &OBJECT;
        type Hint = extern "system" fn(This, *mut ConnectivityHint) -> HRESULT;
        let mut hint = OFFLINE;
        hint.initialized = false;
        assert_eq!(method::<Hint>(&OBJECT, 0x78)(this, &mut hint), S_OK);
        assert!(hint.initialized && hint.level == 1);
        assert_eq!(size_of::<ConnectivityHint>(), 16);

        type Begin = extern "system" fn(This, *mut AsyncBlock) -> HRESULT;
        type Port = extern "system" fn(This, *mut AsyncBlock, *mut u16) -> HRESULT;
        let mut b = block();
        assert_eq!(method::<Begin>(&OBJECT, 0x20)(this, &mut b), S_OK);
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), S_OK);
        let mut port = 0;
        assert_eq!(method::<Port>(&OBJECT, 0x28)(this, &mut b, &mut port), S_OK);
        assert_eq!(port, 3074);

        type Url = extern "system" fn(This, *const c_char, *mut AsyncBlock) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut usize) -> HRESULT;
        let mut b = block();
        let url = c"https://example.invalid/";
        assert_eq!(
            method::<Url>(&OBJECT, 0x40)(this, url.as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x48)(this, &mut b, &mut size),
            UNREACHABLE
        );
    }

    /// The preferred UDP port never changes offline, but registering for it
    /// still hands out a token, as the connectivity hint's slot does.
    #[test]
    fn a_port_change_callback_registers_but_never_runs() {
        let this: This = &OBJECT;
        type Register = extern "system" fn(
            This,
            *const c_void,
            *mut c_void,
            *const c_void,
            *mut u64,
        ) -> HRESULT;
        type Unregister = extern "system" fn(This, u64, bool) -> bool;
        let (queue, context, callback) = (std::ptr::null(), std::ptr::null_mut(), std::ptr::null());

        let mut first = 0;
        assert_eq!(
            method::<Register>(&OBJECT, 0x30)(this, queue, context, callback, &mut first),
            S_OK
        );
        assert_ne!(first, 0, "a token titles can keep");
        let mut second = 0;
        assert_eq!(
            method::<Register>(&OBJECT, 0x30)(this, queue, context, callback, &mut second),
            S_OK
        );
        assert_ne!(second, first, "a token of its own per registration");
        assert_eq!(
            method::<Register>(&OBJECT, 0x30)(this, queue, context, callback, std::ptr::null_mut()),
            E_INVALIDARG
        );

        assert!(method::<Unregister>(&OBJECT, 0x38)(this, first, true));
        assert!(
            method::<Unregister>(&OBJECT, 0x38)(this, 0, false),
            "nothing was ever raised, so any token unregisters"
        );
    }

    /// The UTF-16 siblings answer as the ANSI trio does: nothing to report,
    /// and nothing to read back.
    #[test]
    fn utf16_security_information_is_unreachable_offline() {
        let this: This = &OBJECT;
        type Begin = extern "system" fn(This, *const u16, *mut AsyncBlock) -> HRESULT;
        type Size = extern "system" fn(This, *mut AsyncBlock, *mut usize) -> HRESULT;
        type Result = extern "system" fn(
            This,
            *mut AsyncBlock,
            usize,
            *mut usize,
            *mut u8,
            *mut *const c_void,
        ) -> HRESULT;

        let url: Vec<u16> = "https://example.invalid/\0".encode_utf16().collect();
        let mut b = block();
        assert_eq!(
            method::<Begin>(&OBJECT, 0x58)(this, url.as_ptr(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
        let mut size = 0;
        assert_eq!(
            method::<Size>(&OBJECT, 0x60)(this, &mut b, &mut size),
            UNREACHABLE
        );
        assert_eq!(size, 0, "a failed call has no size to report");
        let mut buffer = [0u8; 64];
        let mut used = 0;
        let mut info: *const c_void = std::ptr::null();
        assert_eq!(
            method::<Result>(&OBJECT, 0x68)(
                this,
                &mut b,
                buffer.len(),
                &mut used,
                buffer.as_mut_ptr(),
                &mut info,
            ),
            UNREACHABLE
        );
        assert_eq!(used, 0, "and no bytes were used");
        // The ANSI slot reads a failed call the same way: the status is all
        // there is, so which slot began it never comes into it.
        assert_eq!(
            method::<Result>(&OBJECT, 0x50)(
                this,
                &mut b,
                0,
                &mut used,
                std::ptr::null_mut(),
                &mut info,
            ),
            UNREACHABLE
        );
    }

    /// A null URL is logged as an empty one, not read through.
    #[test]
    fn a_null_utf16_url_completes_like_an_empty_one() {
        let this: This = &OBJECT;
        type Begin = extern "system" fn(This, *const u16, *mut AsyncBlock) -> HRESULT;
        let mut b = block();
        assert_eq!(
            method::<Begin>(&OBJECT, 0x58)(this, std::ptr::null(), &mut b),
            S_OK
        );
        assert_eq!(xasync::XAsyncGetStatus(&mut b, true), UNREACHABLE);
    }

    /// Each trio tags its calls with an identity of its own, so a result
    /// begun for one is refused by the other's result slot. Offline a call
    /// ends with an error and no result, which `XAsyncGetResult` reports
    /// before it ever looks at the tag; this stands in with a call that
    /// does carry one, so that the tags themselves are what is under test.
    #[test]
    fn utf16_and_ansi_results_are_not_interchangeable() {
        let this: This = &OBJECT;
        type Result = extern "system" fn(
            This,
            *mut AsyncBlock,
            usize,
            *mut usize,
            *mut u8,
            *mut *const c_void,
        ) -> HRESULT;

        let mut b = block();
        let identity = (&raw const SECURITY_UTF16).cast();
        assert_eq!(
            xasync::finished_with(&mut b, identity, Ok(vec![0xAB; 4])),
            S_OK
        );
        let mut info: *const c_void = std::ptr::null();
        let mut used = 0;
        let mut ansi = [0u8; 4];
        assert_eq!(
            method::<Result>(&OBJECT, 0x50)(
                this,
                &mut b,
                4,
                &mut used,
                ansi.as_mut_ptr(),
                &mut info,
            ),
            E_INVALIDARG
        );
        assert_eq!(ansi, [0; 4], "and nothing was written through it");
        let mut utf16 = [0u8; 4];
        assert_eq!(
            method::<Result>(&OBJECT, 0x68)(
                this,
                &mut b,
                4,
                &mut used,
                utf16.as_mut_ptr(),
                &mut info,
            ),
            S_OK
        );
        assert_eq!((utf16, used), ([0xAB; 4], 4));
    }
}
