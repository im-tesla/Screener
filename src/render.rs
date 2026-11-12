//! Small Direct3D11 NV12 renderer. It presents the newest frame immediately;
//! there is no media clock, frame queue, interpolation, or software RGB conversion.
use anyhow::{ensure, Context, Result};
use windows::{
    core::{s, Interface, PCSTR},
    Win32::{
        Foundation::*,
        Graphics::{Direct3D::Fxc::*, Direct3D::*, Direct3D11::*, Dxgi::Common::*, Dxgi::*},
        UI::WindowsAndMessaging::GetClientRect,
    },
};

const VERTEX: &str = r#"
struct Out { float4 pos:SV_Position; float2 uv:TEXCOORD0; };
Out main(uint id:SV_VertexID) {
    Out o; float2 uv=float2((id<<1)&2,id&2);
    o.pos=float4(uv*float2(2,-2)+float2(-1,1),0,1); o.uv=uv; return o;
}
"#;
const PIXEL: &str = r#"
Texture2D<float> Y:register(t0); Texture2D<float2> UV:register(t1);
SamplerState S:register(s0);
float4 main(float4 pos:SV_Position,float2 uv:TEXCOORD0):SV_Target {
    float y=(Y.Sample(S,uv)-16.0/255.0)*(255.0/219.0);
    float2 c=(UV.Sample(S,uv)-128.0/255.0)*(255.0/224.0);
    return float4(saturate(float3(y+1.5748*c.y,y-0.187324*c.x-0.468124*c.y,y+1.8556*c.x)),1);
}
"#;

pub struct Renderer {
    hwnd: HWND,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    swap: IDXGISwapChain,
    target: Option<ID3D11RenderTargetView>,
    vertex: ID3D11VertexShader,
    pixel: ID3D11PixelShader,
    sampler: ID3D11SamplerState,
    y: Option<ID3D11Texture2D>,
    uv: Option<ID3D11Texture2D>,
    y_view: Option<ID3D11ShaderResourceView>,
    uv_view: Option<ID3D11ShaderResourceView>,
    window_size: (u32, u32),
    image_size: (u32, u32),
}
impl Renderer {
    pub fn new(hwnd: HWND) -> Result<Self> {
        unsafe {
            let mut r = RECT::default();
            GetClientRect(hwnd, &mut r)?;
            let width = (r.right - r.left).max(1) as u32;
            let height = (r.bottom - r.top).max(1) as u32;
            let desc = DXGI_SWAP_CHAIN_DESC {
                BufferDesc: DXGI_MODE_DESC {
                    Width: width,
                    Height: height,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    ..Default::default()
                },
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                OutputWindow: hwnd,
                Windowed: true.into(),
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            let (mut device, mut context, mut swap) = (None, None, None);
            D3D11CreateDeviceAndSwapChain(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&desc),
                Some(&mut swap),
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.context("D3D device")?;
            let context = context.context("D3D context")?;
            let swap = swap.context("DXGI swap chain")?;
            device.cast::<IDXGIDevice1>()?.SetMaximumFrameLatency(1)?;
            let vs = compile(VERTEX, s!("vs_5_0"))?;
            let ps = compile(PIXEL, s!("ps_5_0"))?;
            let (mut vertex, mut pixel, mut sampler) = (None, None, None);
            device.CreateVertexShader(&vs, None, Some(&mut vertex))?;
            device.CreatePixelShader(&ps, None, Some(&mut pixel))?;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxLOD: f32::MAX,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            let mut renderer = Self {
                hwnd,
                device,
                context,
                swap,
                target: None,
                vertex: vertex.unwrap(),
                pixel: pixel.unwrap(),
                sampler: sampler.unwrap(),
                y: None,
                uv: None,
                y_view: None,
                uv_view: None,
                window_size: (width, height),
                image_size: (0, 0),
            };
            renderer.make_target()?;
            Ok(renderer)
        }
    }
    unsafe fn make_target(&mut self) -> Result<()> {
        let back: ID3D11Texture2D = self.swap.GetBuffer(0)?;
        self.device
            .CreateRenderTargetView(&back, None, Some(&mut self.target))?;
        Ok(())
    }
    unsafe fn texture(
        &self,
        w: u32,
        h: u32,
        format: DXGI_FORMAT,
    ) -> Result<(ID3D11Texture2D, ID3D11ShaderResourceView)> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            ..Default::default()
        };
        let (mut tex, mut view) = (None, None);
        self.device.CreateTexture2D(&desc, None, Some(&mut tex))?;
        let tex = tex.context("video texture")?;
        self.device
            .CreateShaderResourceView(&tex, None, Some(&mut view))?;
        Ok((tex, view.context("video texture view")?))
    }
    pub fn present(&mut self, pixels: &[u8], w: u32, h: u32) -> Result<bool> {
        unsafe {
            ensure!(
                w > 0
                    && h > 0
                    && w.is_multiple_of(2)
                    && h.is_multiple_of(2)
                    && pixels.len() == w as usize * h as usize * 3 / 2,
                "invalid NV12 image"
            );
            let mut r = RECT::default();
            GetClientRect(self.hwnd, &mut r)?;
            let cw = r.right - r.left;
            let ch = r.bottom - r.top;
            if cw <= 0 || ch <= 0 {
                return Ok(false);
            }
            let size = (cw as u32, ch as u32);
            if size != self.window_size {
                self.context.OMSetRenderTargets(None, None);
                self.target = None;
                self.context.Flush();
                self.swap.ResizeBuffers(
                    0,
                    size.0,
                    size.1,
                    DXGI_FORMAT_UNKNOWN,
                    DXGI_SWAP_CHAIN_FLAG(0),
                )?;
                self.window_size = size;
                self.make_target()?;
            }
            if self.image_size != (w, h) {
                self.context.PSSetShaderResources(0, Some(&[None, None]));
                let (y, y_view) = self.texture(w, h, DXGI_FORMAT_R8_UNORM)?;
                let (uv, uv_view) = self.texture(w / 2, h / 2, DXGI_FORMAT_R8G8_UNORM)?;
                self.y = Some(y);
                self.y_view = Some(y_view);
                self.uv = Some(uv);
                self.uv_view = Some(uv_view);
                self.image_size = (w, h);
            }
            self.context.UpdateSubresource(
                self.y.as_ref().unwrap(),
                0,
                None,
                pixels.as_ptr().cast(),
                w,
                0,
            );
            self.context.UpdateSubresource(
                self.uv.as_ref().unwrap(),
                0,
                None,
                pixels[w as usize * h as usize..].as_ptr().cast(),
                w,
                0,
            );
            let target = self.target.as_ref().context("render target")?;
            self.context
                .ClearRenderTargetView(target, &[0.0, 0.0, 0.0, 1.0]);
            self.context
                .OMSetRenderTargets(Some(&[Some(target.clone())]), None);
            let scale = (cw as f32 / w as f32).min(ch as f32 / h as f32);
            let vw = w as f32 * scale;
            let vh = h as f32 * scale;
            self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: (cw as f32 - vw) * 0.5,
                TopLeftY: (ch as f32 - vh) * 0.5,
                Width: vw,
                Height: vh,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShader(&self.pixel, None);
            self.context
                .PSSetShaderResources(0, Some(&[self.y_view.clone(), self.uv_view.clone()]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            self.context.Draw(3, 0);
            self.swap.Present(0, DXGI_PRESENT(0)).ok()?;
            Ok(true)
        }
    }
}
fn compile(source: &str, target: PCSTR) -> Result<Vec<u8>> {
    unsafe {
        let (mut code, mut errors) = (None, None);
        let result = D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            PCSTR::null(),
            None,
            None,
            s!("main"),
            target,
            D3DCOMPILE_OPTIMIZATION_LEVEL3,
            0,
            &mut code,
            Some(&mut errors),
        );
        if let Err(e) = result {
            let detail = errors
                .as_ref()
                .map(|b: &ID3DBlob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        b.GetBufferPointer() as *const u8,
                        b.GetBufferSize(),
                    ))
                    .into_owned()
                })
                .unwrap_or_default();
            return Err(anyhow::anyhow!("shader compilation: {e}: {detail}"));
        }
        let code = code.context("compiled shader")?;
        Ok(
            std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize())
                .to_vec(),
        )
    }
}
