//! Verify dirty-page compaction and exact byte masks on a D3D12 compute queue.

use std::collections::BTreeMap;
use std::mem::ManuallyDrop;
use std::ops::Range;
use std::ptr;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_11_0, Fxc::D3DCompile, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, Result, s};

use crate::host::transition;

const SOURCE: &str = concat!(
    "#define D3D12_TEST_ROOT_SIGNATURE\n",
    include_str!("dirty_pages.hlsl")
);
const PAGE: usize = 64 << 10;
const MAX_WORDS: usize = PAGE / 4 + 1;
const MASK_BYTES: usize = MAX_WORDS.div_ceil(8) * 4;
const HEADER_BYTES: usize = 16;
const RECORD_BYTES: usize = HEADER_BYTES + MASK_BYTES + MAX_WORDS * 4;
const GUARD: u8 = 0xa5;
const INPUT_BYTES: usize = 3 * PAGE + 64;

struct Case {
    name: &'static str,
    range: Range<usize>,
    bias: usize,
    capacity: usize,
    changes: Vec<usize>,
}

fn read_word(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
}

fn write_word(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Build expected records by byte, leaving unused words and padding poisoned.
fn expect_records(case: &Case, before: &[u8], after: &[u8]) -> BTreeMap<u32, Vec<u8>> {
    let first_page = (case.bias + case.range.start) / PAGE;
    let mut records = BTreeMap::new();
    for byte in case.range.clone().filter(|&at| before[at] != after[at]) {
        let page = (case.bias + byte) / PAGE;
        let start = (page * PAGE)
            .saturating_sub(case.bias)
            .max(case.range.start);
        let end = ((page + 1) * PAGE - case.bias).min(case.range.end);
        let id = (page - first_page) as u32;
        let record = records.entry(id).or_insert_with(|| {
            let mut record = vec![GUARD; RECORD_BYTES];
            write_word(&mut record, 0, id);
            write_word(&mut record, 4, start as u32);
            write_word(&mut record, 8, end as u32);
            let words = end.div_ceil(4) - start / 4;
            record[HEADER_BYTES..HEADER_BYTES + words.div_ceil(8) * 4].fill(0);
            record
        });
        let word = byte / 4 - start / 4;
        let mask_at = HEADER_BYTES + 4 * (word / 8);
        let mask = read_word(record, mask_at) | (1 << (4 * (word % 8) + byte % 4));
        write_word(record, mask_at, mask);
        let value_at = HEADER_BYTES + MASK_BYTES + 4 * word;
        let dword = byte & !3;
        record[value_at..value_at + 4].copy_from_slice(&after[dword..dword + 4]);
    }
    records
}

fn assert_bytes(case: &str, actual: &[u8], expected: &[u8], start: usize) {
    assert_eq!(actual.len(), expected.len(), "{case}: compare size");
    if let Some(i) = actual.iter().zip(expected).position(|(a, b)| a != b) {
        panic!(
            "{case}: output byte {} is {:#04x}, expected {:#04x}",
            start + i,
            actual[i],
            expected[i]
        );
    }
}

fn verify(fixture: &Fixture, case: Case) -> Result<()> {
    let before: Vec<_> = (0..INPUT_BYTES)
        .map(|i| ((37 * i) ^ (i >> 3)) as u8)
        .collect();
    let mut after = before.clone();
    for &at in &case.changes {
        after[at] ^= (at % 255 + 1) as u8;
    }

    let mut expected = expect_records(&case, &before, &after);
    let count = expected.len();
    let groups = if case.range.is_empty() {
        1
    } else {
        // Dispatch one extra group to cover the guard after the last page.
        (case.bias + case.range.end - 1) / PAGE - (case.bias + case.range.start) / PAGE + 2
    };
    let constants = [
        case.range.start as u32,
        case.range.len() as u32,
        case.bias as u32,
        case.capacity as u32,
    ];
    let mut initial = vec![GUARD; HEADER_BYTES + case.capacity * RECORD_BYTES + 32];
    initial[..8].fill(0);
    let actual = fixture.run(&before, &after, &initial, constants, groups as u32)?;

    assert_eq!(
        read_word(&actual, 0) as usize,
        count,
        "{}: dirty count",
        case.name
    );
    assert_eq!(
        read_word(&actual, 4),
        u32::from(count > case.capacity),
        "{}: overflow",
        case.name
    );
    assert_bytes(
        case.name,
        &actual[8..HEADER_BYTES],
        &initial[8..HEADER_BYTES],
        8,
    );
    let written = count.min(case.capacity);
    for slot in 0..written {
        let at = HEADER_BYTES + slot * RECORD_BYTES;
        let id = read_word(&actual, at);
        let record = expected
            .remove(&id)
            .unwrap_or_else(|| panic!("{}: unexpected or duplicate page {id}", case.name));
        assert_bytes(case.name, &actual[at..at + RECORD_BYTES], &record, at);
    }
    let unused = HEADER_BYTES + written * RECORD_BYTES;
    assert_bytes(case.name, &actual[unused..], &initial[unused..], unused);
    eprintln!("{}: {count} dirty pages, {written} records", case.name);
    Ok(())
}

fn exercise(device: ID3D12Device, adapter: &DXGI_ADAPTER_DESC1, label: &str) -> Result<()> {
    let name = String::from_utf16_lossy(&adapter.Description);
    eprintln!(
        "D3D12 dirty-page adapter ({label}): {} ({:#06x}:{:#06x}, flags {:#x})",
        name.trim_end_matches('\0'),
        adapter.VendorId,
        adapter.DeviceId,
        adapter.Flags,
    );
    let fixture = Fixture::new(device)?;
    for case in [
        Case {
            name: "clean",
            range: 0..3 * PAGE,
            bias: 0,
            capacity: 3,
            changes: vec![],
        },
        Case {
            name: "empty",
            range: 5..5,
            bias: 0,
            capacity: 0,
            changes: vec![5],
        },
        Case {
            name: "one byte",
            range: 5..6,
            bias: 0,
            capacity: 1,
            changes: vec![4, 5, 6],
        },
        Case {
            name: "sparse first and partial last",
            range: 0..3 * PAGE + 3,
            bias: 0,
            capacity: 4,
            changes: vec![0, 2 * PAGE + 7, 3 * PAGE + 2],
        },
        Case {
            name: "dense",
            range: 0..2 * PAGE + 3,
            bias: 0,
            capacity: 3,
            changes: (0..2 * PAGE + 3).collect(),
        },
        Case {
            name: "unrelated and guard bytes",
            range: 5..PAGE + 7,
            bias: 0,
            capacity: 2,
            changes: vec![4, PAGE + 7, PAGE + 8, 2 * PAGE],
        },
        Case {
            name: "unaligned page boundary",
            range: 5..2 * PAGE + 15,
            bias: 3,
            capacity: 3,
            changes: vec![
                4,
                5,
                PAGE - 4,
                PAGE - 3,
                PAGE - 2,
                2 * PAGE + 14,
                2 * PAGE + 15,
            ],
        },
        Case {
            name: "full unaligned page",
            range: PAGE - 3..2 * PAGE - 3,
            bias: 3,
            capacity: 1,
            changes: vec![PAGE - 4, PAGE - 3, 2 * PAGE - 4, 2 * PAGE - 3],
        },
        Case {
            name: "padded physical right edge",
            range: INPUT_BYTES - 7..INPUT_BYTES - 1,
            bias: 0,
            capacity: 1,
            changes: vec![
                INPUT_BYTES - 8,
                INPUT_BYTES - 7,
                INPUT_BYTES - 2,
                INPUT_BYTES - 1,
            ],
        },
        Case {
            name: "zero capacity",
            range: 0..PAGE,
            bias: 0,
            capacity: 0,
            changes: vec![0],
        },
        Case {
            name: "capacity overflow",
            range: 0..3 * PAGE + 3,
            bias: 0,
            capacity: 2,
            changes: vec![0, PAGE + 1, 2 * PAGE + 2, 3 * PAGE + 1],
        },
    ] {
        verify(&fixture, case)?;
    }
    Ok(())
}

#[test]
fn warp_dirty_pages() -> Result<()> {
    // SAFETY: plain DXGI calls with valid out pointers and a live adapter.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter1 = factory.EnumWarpAdapter()?;
        let desc = adapter.GetDesc1()?;
        assert_ne!(
            desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32,
            0,
            "WARP requires software adapter"
        );
        let mut device = None;
        D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device)?;
        exercise(device.expect("WARP device"), &desc, "WARP")
    }
}

#[test]
fn hardware_dirty_pages() -> Result<()> {
    // SAFETY: plain DXGI calls with valid out pointers and live adapters.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        for index in 0.. {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(adapter) => adapter,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e),
            };
            let desc = adapter.GetDesc1()?;
            // The Microsoft Basic adapter can appear without the software flag
            // when DXGI enumerates the default display adapter.
            let basic_render_driver = (desc.VendorId, desc.DeviceId) == (0x1414, 0x008c);
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || basic_render_driver {
                continue;
            }
            let mut device = None;
            if D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_11_0, &mut device).is_ok()
                && let Some(device) = device
            {
                return exercise(device, &desc, "hardware");
            }
        }
        eprintln!("SKIP D3D12 dirty-page hardware check: no compatible physical adapter");
        Ok(())
    }
}

struct Fixture {
    device: ID3D12Device,
    queue: ID3D12CommandQueue,
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}

impl Fixture {
    fn new(device: ID3D12Device) -> Result<Self> {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the shader source, names and out pointers live through the call.
        let compiled = unsafe {
            D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("dirty_pages.hlsl"),
                None,
                None,
                s!("main"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            )
        };
        if let Err(error) = compiled {
            if let Some(errors) = errors {
                // SAFETY: the blob owns the reported byte range.
                let log = unsafe {
                    std::slice::from_raw_parts(
                        errors.GetBufferPointer().cast::<u8>(),
                        errors.GetBufferSize(),
                    )
                };
                eprintln!(
                    "dirty-page shader compiler: {}",
                    String::from_utf8_lossy(log)
                );
            }
            return Err(error);
        }
        let code = code.expect("compiled shader");
        // SAFETY: the blob owns the shader and the embedded root signature.
        unsafe {
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: ManuallyDrop::new(Some(root.clone())),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            };
            let pipeline = device.CreateComputePipelineState(&desc);
            drop(ManuallyDrop::into_inner(desc.pRootSignature));
            let queue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })?;
            Ok(Self {
                device,
                queue,
                root,
                pipeline: pipeline?,
            })
        }
    }

    fn run(
        &self,
        before: &[u8],
        after: &[u8],
        initial: &[u8],
        constants: [u32; 4],
        groups: u32,
    ) -> Result<Vec<u8>> {
        let before = upload(&self.device, before)?;
        let after = upload(&self.device, after)?;
        let size = initial.len();
        let initial = upload(&self.device, initial)?;
        let result = buffer(
            &self.device,
            size,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        )?;
        let readback = buffer(
            &self.device,
            size,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_FLAG_NONE,
        )?;
        // SAFETY: the test owns five live buffers covering every recorded range.
        // The fence completes before the readback mapping or resource destruction.
        unsafe {
            let alloc: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList =
                self.device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)?;
            list.CopyBufferRegion(&result, 0, &initial, 0, size as u64);
            list.ResourceBarrier(&[transition(
                &result,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
            list.SetComputeRootSignature(&self.root);
            list.SetPipelineState(&self.pipeline);
            list.SetComputeRoot32BitConstants(
                0,
                constants.len() as u32,
                constants.as_ptr().cast(),
                0,
            );
            list.SetComputeRootShaderResourceView(1, before.GetGPUVirtualAddress());
            list.SetComputeRootShaderResourceView(2, after.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(3, result.GetGPUVirtualAddress());
            list.Dispatch(groups, 1, 1);
            list.ResourceBarrier(&[transition(
                &result,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.CopyBufferRegion(&readback, 0, &result, 0, size as u64);
            list.Close()?;
            let fence: ID3D12Fence = self.device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
            self.queue.ExecuteCommandLists(&[Some(list.cast()?)]);
            self.queue.Signal(&fence, 1)?;
            // A null event makes SetEventOnCompletion wait on this thread.
            fence.SetEventOnCompletion(1, HANDLE::default())?;
            self.device.GetDeviceRemovedReason()?;
            let mut mapped = ptr::null_mut();
            readback.Map(
                0,
                Some(&D3D12_RANGE {
                    Begin: 0,
                    End: size,
                }),
                Some(&mut mapped),
            )?;
            let bytes = std::slice::from_raw_parts(mapped.cast::<u8>(), size).to_vec();
            readback.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            Ok(bytes)
        }
    }
}

fn buffer(
    device: &ID3D12Device,
    size: usize,
    heap: D3D12_HEAP_TYPE,
    state: D3D12_RESOURCE_STATES,
    flags: D3D12_RESOURCE_FLAGS,
) -> Result<ID3D12Resource> {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: heap,
        CreationNodeMask: 1,
        VisibleNodeMask: 1,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
        Width: size as u64,
        Height: 1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
        Flags: flags,
        ..Default::default()
    };
    let mut resource = None;
    // SAFETY: valid heap properties, buffer description and out pointer.
    unsafe {
        device.CreateCommittedResource(
            &heap,
            D3D12_HEAP_FLAG_NONE,
            &desc,
            state,
            None,
            &mut resource,
        )?
    };
    Ok(resource.expect("created buffer"))
}

fn upload(device: &ID3D12Device, bytes: &[u8]) -> Result<ID3D12Resource> {
    let resource = buffer(
        device,
        bytes.len(),
        D3D12_HEAP_TYPE_UPLOAD,
        D3D12_RESOURCE_STATE_GENERIC_READ,
        D3D12_RESOURCE_FLAG_NONE,
    )?;
    // SAFETY: the test owns an upload resource of exactly this size.
    unsafe {
        let mut mapped = ptr::null_mut();
        resource.Map(
            0,
            Some(&D3D12_RANGE { Begin: 0, End: 0 }),
            Some(&mut mapped),
        )?;
        ptr::copy_nonoverlapping(bytes.as_ptr(), mapped.cast::<u8>(), bytes.len());
        resource.Unmap(
            0,
            Some(&D3D12_RANGE {
                Begin: 0,
                End: bytes.len(),
            }),
        );
    }
    Ok(resource)
}
