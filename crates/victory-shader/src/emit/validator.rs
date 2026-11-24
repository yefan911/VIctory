//! The DXIL validator in `dxil.dll` (`IDxcValidator`), for tests and the
//! offline batch. Translated shaders do not need it: they carry the
//! container hash we compute (`container::hash`).
//!
//! The DLL is found at `VICTORY_DXIL` if set, else the newest Windows SDK's
//! `bin\<version>\x64\dxil.dll`, else wherever `LoadLibrary` finds it.

use std::ffi::c_void;
use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

type Hresult = i32;

#[repr(C)]
struct Guid(u32, u16, u16, [u8; 8]);

const CLSID_DXC_VALIDATOR: Guid = Guid(
    0x8ca3_e215,
    0xf728,
    0x4cf3,
    [0x8c, 0xdd, 0x88, 0xaf, 0x91, 0x75, 0x87, 0xa1],
);
const IID_IDXC_VALIDATOR: Guid = Guid(
    0xa6e8_2bd2,
    0x1fd7,
    0x4826,
    [0x98, 0x11, 0x28, 0x57, 0xe7, 0x97, 0xf4, 0x9a],
);
/// `DxcValidatorFlags_InPlaceEdit`: sign the container we pass.
const IN_PLACE_EDIT: u32 = 1;

type CreateInstance =
    unsafe extern "system" fn(*const Guid, *const Guid, *mut *mut c_void) -> Hresult;

/// COM object: a pointer to a table of method pointers.
type Object = *mut *const usize;

unsafe fn method<F: Copy>(obj: Object, slot: usize) -> F {
    // SAFETY: the caller names a slot of the object's interface.
    unsafe { std::mem::transmute_copy(&*(*obj).add(slot)) }
}

unsafe fn release(obj: Object) {
    if !obj.is_null() {
        // SAFETY: slot 2 of every COM interface is Release.
        unsafe { method::<unsafe extern "system" fn(Object) -> u32>(obj, 2)(obj) };
    }
}

/// An `IDxcBlob` over our bytes: QueryInterface, AddRef, Release,
/// GetBufferPointer, GetBufferSize. Lives on the caller's stack.
#[repr(C)]
struct Blob {
    vtable: *const BlobVtable,
    data: *mut u8,
    len: usize,
}

/// IUnknown and IDxcBlob, as raw GUID bytes.
const IUNKNOWN: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0xc0, 0, 0, 0, 0, 0, 0, 0x46];
const IDXC_BLOB: [u8; 16] = [
    0x08, 0xfb, 0xa5, 0x8b, 0x95, 0x51, 0xe2, 0x40, 0xac, 0x58, 0x0d, 0x98, 0x9c, 0x3a, 0x01, 0x02,
];

unsafe extern "system" fn blob_qi(
    this: *mut Blob,
    iid: *const [u8; 16],
    out: *mut *mut c_void,
) -> Hresult {
    // SAFETY: the validator passes valid pointers.
    unsafe {
        if *iid == IUNKNOWN || *iid == IDXC_BLOB {
            *out = this.cast();
            0
        } else {
            *out = std::ptr::null_mut();
            0x8000_4002_u32 as i32 // E_NOINTERFACE
        }
    }
}
unsafe extern "system" fn blob_ref(_: *mut Blob) -> u32 {
    1
}
unsafe extern "system" fn blob_ptr(this: *mut Blob) -> *mut u8 {
    // SAFETY: `this` is our blob.
    unsafe { (*this).data }
}
unsafe extern "system" fn blob_len(this: *mut Blob) -> usize {
    // SAFETY: `this` is our blob.
    unsafe { (*this).len }
}

#[repr(C)]
struct BlobVtable {
    qi: unsafe extern "system" fn(*mut Blob, *const [u8; 16], *mut *mut c_void) -> Hresult,
    add_ref: unsafe extern "system" fn(*mut Blob) -> u32,
    release: unsafe extern "system" fn(*mut Blob) -> u32,
    ptr: unsafe extern "system" fn(*mut Blob) -> *mut u8,
    len: unsafe extern "system" fn(*mut Blob) -> usize,
}

static BLOB_VTABLE: BlobVtable = BlobVtable {
    qi: blob_qi,
    add_ref: blob_ref,
    release: blob_ref,
    ptr: blob_ptr,
    len: blob_len,
};

pub struct Validator {
    object: Object,
    module: HMODULE,
}

pub(super) const SDK_BIN: &str = r"C:\Program Files (x86)\Windows Kits\10\bin";

pub(super) fn sdk_tool(kits: &Path, name: &str) -> Option<PathBuf> {
    std::fs::read_dir(kits)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("x64").join(name))
        .filter(|path| path.exists())
        .max()
}

// SAFETY: the validator object is free-threaded (DXC's validator has no
// thread affinity) and the module handle is process-wide; calls are
// serialised by `&mut self`.
unsafe impl Send for Validator {}

impl Validator {
    /// Where `dxil.dll` is looked for.
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("VICTORY_DXIL")
            .map(PathBuf::from)
            .or_else(|| sdk_tool(Path::new(SDK_BIN), "dxil.dll"))
    }

    /// Loads `path`'s validator (or `dxil.dll` from the DLL search path).
    pub fn load(path: Option<&std::path::Path>) -> Result<Self, String> {
        let name: Vec<u16> = path
            .map_or_else(|| "dxil.dll".into(), |p| p.as_os_str().to_owned())
            .to_string_lossy()
            .encode_utf16()
            .chain([0])
            .collect();
        // SAFETY: plain Win32 calls with a NUL-terminated name.
        unsafe {
            let module = LoadLibraryW(name.as_ptr());
            if module.is_null() {
                return Err(format!(
                    "cannot load {}",
                    String::from_utf16_lossy(&name[..name.len() - 1])
                ));
            }
            let Some(create) = GetProcAddress(module, c"DxcCreateInstance".as_ptr().cast()) else {
                FreeLibrary(module);
                return Err("dxil.dll lacks DxcCreateInstance".into());
            };
            let create: CreateInstance = std::mem::transmute(create);
            let mut object = std::ptr::null_mut();
            let hr = create(&CLSID_DXC_VALIDATOR, &IID_IDXC_VALIDATOR, &mut object);
            if hr < 0 || object.is_null() {
                release(object.cast());
                FreeLibrary(module);
                return Err(format!("DxcCreateInstance(validator) failed: {hr:#x}"));
            }
            Ok(Validator {
                object: object.cast(),
                module,
            })
        }
    }

    /// Validates `container` and signs it in place; the error text if it
    /// is rejected.
    pub fn validate(&mut self, container: &mut [u8]) -> Result<(), String> {
        let mut blob = Blob {
            vtable: &BLOB_VTABLE,
            data: container.as_mut_ptr(),
            len: container.len(),
        };
        // SAFETY: IDxcValidator::Validate (slot 3) with our blob; then
        // IDxcOperationResult GetStatus (3) and GetErrorBuffer (5), and
        // IDxcBlob GetBufferPointer (3) / GetBufferSize (4).
        unsafe {
            let mut result: Object = std::ptr::null_mut();
            let validate: unsafe extern "system" fn(
                Object,
                *mut Blob,
                u32,
                *mut Object,
            ) -> Hresult = method(self.object, 3);
            let hr = validate(self.object, &mut blob, IN_PLACE_EDIT, &mut result);
            if hr < 0 || result.is_null() {
                if !result.is_null() {
                    release(result);
                }
                return Err(format!("Validate failed: {hr:#x}"));
            }
            let mut status = 0;
            let get_status: unsafe extern "system" fn(Object, *mut Hresult) -> Hresult =
                method(result, 3);
            let status_hr = get_status(result, &mut status);
            if status_hr < 0 {
                release(result);
                return Err(format!("GetStatus failed: {status_hr:#x}"));
            }
            let mut text = String::new();
            if status < 0 {
                let mut errors: Object = std::ptr::null_mut();
                let get_errors: unsafe extern "system" fn(Object, *mut Object) -> Hresult =
                    method(result, 5);
                if get_errors(result, &mut errors) >= 0 && !errors.is_null() {
                    let ptr: unsafe extern "system" fn(Object) -> *const u8 = method(errors, 3);
                    let len: unsafe extern "system" fn(Object) -> usize = method(errors, 4);
                    let (p, n) = (ptr(errors), len(errors));
                    if !p.is_null() && n > 0 {
                        let bytes = std::slice::from_raw_parts(p, n);
                        text = String::from_utf8_lossy(bytes)
                            .trim_end_matches('\0')
                            .trim()
                            .into();
                    }
                }
                release(errors);
                if text.is_empty() {
                    text = format!("rejected ({status:#x})");
                }
            }
            release(result);
            if status < 0 { Err(text) } else { Ok(()) }
        }
    }
}

impl Drop for Validator {
    fn drop(&mut self) {
        // SAFETY: we own one reference to the validator object and, when loaded
        // from a DLL, one module handle; release the object before unloading
        // the DLL that implements its vtable.
        unsafe {
            release(self.object);
            if !self.module.is_null() {
                FreeLibrary(self.module);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;

    const E_FAIL: Hresult = 0x8000_4005_u32 as i32;
    const E_NOINTERFACE: Hresult = 0x8000_4002_u32 as i32;

    #[repr(C)]
    struct ErrorBlobVtable {
        qi: unsafe extern "system" fn(
            *mut MockErrorBlob,
            *const [u8; 16],
            *mut *mut c_void,
        ) -> Hresult,
        add_ref: unsafe extern "system" fn(*mut MockErrorBlob) -> u32,
        release: unsafe extern "system" fn(*mut MockErrorBlob) -> u32,
        ptr: unsafe extern "system" fn(*mut MockErrorBlob) -> *const u8,
        len: unsafe extern "system" fn(*mut MockErrorBlob) -> usize,
    }

    #[repr(C)]
    struct MockErrorBlob {
        vtable: *const ErrorBlobVtable,
        ptr: *const u8,
        len: usize,
        releases: usize,
    }

    unsafe extern "system" fn mock_blob_qi(
        _: *mut MockErrorBlob,
        _: *const [u8; 16],
        out: *mut *mut c_void,
    ) -> Hresult {
        // SAFETY: caller provides a writable output pointer.
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }

    unsafe extern "system" fn mock_blob_add_ref(_: *mut MockErrorBlob) -> u32 {
        1
    }

    unsafe extern "system" fn mock_blob_release(this: *mut MockErrorBlob) -> u32 {
        // SAFETY: `this` points to a live `MockErrorBlob` on the test stack.
        unsafe {
            (*this).releases += 1;
        }
        0
    }

    unsafe extern "system" fn mock_blob_ptr(this: *mut MockErrorBlob) -> *const u8 {
        // SAFETY: `this` points to a live `MockErrorBlob` on the test stack.
        unsafe { (*this).ptr }
    }

    unsafe extern "system" fn mock_blob_len(this: *mut MockErrorBlob) -> usize {
        // SAFETY: `this` points to a live `MockErrorBlob` on the test stack.
        unsafe { (*this).len }
    }

    static MOCK_ERROR_BLOB_VTABLE: ErrorBlobVtable = ErrorBlobVtable {
        qi: mock_blob_qi,
        add_ref: mock_blob_add_ref,
        release: mock_blob_release,
        ptr: mock_blob_ptr,
        len: mock_blob_len,
    };

    #[repr(C)]
    struct ResultVtable {
        qi: unsafe extern "system" fn(
            *mut MockResult,
            *const [u8; 16],
            *mut *mut c_void,
        ) -> Hresult,
        add_ref: unsafe extern "system" fn(*mut MockResult) -> u32,
        release: unsafe extern "system" fn(*mut MockResult) -> u32,
        get_status: unsafe extern "system" fn(*mut MockResult, *mut Hresult) -> Hresult,
        get_result: unsafe extern "system" fn(*mut MockResult, *mut Object) -> Hresult,
        get_errors: unsafe extern "system" fn(*mut MockResult, *mut Object) -> Hresult,
    }

    #[repr(C)]
    struct MockResult {
        vtable: *const ResultVtable,
        status_hr: Hresult,
        status: Hresult,
        error_hr: Hresult,
        error_blob: Object,
        releases: usize,
    }

    unsafe extern "system" fn mock_result_qi(
        _: *mut MockResult,
        _: *const [u8; 16],
        out: *mut *mut c_void,
    ) -> Hresult {
        // SAFETY: caller provides a writable output pointer.
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }

    unsafe extern "system" fn mock_result_add_ref(_: *mut MockResult) -> u32 {
        1
    }

    unsafe extern "system" fn mock_result_release(this: *mut MockResult) -> u32 {
        // SAFETY: `this` points to a live `MockResult` on the test stack.
        unsafe {
            (*this).releases += 1;
        }
        0
    }

    unsafe extern "system" fn mock_result_get_status(
        this: *mut MockResult,
        out: *mut Hresult,
    ) -> Hresult {
        // SAFETY: `this` and `out` are valid pointers for this mock call.
        unsafe {
            *out = (*this).status;
            (*this).status_hr
        }
    }

    unsafe extern "system" fn mock_result_get_result(
        _: *mut MockResult,
        out: *mut Object,
    ) -> Hresult {
        // SAFETY: caller provides a writable output pointer.
        unsafe { *out = std::ptr::null_mut() };
        E_FAIL
    }

    unsafe extern "system" fn mock_result_get_errors(
        this: *mut MockResult,
        out: *mut Object,
    ) -> Hresult {
        // SAFETY: `this` and `out` are valid pointers for this mock call.
        unsafe {
            *out = (*this).error_blob;
            (*this).error_hr
        }
    }

    static MOCK_RESULT_VTABLE: ResultVtable = ResultVtable {
        qi: mock_result_qi,
        add_ref: mock_result_add_ref,
        release: mock_result_release,
        get_status: mock_result_get_status,
        get_result: mock_result_get_result,
        get_errors: mock_result_get_errors,
    };

    #[repr(C)]
    struct ValidatorVtable {
        qi: unsafe extern "system" fn(
            *mut MockValidator,
            *const [u8; 16],
            *mut *mut c_void,
        ) -> Hresult,
        add_ref: unsafe extern "system" fn(*mut MockValidator) -> u32,
        release: unsafe extern "system" fn(*mut MockValidator) -> u32,
        validate:
            unsafe extern "system" fn(*mut MockValidator, *mut Blob, u32, *mut Object) -> Hresult,
    }

    #[repr(C)]
    struct MockValidator {
        vtable: *const ValidatorVtable,
        validate_hr: Hresult,
        result: Object,
        releases: usize,
    }

    unsafe extern "system" fn mock_val_qi(
        _: *mut MockValidator,
        _: *const [u8; 16],
        out: *mut *mut c_void,
    ) -> Hresult {
        // SAFETY: caller provides a writable output pointer.
        unsafe { *out = std::ptr::null_mut() };
        E_NOINTERFACE
    }

    unsafe extern "system" fn mock_val_add_ref(_: *mut MockValidator) -> u32 {
        1
    }

    unsafe extern "system" fn mock_val_release(this: *mut MockValidator) -> u32 {
        // SAFETY: `this` points to a live `MockValidator` on the test stack.
        unsafe {
            (*this).releases += 1;
        }
        0
    }

    unsafe extern "system" fn mock_val_validate(
        this: *mut MockValidator,
        _: *mut Blob,
        _: u32,
        out: *mut Object,
    ) -> Hresult {
        // SAFETY: `this` and `out` are valid pointers for this mock call.
        unsafe {
            *out = (*this).result;
            (*this).validate_hr
        }
    }

    static MOCK_VALIDATOR_VTABLE: ValidatorVtable = ValidatorVtable {
        qi: mock_val_qi,
        add_ref: mock_val_add_ref,
        release: mock_val_release,
        validate: mock_val_validate,
    };

    #[test]
    fn get_status_failure_returns_err_and_releases_result() {
        let mut mock_res = MockResult {
            vtable: &MOCK_RESULT_VTABLE,
            status_hr: E_FAIL,
            status: 0,
            error_hr: E_FAIL,
            error_blob: std::ptr::null_mut(),
            releases: 0,
        };
        let mut mock_val = MockValidator {
            vtable: &MOCK_VALIDATOR_VTABLE,
            validate_hr: 0,
            result: (&raw mut mock_res).cast(),
            releases: 0,
        };
        let mut v = Validator {
            object: (&raw mut mock_val).cast(),
            module: std::ptr::null_mut(),
        };
        let mut buf = [0u8; 4];
        let Err(err) = v.validate(&mut buf) else {
            panic!("expected GetStatus failure");
        };
        assert_eq!(err, format!("GetStatus failed: {E_FAIL:#x}"));
        assert_eq!(mock_res.releases, 1);
        drop(v);
        assert_eq!(mock_val.releases, 1);
    }

    #[test]
    fn null_empty_error_blob_falls_back_without_ub() {
        let dummy = *b"x";
        let cases = [
            (std::ptr::null(), 0),
            (std::ptr::null(), 16),
            (dummy.as_ptr(), 0),
        ];
        for (ptr, len) in cases {
            let mut mock_blob = MockErrorBlob {
                vtable: &MOCK_ERROR_BLOB_VTABLE,
                ptr,
                len,
                releases: 0,
            };
            let mut mock_res = MockResult {
                vtable: &MOCK_RESULT_VTABLE,
                status_hr: 0,
                status: E_FAIL,
                error_hr: 0,
                error_blob: (&raw mut mock_blob).cast(),
                releases: 0,
            };
            let mut mock_val = MockValidator {
                vtable: &MOCK_VALIDATOR_VTABLE,
                validate_hr: 0,
                result: (&raw mut mock_res).cast(),
                releases: 0,
            };
            let mut v = Validator {
                object: (&raw mut mock_val).cast(),
                module: std::ptr::null_mut(),
            };
            let mut buf = [0u8; 4];
            let Err(err) = v.validate(&mut buf) else {
                panic!("expected validation rejection");
            };
            assert_eq!(err, format!("rejected ({E_FAIL:#x})"));
            assert_eq!(mock_blob.releases, 1);
            assert_eq!(mock_res.releases, 1);
            drop(v);
            assert_eq!(mock_val.releases, 1);
        }
    }

    #[test]
    fn validate_error_with_non_null_result_releases_result() {
        let mut mock_res = MockResult {
            vtable: &MOCK_RESULT_VTABLE,
            status_hr: 0,
            status: 0,
            error_hr: 0,
            error_blob: std::ptr::null_mut(),
            releases: 0,
        };
        let mut mock_val = MockValidator {
            vtable: &MOCK_VALIDATOR_VTABLE,
            validate_hr: E_FAIL,
            result: (&raw mut mock_res).cast(),
            releases: 0,
        };
        let mut v = Validator {
            object: (&raw mut mock_val).cast(),
            module: std::ptr::null_mut(),
        };
        let mut buf = [0u8; 4];
        let Err(err) = v.validate(&mut buf) else {
            panic!("expected Validate failure");
        };
        assert_eq!(err, format!("Validate failed: {E_FAIL:#x}"));
        assert_eq!(mock_res.releases, 1);
        drop(v);
        assert_eq!(mock_val.releases, 1);
    }

    #[test]
    fn load_errors_strip_nul_and_free_module() {
        let missing = std::path::Path::new("victory_missing_validator_test.dll");
        let Err(err) = Validator::load(Some(missing)) else {
            panic!("loading missing DLL unexpectedly succeeded");
        };
        assert!(!err.contains('\0'), "NUL byte leaked into error: {err:?}");
        assert!(
            err.contains("victory_missing_validator_test.dll"),
            "unexpected missing-DLL error: {err:?}"
        );

        let candidates = ["cabinet.dll", "msimg32.dll", "lz32.dll", "version.dll"];
        let mut target = None;
        for dll in candidates {
            let wide: Vec<u16> = dll.encode_utf16().chain([0]).collect();
            // SAFETY: `wide` is a NUL-terminated UTF-16 DLL name.
            let handle = unsafe { GetModuleHandleW(wide.as_ptr()) };
            if handle.is_null() {
                target = Some((dll, wide));
                break;
            }
        }
        let (dll, wide) = target.expect("all candidate system DLLs are already loaded");
        let Err(err) = Validator::load(Some(std::path::Path::new(dll))) else {
            panic!("loading {dll} as a validator unexpectedly succeeded");
        };
        assert!(
            err.contains("DxcCreateInstance"),
            "expected GetProcAddress failure for {dll}, got: {err:?}"
        );
        // SAFETY: `wide` is a NUL-terminated UTF-16 DLL name.
        let after = unsafe { GetModuleHandleW(wide.as_ptr()) };
        assert!(after.is_null(), "{dll} remained loaded after failed load");
    }
}
