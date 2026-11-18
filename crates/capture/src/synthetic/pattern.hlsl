// Synthetic test pattern with a visual frame stamp.
// Layout of the stamp must match telemetry::frame_code.

cbuffer FrameConstants : register(b0)
{
    uint4 code_words;   // frame id, timestamp low, timestamp high, crc32
    uint frame_id;
    uint mode;          // 0 = static, 1 = motion
    uint width;
    uint height;
};

static const uint BLOCK_SIZE = 16;
static const uint BLOCKS_PER_ROW = 64;
static const uint BLOCK_ROWS = 2;
static const uint SQUARE_COUNT = 8;
static const uint SQUARE_SIZE = 160;

struct VsOut
{
    float4 position : SV_Position;
};

// Full-screen triangle from SV_VertexID; no vertex buffer or input layout.
VsOut vs_main(uint id : SV_VertexID)
{
    float2 uv = float2((id << 1) & 2, id & 2);
    VsOut output;
    output.position = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return output;
}

// Integer hash (lowbias32) for deterministic pseudo-randomness.
uint hash(uint x)
{
    x ^= x >> 16;
    x *= 0x7feb352du;
    x ^= x >> 15;
    x *= 0x846ca68bu;
    x ^= x >> 16;
    return x;
}

float3 static_background(uint2 p)
{
    float2 uv = float2(p) / float2(width, height);
    float3 color = float3(uv.x, uv.y, 0.5) * 0.6 + 0.2;
    bool grid_line = (p.x % 64 == 0) || (p.y % 64 == 0);
    return grid_line ? color * 0.5 : color;
}

float3 motion_background(uint2 p)
{
    // Diagonal bands scrolling 8 px per frame.
    float band = frac((p.x + p.y + frame_id * 8) / 256.0);
    float3 color = float3(band, frac(band + 0.33), frac(band + 0.66));

    // Squares moving at different speeds; they wrap around the frame.
    uint2 travel = uint2(width - SQUARE_SIZE, height - SQUARE_SIZE);
    for (uint i = 0; i < SQUARE_COUNT; i++)
    {
        uint2 origin = uint2((hash(i) + frame_id * (3 + i)) % travel.x,
                             (hash(i + 99) + frame_id * (2 + i)) % travel.y);
        if (all(p >= origin) && all(p < origin + SQUARE_SIZE))
        {
            color = float3(hash(i * 7) & 255, hash(i * 13) & 255, hash(i * 17) & 255) / 255.0;
        }
    }

    // Per-pixel noise that changes every frame: hard to compress, like a game.
    float noise = (hash(p.x + p.y * width + frame_id * 0x9E3779B9u) & 255) / 255.0;
    return lerp(color, noise.xxx, 0.25);
}

float4 ps_main(VsOut input) : SV_Target
{
    uint2 p = uint2(input.position.xy);

    if (p.x < BLOCK_SIZE * BLOCKS_PER_ROW && p.y < BLOCK_SIZE * BLOCK_ROWS)
    {
        uint bit = (p.y / BLOCK_SIZE) * BLOCKS_PER_ROW + p.x / BLOCK_SIZE;
        float value = (code_words[bit / 32] >> (bit % 32)) & 1;
        return float4(value, value, value, 1);
    }

    float3 color = mode == 0 ? static_background(p) : motion_background(p);
    return float4(color, 1);
}
