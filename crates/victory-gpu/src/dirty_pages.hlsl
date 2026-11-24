ByteAddressBuffer Before : register(t0);
ByteAddressBuffer After : register(t1);
RWByteAddressBuffer Result : register(u0);

cbuffer Range : register(b0) {
    uint FirstByte;
    uint ByteCount;
    uint AddressLow16;
    uint RecordCapacity;
};

static const uint PageBytes = 64u * 1024u;
static const uint GroupThreads = 64u;
static const uint MaxWords = PageBytes / 4u + 1u;
static const uint MaskBytes = 4u * ((MaxWords + 7u) / 8u);
static const uint HeaderBytes = 16u;
static const uint RecordBytes = HeaderBytes + MaskBytes + 4u * MaxWords;

groupshared uint Dirty;
groupshared uint Slot;

uint Diff(uint offset, uint begin, uint end, uint previous, uint next) {
    // The caller visits only dwords intersecting [begin, end), so both shifts are at most 24.
    uint low = max(begin, offset) - offset;
    uint high = min(end - offset, 4u);
    uint keep = (0xffffffffu << (8u * low)) & (0xffffffffu >> (8u * (4u - high)));
    return (previous ^ next) & keep;
}

uint ChangedLanes(uint diff) {
    return uint((diff & 0x000000ffu) != 0) |
           (uint((diff & 0x0000ff00u) != 0) << 1) |
           (uint((diff & 0x00ff0000u) != 0) << 2) |
           (uint((diff & 0xff000000u) != 0) << 3);
}

#ifdef D3D12_TEST_ROOT_SIGNATURE
[RootSignature("RootConstants(num32BitConstants=4, b0), SRV(t0), SRV(t1), UAV(u0)")]
#endif
[numthreads(64, 1, 1)]
void main(uint3 group : SV_GroupID, uint thread : SV_GroupIndex) {
    uint firstPageLength = PageBytes - ((AddressLow16 + FirstByte) & (PageBytes - 1u));
    uint relative = group.x == 0 ? 0 : firstPageLength + (group.x - 1u) * PageBytes;
    if (relative >= ByteCount) return;

    uint begin = FirstByte + relative;
    uint end = begin + min(ByteCount - relative, group.x == 0 ? firstPageLength : PageBytes);
    uint firstWord = begin & ~3u;
    uint words = (end - firstWord - 1u) / 4u + 1u;

    if (thread == 0) Dirty = 0;
    GroupMemoryBarrierWithGroupSync();

    for (uint word = thread; word < words; word += GroupThreads) {
        uint offset = firstWord + 4u * word;
        if (Diff(offset, begin, end, Before.Load(offset), After.Load(offset)) != 0) {
            InterlockedOr(Dirty, 1);
            break;
        }
    }
    GroupMemoryBarrierWithGroupSync();
    if (Dirty == 0) return;

    // Capacity is a hard bound. The caller can classify a dense batch as fallback.
    if (thread == 0) {
        uint slot;
        Result.InterlockedAdd(0, 1, slot);
        Slot = slot;
        if (Slot >= RecordCapacity) {
            Result.InterlockedOr(4, 1);
        } else {
            Result.Store3(HeaderBytes + Slot * RecordBytes, uint3(group.x, begin, end));
        }
    }
    GroupMemoryBarrierWithGroupSync();
    if (Slot >= RecordCapacity) return;

    uint masksAt = HeaderBytes + Slot * RecordBytes + HeaderBytes;
    uint valuesAt = masksAt + MaskBytes;
    for (uint block = thread; block < (words + 7u) / 8u; block += GroupThreads) {
        uint mask = 0;
        for (uint j = 0; j < 8 && block * 8u + j < words; ++j) {
            uint word = block * 8u + j;
            uint offset = firstWord + 4u * word;
            uint next = After.Load(offset);
            uint lanes = ChangedLanes(Diff(offset, begin, end, Before.Load(offset), next));
            if (lanes != 0) Result.Store(valuesAt + 4u * word, next);
            mask |= lanes << (4u * j);
        }
        Result.Store(masksAt + 4u * block, mask);
    }
}
