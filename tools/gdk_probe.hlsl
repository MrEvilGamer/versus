RWStructuredBuffer<uint> Out : register(u0);
cbuffer Params : register(b0) { uint Value; };
[RootSignature("CBV(b0),DescriptorTable(UAV(u0))")]
[numthreads(32, 1, 1)]
void main(uint3 tid : SV_DispatchThreadID) {
  Out[tid.x] = Value + tid.x;
}
