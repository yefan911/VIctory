import Foundation
import Metal

func require(_ ok: Bool, _ message: String) {
    if !ok {
        print("FAIL: \(message)")
        exit(1)
    }
}

let args = CommandLine.arguments
require(args.count >= 2, "Missing probe mode")
let mode = args[1]
require(
    (mode == "compute" && args.count == 4)
    || (mode == "render" && args.count == 8),
    "Invalid probe arguments"
)

let device = MTLCreateSystemDefaultDevice()!
let queue = device.makeCommandQueue()!

print("GPU: \(device.name)")

func load(
    _ path: String,
    _ type: MTLFunctionType
) throws -> MTLFunction {
    let lib = try device.makeLibrary(
        URL: URL(fileURLWithPath: path)
    )

    for name in lib.functionNames {
        if let f = lib.makeFunction(name: name),
           f.functionType == type {
            return f
        }
    }

    throw NSError(
        domain: "Missing shader function", code: 1
    )
}

let source = """
#include <metal_stdlib>
using namespace metal;

vertex float4 fullscreen(uint id [[vertex_id]]) {
    const float2 p[3] = {
        float2(-1,-1),
        float2(3,-1),
        float2(-1,3)
    };
    return float4(p[id],0,1);
}

fragment float4 control() {
    return float4(0,1,0,1);
}
"""

let native = try device.makeLibrary(
    source: source, options: nil
)

func permByte(
    _ a: UInt32,
    _ b: UInt32,
    _ selector: Int
) -> Int {
    if selector < 8 {
        let word = selector < 4 ? b : a
        return Int(
            (word >> ((selector % 4) * 8)) & 255
        )
    }

    if selector < 12 {
        let word = selector < 10 ? b : a
        let bit = selector % 2 == 0 ? 15 : 31
        return ((word >> bit) & 1) == 1 ? 255 : 0
    }

    return selector == 12 ? 0 : 255
}

func expected(
    _ name: String,
    _ x: Int,
    _ y: Int
) -> [Int] {
    switch name {
    case "control":
        return [0,255,0,255]
    case "red":
        return [255,0,0,255]
    case "gradient":
        return [
            Int(((Double(x)+0.5)*255/32).rounded()),
            Int(((Double(y)+0.5)*255/32).rounded()),
            0,255
        ]
    case "checker":
        let r = ((x ^ y) & 1) * 255
        return [r,255-r,0,255]
    case "select":
        let t = x >= 32 ? 255 : 0
        return [t,t,t,255]
    case "perm_a", "perm_b":
        let a: UInt32 = name == "perm_a"
            ? 0x807f12e3 : 0x7f80ed1c

        let b: UInt32 = name == "perm_a"
            ? 0x7f80a145 : 0x807f5eba

        return [0,0x55,0xaa,0xff].map {
            permByte(a,b,x ^ $0)
        }
    default:
        fatalError("Unknown reference case: " + name)
    }
}

if mode == "compute" {
    let function = try load(args[2], .kernel)

    let pipeline = try device.makeComputePipelineState(
        function: function
    )

    let threads = Int(args[3])!

    require(threads == 64, "Unexpected reflection size")

    require(
        pipeline.maxTotalThreadsPerThreadgroup >= threads,
        "Unsupported threadgroup"
    )

    let count = 80
    let sentinel: UInt32 = 0xDEADBEEF

    let output = device.makeBuffer(
        length: count * 4,
        options: .storageModeShared
    )!

    let ptr = output.contents().bindMemory(
        to: UInt32.self,
        capacity: count
    )

    for i in 0..<count {
        ptr[i] = sentinel
    }

    let heap = device.makeBuffer(
        length: 24,
        options: .storageModeShared
    )!

    let entry = heap.contents().bindMemory(
        to: UInt64.self,
        capacity: 3
    )

    entry[0] = output.gpuAddress
    entry[1] = 0
    entry[2] = UInt64(output.length)

    let cmd = queue.makeCommandBuffer()!
    let enc = cmd.makeComputeCommandEncoder()!

    enc.setComputePipelineState(pipeline)
    enc.setBuffer(heap, offset: 0, index: 0)
    enc.useResource(output, usage: [.read, .write])

    enc.dispatchThreadgroups(
        MTLSize(width:1,height:1,depth:1),
        threadsPerThreadgroup:
            MTLSize(width:threads,height:1,depth:1)
    )

    enc.endEncoding()
    cmd.commit()
    cmd.waitUntilCompleted()

    require(
        cmd.status == .completed,
        "\(String(describing: cmd.error))"
    )

    var matched = 0

    for i in 0..<64 {
        let want = UInt32(i) ^ 0x13579bdf
        if ptr[i] == want {
            matched += 1
        }
    }

    let guards = (64..<80).filter {
        ptr[$0] == sentinel
    }.count

    print("Compute values: \(matched)/64")
    print("Buffer guards: \(guards)/16")

    require(matched == 64, "Compute result mismatch")
    require(guards == 16, "Buffer guard modified")

    print("PASS: compute GPU readback")
    exit(0)
}

let name = args[2]
let width = Int(args[3])!
let height = Int(args[4])!

let vertex: MTLFunction

if args[5] == "native" {
    vertex = native.makeFunction(name:"fullscreen")!
} else {
    vertex = try load(args[5], .vertex)
}

let fragment: MTLFunction

if name == "control" {
    fragment = native.makeFunction(name:"control")!
} else {
    fragment = try load(args[6], .fragment)
}

let td = MTLTextureDescriptor.texture2DDescriptor(
    pixelFormat: .rgba8Unorm,
    width: width,
    height: height,
    mipmapped: false
)

td.storageMode = .shared
td.usage = [.renderTarget]

let texture = device.makeTexture(descriptor:td)!

let pd = MTLRenderPipelineDescriptor()

pd.vertexFunction = vertex
pd.fragmentFunction = fragment
pd.colorAttachments[0].pixelFormat = .rgba8Unorm
pd.colorAttachments[0].isBlendingEnabled = false

let pipeline = try device.makeRenderPipelineState(
    descriptor: pd
)

let pass = MTLRenderPassDescriptor()

pass.colorAttachments[0].texture = texture
pass.colorAttachments[0].loadAction = .clear
pass.colorAttachments[0].storeAction = .store
pass.colorAttachments[0].clearColor = MTLClearColor(
    red:0,green:0,blue:1,alpha:1
)

let cmd = queue.makeCommandBuffer()!
let enc = cmd.makeRenderCommandEncoder(descriptor:pass)!

enc.setRenderPipelineState(pipeline)

enc.drawPrimitives(
    type:.triangle,
    vertexStart:0,
    vertexCount:3
)

enc.endEncoding()
cmd.commit()
cmd.waitUntilCompleted()

require(
    cmd.status == .completed,
    "\(String(describing:cmd.error))"
)

var pixels = [UInt8](
    repeating:0,
    count:width*height*4
)

pixels.withUnsafeMutableBytes { bytes in
    texture.getBytes(
        bytes.baseAddress!,
        bytesPerRow:width*4,
        from:MTLRegionMake2D(0,0,width,height),
        mipmapLevel:0
    )
}

var matched = 0
var worst = 0
let tolerance = name == "gradient" ? 1 : 0

for y in 0..<height {
    for x in 0..<width {
        let at = (y*width+x)*4
        let want = expected(name,x,y)

        let error = (0..<4).map {
            abs(Int(pixels[at+$0])-want[$0])
        }.max()!

        worst = max(worst,error)

        if error <= tolerance {
            matched += 1
        }
    }
}

try Data(pixels).write(
    to:URL(fileURLWithPath:args[7])
)

print("Pixels: \(matched)/\(width*height)")
print("Maximum channel error: \(worst)")

require(
    matched == width*height,
    "Pixel readback mismatch"
)

print("PASS: \(name)")
