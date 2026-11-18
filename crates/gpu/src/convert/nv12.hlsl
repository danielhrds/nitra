// BGRA8 to NV12 (BT.709, limited range) in two passes: luma into the Y
// plane (R8 view, full size), chroma into the CbCr plane (R8G8 view, half
// size). Scaling uses bilinear filtering. Must match convert::bt709_limited.

Texture2D<float4> source_image : register(t0);
SamplerState linear_clamp : register(s0);

struct VsOut
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

// Full-screen triangle; uv spans 0..1 over the viewport.
VsOut vs_main(uint id : SV_VertexID)
{
    float2 uv = float2((id << 1) & 2, id & 2);
    VsOut output;
    output.position = float4(uv * float2(2, -2) + float2(-1, 1), 0, 1);
    output.uv = uv;
    return output;
}

static const float3 LUMA_WEIGHTS = float3(0.2126, 0.7152, 0.0722);

float ps_luma(VsOut input) : SV_Target
{
    float3 rgb = source_image.Sample(linear_clamp, input.uv).rgb;
    return (16.0 + 219.0 * dot(rgb, LUMA_WEIGHTS)) / 255.0;
}

// Each chroma pixel covers 2x2 luma pixels; sampling at its center with
// bilinear filtering averages them (exactly so when not scaling).
float2 ps_chroma(VsOut input) : SV_Target
{
    float3 rgb = source_image.Sample(linear_clamp, input.uv).rgb;
    float luma = dot(rgb, LUMA_WEIGHTS);
    float2 chroma = float2((rgb.b - luma) / 1.8556, (rgb.r - luma) / 1.5748);
    return (128.0 + 224.0 * chroma) / 255.0;
}
