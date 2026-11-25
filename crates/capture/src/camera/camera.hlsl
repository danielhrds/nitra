// A camera frame, uploaded as it came, to BGRA: one pixel shader per
// pixel format, drawn over the whole target. Loads only (no sampler): each
// output pixel reads the bytes of the same pixel.

cbuffer Colors : register(b0)
{
    uint bt709;     // 1: BT.709 matrix, 0: BT.601 (most cameras)
    uint3 padding;
};

Texture2D<float4> plane0 : register(t0);
Texture2D<float4> plane1 : register(t1);

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

// Limited-range YCbCr (16-235, 16-240) to RGB.
float4 to_rgb(float y, float cb, float cr)
{
    float c = (y - 16.0 / 255.0) * (255.0 / 219.0);
    float d = (cb - 0.5) * (255.0 / 224.0);
    float e = (cr - 0.5) * (255.0 / 224.0);
    float3 rgb = bt709
        ? float3(c + 1.5748 * e, c - 0.1873 * d - 0.4681 * e, c + 1.8556 * d)
        : float3(c + 1.4020 * e, c - 0.3441 * d - 0.7141 * e, c + 1.7720 * d);
    return float4(saturate(rgb), 1);
}

// Two pixels per texel: Y0 U Y1 V in R G B A.
float4 ps_yuy2(VsOut input) : SV_Target
{
    uint2 p = uint2(input.position.xy);
    float4 t = plane0.Load(int3(p.x / 2, p.y, 0));
    return to_rgb((p.x & 1) ? t.b : t.r, t.g, t.a);
}

// Luma in plane 0 (R8), chroma at half size in plane 1 (R8G8).
float4 ps_nv12(VsOut input) : SV_Target
{
    uint2 p = uint2(input.position.xy);
    float y = plane0.Load(int3(p, 0)).r;
    float2 c = plane1.Load(int3(p / 2, 0)).rg;
    return to_rgb(y, c.r, c.g);
}

// Uploaded as BGRA: the fourth byte is unused, so alpha is set.
float4 ps_rgb32(VsOut input) : SV_Target
{
    return float4(plane0.Load(int3(uint2(input.position.xy), 0)).rgb, 1);
}

// Three bytes per pixel (B, G, R) in an R8 plane three times as wide.
float4 ps_rgb24(VsOut input) : SV_Target
{
    uint2 p = uint2(input.position.xy);
    int x = p.x * 3;
    float b = plane0.Load(int3(x, p.y, 0)).r;
    float g = plane0.Load(int3(x + 1, p.y, 0)).r;
    float r = plane0.Load(int3(x + 2, p.y, 0)).r;
    return float4(r, g, b, 1);
}
