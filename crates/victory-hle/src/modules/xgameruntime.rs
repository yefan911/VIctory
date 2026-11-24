//! `xgameruntime.dll`: the console game runtime (XGame*, XUser*, XTaskQueue*...).
//!
//! Titles rarely import it. A stub linked into the title loads the DLL with
//! `LoadLibraryExA` (answered with a pseudo handle, see `kernel`), looks up
//! the `*ApiImpl` functions and `XErrorReport` with `GetProcAddress`, and
//! initialises. Each runtime API call then asks `QueryApiImpl` for a COM-style
//! object of its area (provider and interface GUIDs), calls one method and
//! releases it. Until an area is implemented its query fails, and the call
//! returns that error without touching the object. An area's interface id
//! changes when its method table does, so each object answers only the ids
//! whose slots we know. Implemented, as an offline retail console with one
//! signed-in user: task queues and async blocks (`xasync`), the user
//! (`xuser`), system information (`xsystem`), title id, feature checks,
//! invites and events (`xgame`), persistent local storage (`storage`), game
//! saves (`xgamesave`), networking (`xnetworking`), the package
//! (`xpackage`), the store (`xstore`), system dialogs (`xgameui`), speech
//! (`speech`), HDR, URIs, display deferrals and screenshots (`display`).
#![allow(non_snake_case)]
// Title code calls these through its import table, passing pointers the real
// API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use super::{
    display, speech, storage, xasync, xgame, xgamesave, xgameui, xnetworking, xpackage, xstore,
    xsystem, xuser,
};
use crate::{HleModule, hle_export};

pub type HRESULT = i32;
pub const S_OK: HRESULT = 0;
/// `HRESULT_FROM_WIN32(ERROR_NOT_SUPPORTED)`.
pub const NOT_SUPPORTED: HRESULT = 0x8007_0032_u32 as i32;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Guid {
        Guid {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

impl std::fmt::Display for Guid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let d = self.data4;
        write!(
            f,
            "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            self.data1, self.data2, self.data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
}

/// A runtime object as title code calls it: its first word points at the
/// method table. Ours are static, so reference counting is a no-op.
#[repr(C)]
pub struct Object {
    vtable: *const *const (),
    /// Named in the log when a title calls a method we lack.
    area: &'static str,
}

// SAFETY: the table is a static array of function addresses.
unsafe impl Sync for Object {}

impl Object {
    pub const fn new<const N: usize>(area: &'static str, methods: &'static Methods<N>) -> Object {
        Object {
            vtable: methods.0.as_ptr(),
            area,
        }
    }
}

/// An object's method table, by slot (`QueryInterface`, `AddRef`,
/// `Release`, then the area's methods from offset 0x18).
pub struct Methods<const N: usize>(pub [*const (); N]);

// SAFETY: function addresses only.
unsafe impl<const N: usize> Sync for Methods<N> {}

/// A (provider, interface id) pair a title may ask for, and our object.
pub type Interface = (Guid, Guid, &'static Object);

const AREAS: &[&[Interface]] = &[
    xasync::INTERFACES,
    xuser::INTERFACES,
    xsystem::INTERFACES,
    xgame::INTERFACES,
    storage::INTERFACES,
    xnetworking::INTERFACES,
    xpackage::INTERFACES,
    display::INTERFACES,
    xgamesave::INTERFACES,
    xstore::INTERFACES,
    xgameui::INTERFACES,
    speech::INTERFACES,
];

/// `AddRef` and `Release`.
pub extern "system" fn add_ref(_: *const Object) -> u32 {
    1
}

/// A slot nobody has needed yet; logged with its area. Wrong for a slot
/// that returns `bool` or a value.
pub extern "system" fn missing<const SLOT: usize>(this: *const Object) -> HRESULT {
    // SAFETY: title code calls methods on objects we handed out.
    let area = unsafe { (*this).area };
    tracing::warn!(
        area,
        slot = format_args!("{:#x}", SLOT * 8),
        "runtime method not implemented"
    );
    xasync::E_NOTIMPL
}

/// Writes `value` through `out` if it is non-null.
pub fn put<T>(out: *mut T, value: T) -> HRESULT {
    if out.is_null() {
        return xasync::E_INVALIDARG;
    }
    // SAFETY: checked non-null; the title passes room for a `T`.
    unsafe { out.write(value) };
    S_OK
}

/// Copies `text` and a NUL into the title's `size`-byte buffer and reports
/// the bytes used, as the runtime's string getters do. A buffer that does
/// not fit is left untouched.
pub fn put_string(text: &str, size: usize, buffer: *mut u8, used: *mut usize) -> HRESULT {
    let len = text.len() + 1;
    if buffer.is_null() || size < len {
        // String getters are called twice: once with no buffer to learn the
        // required size, then with that many bytes. Report it even on the
        // insufficient-buffer branch so the first call can be used.
        if !used.is_null() {
            // SAFETY: checked non-null.
            unsafe { used.write(len) };
        }
        return xasync::E_NOT_SUFFICIENT_BUFFER;
    }
    // SAFETY: the buffer holds `size` bytes, checked above.
    unsafe {
        buffer.copy_from_nonoverlapping(text.as_ptr(), text.len());
        buffer.add(text.len()).write(0);
    }
    if !used.is_null() {
        // SAFETY: checked non-null.
        unsafe { used.write(len) };
    }
    S_OK
}

/// The title's NUL-terminated string, if any.
pub fn text(s: *const c_char) -> Option<String> {
    // SAFETY: the title passes a NUL-terminated string or null.
    (!s.is_null()).then(|| unsafe { CStr::from_ptr(s) }.to_string_lossy().into_owned())
}

/// A new registration token for callbacks we never call (events nobody
/// raises offline).
pub fn token() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Calls method `slot` (byte offset) of `object` as title code would.
#[cfg(test)]
pub(crate) fn method<F: Copy>(object: &Object, slot: usize) -> F {
    // SAFETY: the caller names the slot's signature; tables cover the slots
    // tests use.
    unsafe { std::mem::transmute_copy(&*object.vtable.add(slot / 8)) }
}

pub extern "system" fn XGameRuntimeInitialize() -> HRESULT {
    tracing::info!("XGameRuntimeInitialize");
    S_OK
}

pub extern "system" fn XGameRuntimeUninitialize() {
    tracing::info!("XGameRuntimeUninitialize");
}

/// The arguments identify the title's build of the stub; not checked.
pub extern "system" fn InitializeApiImpl(version: u64, build: u64) -> HRESULT {
    tracing::info!(version, build, "InitializeApiImpl");
    S_OK
}

pub extern "system" fn InitializeApiImplEx(version: u64, build: u64, flags: u64) -> HRESULT {
    tracing::info!(version, build, flags, "InitializeApiImplEx");
    S_OK
}

/// The stub prefers this one when present. It passes the same three values
/// and a pointer to the title's initialisation options, null unless the
/// title initialises with options (Grand Theft Auto VI does not).
pub extern "system" fn InitializeApiImplEx2(
    version: u64,
    build: u64,
    flags: u64,
    options: *const c_void,
) -> HRESULT {
    tracing::info!(version, build, flags, ?options, "InitializeApiImplEx2");
    S_OK
}

pub extern "system" fn UninitializeApiImpl() {
    tracing::info!("UninitializeApiImpl");
}

/// The object for an implemented (provider, interface id); others fail,
/// each pair logged once. A null `out` is the title's own error, not a
/// missing area, and is neither logged nor remembered as one.
pub extern "system" fn QueryApiImpl(
    provider: *const Guid,
    iid: *const Guid,
    out: *mut *mut c_void,
) -> HRESULT {
    static QUERIED: Mutex<Vec<(Guid, Guid)>> = Mutex::new(Vec::new());
    // SAFETY: the title passes pointers to GUIDs (null is tolerated).
    let (Some(&provider), Some(&iid)) = (unsafe { provider.as_ref() }, unsafe { iid.as_ref() })
    else {
        return NOT_SUPPORTED;
    };
    if out.is_null() {
        return xasync::E_INVALIDARG;
    }
    #[cfg(feature = "diagnostics")]
    if tracing::enabled!(tracing::Level::DEBUG) {
        // Each runtime call site once: which title code uses which area, when.
        static CALLS: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let from = super::kernel::callers();
        let mut calls = CALLS.lock().unwrap_or_else(|e| e.into_inner());
        if !calls.contains(&from) {
            tracing::debug!(%provider, %iid, %from, "runtime call");
            calls.push(from);
        }
    }
    let found = AREAS
        .iter()
        .flat_map(|area| area.iter())
        .find(|(p, i, _)| (*p, *i) == (provider, iid));
    if let Some(&(_, _, object)) = found {
        // SAFETY: checked non-null; the title passes room for a pointer.
        unsafe { out.write((object as *const Object).cast_mut().cast()) };
        return S_OK;
    }
    let mut queried = QUERIED.lock().unwrap_or_else(|e| e.into_inner());
    if !queried.contains(&(provider, iid)) {
        queried.push((provider, iid));
        tracing::warn!(%provider, %iid, "runtime API area not implemented");
    }
    NOT_SUPPORTED
}

pub extern "system" fn XErrorReport(hr: HRESULT, message: *const c_char) {
    let message = if message.is_null() {
        "".into()
    } else {
        // SAFETY: a non-null message is a NUL-terminated string.
        unsafe { CStr::from_ptr(message) }.to_string_lossy()
    };
    tracing::warn!(hr = format_args!("{:#010x}", hr as u32), %message, "XErrorReport");
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xgameruntime",
        dlls: &["xgameruntime.dll"],
        exports: vec![
            hle_export!(XGameRuntimeInitialize),
            hle_export!(XGameRuntimeUninitialize),
            hle_export!(InitializeApiImpl),
            hle_export!(InitializeApiImplEx),
            hle_export!(InitializeApiImplEx2),
            hle_export!(UninitializeApiImpl),
            hle_export!(QueryApiImpl),
            hle_export!(XErrorReport),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::kernel::{FreeLibrary, GetProcAddress, LoadLibraryExA};

    fn first_interface() -> Interface {
        AREAS[0][0]
    }

    #[test]
    fn the_error_codes_are_the_documented_ones() {
        assert_eq!(S_OK, 0);
        assert_eq!(NOT_SUPPORTED, 0x8007_0032_u32 as i32);
        assert_eq!(xasync::E_NOTIMPL, 0x8000_4001_u32 as i32);
        assert_eq!(xasync::E_INVALIDARG, 0x8007_0057_u32 as i32);
        assert_eq!(xasync::E_NOT_SUFFICIENT_BUFFER, 0x8007_007A_u32 as i32);
    }

    #[test]
    fn a_guid_prints_the_canonical_form() {
        let guid = Guid::new(
            0x0123_4567,
            0x89ab,
            0xcdef,
            [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef],
        );
        assert_eq!(guid.to_string(), "01234567-89ab-cdef-0123-456789abcdef");
        // Copy and equality: the table lookup compares pairs by value.
        assert_eq!(
            guid,
            Guid::new(guid.data1, guid.data2, guid.data3, guid.data4)
        );
        assert_ne!(guid, Guid::new(0, 0, 0, [0; 8]));
    }

    #[test]
    fn put_writes_through_a_valid_pointer_only() {
        let mut count = 0xdead_beef_u32;
        assert_eq!(put(&mut count, 7_u32), S_OK);
        assert_eq!(count, 7);
        let mut flag = false;
        assert_eq!(put(&mut flag, true), S_OK);
        assert!(flag);
        let mut wide = 0_usize;
        assert_eq!(put(&mut wide, usize::MAX), S_OK);
        assert_eq!(wide, usize::MAX);
        assert_eq!(put::<u32>(std::ptr::null_mut(), 1), xasync::E_INVALIDARG);
    }

    #[test]
    fn put_string_copies_the_text_and_its_nul() {
        let mut buffer = [0xaa_u8; 8];
        let mut used = 0;
        assert_eq!(put_string("abc", 4, buffer.as_mut_ptr(), &mut used), S_OK);
        assert_eq!((used, &buffer[..5]), (4, &[b'a', b'b', b'c', 0, 0xaa][..]));
        // An empty string still needs room for its NUL.
        assert_eq!(put_string("", 1, buffer.as_mut_ptr(), &mut used), S_OK);
        assert_eq!((used, buffer[0]), (1, 0));
        // The buffer's size, not the string's, decides what is written.
        assert_eq!(
            put_string("abc", 8, buffer.as_mut_ptr(), std::ptr::null_mut()),
            S_OK
        );
        assert_eq!(&buffer[..4], b"abc\0");
    }

    #[test]
    fn put_string_refuses_what_it_cannot_fit() {
        let mut buffer = [0xaa_u8; 4];
        let pointer = buffer.as_mut_ptr();
        let mut used = 0xdead_beef_usize;
        for size in [3, 0] {
            assert_eq!(
                put_string("abc", size, pointer, &mut used),
                xasync::E_NOT_SUFFICIENT_BUFFER,
                "size {size}"
            );
        }
        assert_eq!(
            put_string("abc", 4, std::ptr::null_mut(), &mut used),
            xasync::E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(
            put_string("abc", usize::MAX, std::ptr::null_mut(), &mut used),
            xasync::E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(buffer, [0xaa; 4], "nothing is written on failure");
        assert_eq!(used, 0xdead_beef, "and no size is reported");
    }

    #[test]
    fn text_reads_the_titles_strings() {
        assert_eq!(text(std::ptr::null()), None);
        assert_eq!(text(c"".as_ptr()).as_deref(), Some(""));
        assert_eq!(text(c"Player".as_ptr()).as_deref(), Some("Player"));
        // Not UTF-8: the bytes become replacement characters, never a panic.
        assert_eq!(text(c"a\xffb".as_ptr()).as_deref(), Some("a\u{fffd}b"));
    }

    #[test]
    fn tokens_are_nonzero_and_distinct() {
        let first = token();
        let second = token();
        assert!(first >= 1 && second > first);
        let mut seen: Vec<u64> = (0..64).map(|_| token()).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 64);
        assert!(!seen.contains(&0));
    }

    #[test]
    fn every_registered_interface_answers_with_its_object() {
        let mut pairs = Vec::new();
        for area in AREAS {
            for &(provider, iid, object) in *area {
                let mut out: *mut c_void = std::ptr::null_mut();
                assert_eq!(
                    QueryApiImpl(&provider, &iid, &mut out),
                    S_OK,
                    "{provider} {iid}"
                );
                assert_eq!(
                    out as *const Object, object as *const Object,
                    "{provider} {iid}"
                );
                assert!(!object.area.is_empty());
                pairs.push((provider.to_string(), iid.to_string()));
            }
        }
        // A pair listed twice would leave one object unreachable.
        let count = pairs.len();
        pairs.sort_unstable();
        pairs.dedup();
        assert_eq!(pairs.len(), count);
    }

    #[test]
    fn an_unimplemented_area_is_unsupported_and_writes_nothing() {
        let unknown = Guid::new(0xdead_beef, 0x1234, 0x5678, [0; 8]);
        let untouched = 0x1234 as *mut c_void;
        let mut out = untouched;
        assert_eq!(QueryApiImpl(&unknown, &unknown, &mut out), NOT_SUPPORTED);
        // Again, to exercise the once-only log path.
        assert_eq!(QueryApiImpl(&unknown, &unknown, &mut out), NOT_SUPPORTED);
        let (provider, _, _) = first_interface();
        assert_eq!(QueryApiImpl(&provider, &unknown, &mut out), NOT_SUPPORTED);
        assert_eq!(QueryApiImpl(&unknown, &provider, &mut out), NOT_SUPPORTED);
        assert_eq!(out, untouched, "the output is not written on failure");
        // Null GUID pointers are tolerated, never dereferenced.
        assert_eq!(
            QueryApiImpl(std::ptr::null(), std::ptr::null(), &mut out),
            NOT_SUPPORTED
        );
        assert_eq!(
            QueryApiImpl(std::ptr::null(), &unknown, &mut out),
            NOT_SUPPORTED
        );
        assert_eq!(
            QueryApiImpl(&unknown, std::ptr::null(), &mut out),
            NOT_SUPPORTED
        );
        assert_eq!(out, untouched);
    }

    #[test]
    fn a_null_output_is_the_titles_argument_error() {
        let (provider, iid, object) = first_interface();
        assert_eq!(
            QueryApiImpl(&provider, &iid, std::ptr::null_mut()),
            xasync::E_INVALIDARG,
            "an implemented area with nowhere to put its object"
        );
        // Argument validation comes first, whatever the pair names: a null
        // output is not a missing area and must not be remembered as one.
        let unknown = Guid::new(0xdead_beef, 0x1234, 0x5678, [0; 8]);
        assert_eq!(
            QueryApiImpl(&unknown, &unknown, std::ptr::null_mut()),
            xasync::E_INVALIDARG
        );
        // The same query still answers once there is room for the object.
        let mut out: *mut c_void = std::ptr::null_mut();
        assert_eq!(QueryApiImpl(&provider, &iid, &mut out), S_OK);
        assert_eq!(out as *const Object, object as *const Object);
    }

    extern "system" fn answer(_: *const Object) -> u32 {
        7
    }

    static TEST_METHODS: Methods<4> = Methods([
        missing::<0> as *const (),
        add_ref as *const (),
        answer as *const (),
        missing::<3> as *const (),
    ]);
    static TEST_OBJECT: Object = Object::new("test", &TEST_METHODS);

    #[test]
    fn an_object_points_at_its_method_table() {
        assert_eq!(TEST_OBJECT.vtable, TEST_METHODS.0.as_ptr());
        assert_eq!(TEST_OBJECT.area, "test");
        type Slot = extern "system" fn(*const Object) -> u32;
        assert_eq!(method::<Slot>(&TEST_OBJECT, 0x08)(&TEST_OBJECT), 1);
        assert_eq!(method::<Slot>(&TEST_OBJECT, 0x10)(&TEST_OBJECT), 7);
        // Reference counting is a no-op: the tables are static, so a null
        // handle is harmless too.
        assert_eq!(add_ref(std::ptr::null()), 1);
        let not_implemented = xasync::E_NOTIMPL as u32;
        assert_eq!(
            method::<Slot>(&TEST_OBJECT, 0x00)(&TEST_OBJECT),
            not_implemented
        );
        assert_eq!(
            method::<Slot>(&TEST_OBJECT, 0x18)(&TEST_OBJECT),
            not_implemented
        );
    }

    #[test]
    fn the_runtime_initialises_and_uninitialises() {
        assert_eq!(XGameRuntimeInitialize(), S_OK);
        assert_eq!(InitializeApiImpl(1, 2), S_OK);
        assert_eq!(InitializeApiImplEx(1, 2, 3), S_OK);
        assert_eq!(InitializeApiImplEx2(1, 2, 3, std::ptr::null()), S_OK);
        let options = [0_u8; 16];
        assert_eq!(InitializeApiImplEx2(0, 0, 0, options.as_ptr().cast()), S_OK);
        UninitializeApiImpl();
        XGameRuntimeUninitialize();
        // Reporting an error is a log: a null message is fine.
        XErrorReport(S_OK, std::ptr::null());
        XErrorReport(xasync::E_INVALIDARG, c"bad argument".as_ptr());
    }

    #[test]
    fn the_module_serves_the_runtime_dll() {
        let module = module();
        assert_eq!(module.name, "xgameruntime");
        assert!(module.serves("XGAMERUNTIME.DLL"));
        assert!(!module.serves("xgame.dll"));
        let names: Vec<_> = module.exports.iter().map(|export| export.name).collect();
        assert_eq!(
            names,
            [
                "XGameRuntimeInitialize",
                "XGameRuntimeUninitialize",
                "InitializeApiImpl",
                "InitializeApiImplEx",
                "InitializeApiImplEx2",
                "UninitializeApiImpl",
                "QueryApiImpl",
                "XErrorReport",
            ]
        );
        assert!(module.exports.iter().all(|export| export.address != 0));
    }

    #[test]
    fn title_stub_loads_the_runtime_dynamically() {
        let load = |name: &CStr| LoadLibraryExA(name.as_ptr().cast(), std::ptr::null_mut(), 0x800);
        let runtime = load(c"xgameruntime.dll");
        assert!(!runtime.is_null());
        let proc = |name: &CStr| GetProcAddress(runtime, name.as_ptr().cast());
        for name in [
            c"InitializeApiImpl",
            c"InitializeApiImplEx",
            c"InitializeApiImplEx2",
            c"QueryApiImpl",
            c"UninitializeApiImpl",
            c"XErrorReport",
        ] {
            assert!(!proc(name).is_null(), "{name:?}");
        }
        assert!(proc(c"NotAnExport").is_null());
        let ordinal = std::ptr::without_provenance::<u8>(1);
        assert!(GetProcAddress(runtime, ordinal).is_null());

        type Init = extern "system" fn(u64, u64, u64) -> HRESULT;
        type Query = extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> HRESULT;
        // SAFETY: the addresses are our functions with these signatures.
        let (init, query) = unsafe {
            (
                std::mem::transmute::<*mut c_void, Init>(proc(c"InitializeApiImplEx")),
                std::mem::transmute::<*mut c_void, Query>(proc(c"QueryApiImpl")),
            )
        };
        assert_eq!(init(1, 2, 2), S_OK);
        let guid = Guid {
            data1: 1,
            data2: 2,
            data3: 3,
            data4: [4; 8],
        };
        let untouched = 0x1234 as *mut c_void;
        let mut out = untouched;
        assert!(query(&guid, &guid, &mut out) < 0);
        assert_eq!(out, untouched);
        assert_eq!(FreeLibrary(runtime), 1);

        #[cfg(windows)]
        {
            let kernel32 = load(c"kernel32.dll");
            assert!(!kernel32.is_null(), "host DLLs still load from the host");
            assert!(!GetProcAddress(kernel32, c"GetTickCount".as_ptr().cast()).is_null());
        }
    }

    #[test]
    fn string_getters_report_the_required_size() {
        let none = std::ptr::null_mut();
        let mut used = usize::MAX;
        assert_eq!(
            put_string("gamertag", 0, none, &mut used),
            xasync::E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(used, 9, "the NUL is part of the required size");

        // The two-call pattern: query, then fetch exactly that many bytes.
        let mut buffer = vec![0xffu8; used];
        assert_eq!(
            put_string("gamertag", buffer.len(), buffer.as_mut_ptr(), &mut used),
            S_OK
        );
        assert_eq!(buffer, b"gamertag\0");
        assert_eq!(used, 9);

        // A short buffer reports the size too, and writes nothing.
        let mut short = [0xffu8; 3];
        used = 0;
        assert_eq!(
            put_string("gamertag", short.len(), short.as_mut_ptr(), &mut used),
            xasync::E_NOT_SUFFICIENT_BUFFER
        );
        assert_eq!(used, 9);
        assert_eq!(short, [0xff; 3]);

        // Callers that do not ask for the size are still supported.
        assert_eq!(
            put_string("gamertag", 0, none, std::ptr::null_mut()),
            xasync::E_NOT_SUFFICIENT_BUFFER
        );
        let mut empty = [0xffu8; 1];
        assert_eq!(
            put_string("", empty.len(), empty.as_mut_ptr(), std::ptr::null_mut()),
            S_OK
        );
        assert_eq!(empty, [0]);
    }
}
