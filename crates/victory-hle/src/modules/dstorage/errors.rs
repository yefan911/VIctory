use std::collections::BTreeMap;
use std::ffi::CStr;
use std::os::windows::io::{AsRawHandle, FromRawHandle, IntoRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{HANDLE, MAX_PATH};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, SetEvent};

use super::{File, HRESULT, Instance, MEMORY_SOURCE, Request, S_OK, from_win32};

/// The Xbox error record, with the read request arm of the union.
/// The request fields match Microsoft's legacy Xbox compatibility header.
#[repr(C)]
#[derive(Default)]
pub(super) struct ErrorRecord {
    pub(super) failure_count: u32,
    _count_padding: u32,
    pub(super) first: ReadFailure,
}

#[repr(C)]
pub(super) struct ReadFailure {
    pub(super) result: HRESULT,
    // The Xbox command type occupies the legacy record's alignment padding.
    pub(super) command_type: i32,
    pub(super) options: u64,
    pub(super) destination: usize,
    pub(super) destination_size: u32,
    pub(super) filename: [u16; MAX_PATH as usize],
    _filename_padding: u32,
    pub(super) file_offset: u64,
    pub(super) source: usize,
    pub(super) source_size: u32,
    _source_size_padding: u32,
    pub(super) tag: u64,
    pub(super) name: [u8; 64],
}

// Verified with the x64 compiler against Microsoft's
// SimpleDirectStorageCombo/DirectStorageWin32/dstorage_win32.h.
const _: () = {
    use std::mem::{align_of, offset_of};
    assert!(size_of::<ErrorRecord>() == 0x290);
    assert!(align_of::<ErrorRecord>() == 8);
    assert!(offset_of!(ErrorRecord, first) == 0x8);
    assert!(offset_of!(ReadFailure, result) == 0x0);
    assert!(offset_of!(ReadFailure, command_type) == 0x4);
    assert!(offset_of!(ReadFailure, options) == 0x8);
    assert!(offset_of!(ReadFailure, destination) == 0x10);
    assert!(offset_of!(ReadFailure, destination_size) == 0x18);
    assert!(offset_of!(ReadFailure, filename) == 0x1c);
    assert!(offset_of!(ReadFailure, file_offset) == 0x228);
    assert!(offset_of!(ReadFailure, source) == 0x230);
    assert!(offset_of!(ReadFailure, source_size) == 0x238);
    assert!(offset_of!(ReadFailure, tag) == 0x240);
    assert!(offset_of!(ReadFailure, name) == 0x248);
};

impl Default for ReadFailure {
    fn default() -> Self {
        Self {
            result: S_OK,
            command_type: -1, // DSTORAGE_COMMAND_TYPE_NONE
            options: 0,
            destination: 0,
            destination_size: 0,
            filename: [0; MAX_PATH as usize],
            _filename_padding: 0,
            file_offset: 0,
            source: 0,
            source_size: 0,
            _source_size_padding: 0,
            tag: 0,
            name: [0; 64],
        }
    }
}

impl ReadFailure {
    /// Copies metadata while the worker still holds the live request.
    pub(super) fn new(request: &Request, result: HRESULT) -> Self {
        let mut record = Self {
            result,
            command_type: 0, // DSTORAGE_COMMAND_TYPE_REQUEST
            options: request.options,
            destination: request.destination as usize,
            destination_size: request.destination_size,
            source_size: request.source_size,
            tag: request.tag,
            ..Self::default()
        };
        if request.options & MEMORY_SOURCE != 0 {
            record.source = request.source as usize;
        } else if request.has_supported_options() {
            record.file_offset = request.offset;
            // SAFETY: supported file requests name a live file object or null.
            if let Some(file) = unsafe { request.source.cast::<Instance<File>>().as_ref() } {
                let filename = &file.state.filename;
                let length = filename.len().min(record.filename.len() - 1);
                record.filename[..length].copy_from_slice(&filename[..length]);
            }
        }
        if !request.name.is_null() {
            // SAFETY: the title keeps its NUL-terminated name through completion.
            let name = unsafe { CStr::from_ptr(request.name) }.to_bytes();
            let length = name.len().min(record.name.len() - 1);
            record.name[..length].copy_from_slice(&name[..length]);
        }
        record
    }
}

/// Error notifications and records for one lane, under the scheduler lock.
pub(super) struct QueueErrors {
    event: OwnedHandle,
    /// Failed reads waiting for earlier reads to finish.
    pending: BTreeMap<u64, Box<ReadFailure>>,
    record: ErrorRecord,
}

impl QueueErrors {
    pub(super) fn new() -> Result<Self, HRESULT> {
        // SAFETY: a private, initially unsignaled auto-reset event.
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        if event.is_null() {
            return Err(from_win32(&std::io::Error::last_os_error()));
        }
        Ok(Self {
            // SAFETY: a new handle owned by this lane.
            event: unsafe { OwnedHandle::from_raw_handle(event) },
            pending: BTreeMap::new(),
            record: ErrorRecord::default(),
        })
    }

    pub(super) fn duplicate_event(&self) -> HANDLE {
        // The title owns its duplicate and may close it independently.
        self.event
            .try_clone()
            .map_or(std::ptr::null_mut(), OwnedHandle::into_raw_handle)
    }

    pub(super) fn defer(&mut self, sequence: u64, failure: Box<ReadFailure>) {
        self.pending.insert(sequence, failure);
    }

    /// Publishes completed failures up to `limit` and returns the first
    /// newly published result for the status entry.
    pub(super) fn publish_before(&mut self, limit: u64) -> Option<HRESULT> {
        let mut first = None;
        while self
            .pending
            .first_key_value()
            .is_some_and(|(&seq, _)| seq < limit)
        {
            let (_, failure) = self.pending.pop_first().unwrap();
            first.get_or_insert(failure.result);
            if self.record.failure_count == 0 {
                self.record.first = *failure;
            }
            self.record.failure_count = self.record.failure_count.saturating_add(1);
        }
        if first.is_some() {
            // SAFETY: the lane owns the live event.
            unsafe { SetEvent(self.event.as_raw_handle()) };
        }
        first
    }

    pub(super) fn take(&mut self) -> ErrorRecord {
        // SAFETY: the lane owns the event. Reset and retrieval share the
        // scheduler lock with publication so a new failure cannot be lost.
        unsafe { ResetEvent(self.event.as_raw_handle()) };
        std::mem::take(&mut self.record)
    }
}
