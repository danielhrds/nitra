//! Dear ImGui's own textures (font atlas, icons) on D3D11, for
//! dear-imgui-rs's managed-texture protocol. Draw commands then reference a
//! texture by its shader resource view pointer, which is what the official
//! DX11 backend binds. These are UI pixels (a few hundred KiB once), not
//! video.

use std::collections::{HashMap, HashSet};

use dear_imgui_rs::render::{
    PendingFrame, ReconciledFrame, SnapshotTextureId, SynchronousRendererConsumer, TextureOp,
    TextureUploadIdentity,
};
use dear_imgui_rs::texture::TextureFormat;
use dear_imgui_rs::{Context, TextureId};
use gpu::D3d11Device;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_SHADER_RESOURCE, D3D11_BOX, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, ID3D11DeviceContext, ID3D11ShaderResourceView, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::core::Interface;

struct Texture {
    texture: ID3D11Texture2D,
    view: ID3D11ShaderResourceView,
    width: u32,
    height: u32,
    upload: TextureUploadIdentity,
}

impl Texture {
    fn id(&self) -> TextureId {
        TextureId::new(self.view.as_raw() as u64)
    }
}

pub struct TextureManager {
    device: D3d11Device,
    context: ID3D11DeviceContext,
    consumer: Option<SynchronousRendererConsumer>,
    textures: HashMap<SnapshotTextureId, Texture>,
    destroyed: HashSet<SnapshotTextureId>,
}

/// RGBA rows of `width` pixels; Alpha8 is expanded to white with alpha.
fn rgba_rows(
    format: TextureFormat,
    width: usize,
    rows: usize,
    pitch: usize,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * rows * 4);
    for row in 0..rows {
        let start = row * pitch;
        match format {
            TextureFormat::RGBA32 => out.extend_from_slice(&data[start..start + width * 4]),
            TextureFormat::Alpha8 => {
                for &alpha in &data[start..start + width] {
                    out.extend_from_slice(&[255, 255, 255, alpha]);
                }
            }
        }
    }
    out
}

impl TextureManager {
    pub fn new(device: &D3d11Device, context: &mut Context) -> Result<Self, String> {
        Ok(Self {
            device: device.clone(),
            context: device.immediate_context().map_err(|e| e.to_string())?,
            consumer: Some(
                context
                    .create_synchronous_renderer_consumer()
                    .map_err(|e| e.to_string())?,
            ),
            textures: HashMap::new(),
            destroyed: HashSet::new(),
        })
    }

    pub fn consumer(&self) -> &SynchronousRendererConsumer {
        self.consumer
            .as_ref()
            .expect("texture manager is shut down")
    }

    /// Applies the frame's texture requests and returns the drawable frame.
    pub fn reconcile<'ctx>(
        &mut self,
        frame: PendingFrame<'ctx>,
    ) -> Result<ReconciledFrame<'ctx>, String> {
        let mut feedback = Vec::with_capacity(frame.texture_requests().len());
        for request in frame.texture_requests() {
            let key = request.texture();
            let result = match request.operation() {
                _ if self.destroyed.contains(&key) => Ok(request.superseded()),
                TextureOp::Create {
                    format,
                    width,
                    height,
                    row_pitch,
                    pixels,
                } => {
                    let upload = request.upload_identity().expect("create has an identity");
                    match self.textures.get(&key) {
                        Some(texture) if texture.upload == upload => request.uploaded(texture.id()),
                        _ => {
                            let rgba = rgba_rows(
                                *format,
                                *width as usize,
                                *height as usize,
                                *row_pitch,
                                pixels,
                            );
                            let texture = self.create(*width, *height, &rgba, upload)?;
                            let id = texture.id();
                            self.textures.insert(key, texture);
                            request.uploaded(id)
                        }
                    }
                }
                TextureOp::Update { format, rects, .. } => {
                    let upload = request.upload_identity().expect("update has an identity");
                    match self.textures.get_mut(&key) {
                        None => Ok(request.retry()),
                        Some(texture) if texture.upload == upload => request.uploaded(texture.id()),
                        Some(texture) => {
                            for rect in rects {
                                let r = rect.rect;
                                let rgba = rgba_rows(
                                    *format,
                                    usize::from(r.w),
                                    usize::from(r.h),
                                    rect.row_pitch,
                                    &rect.data,
                                );
                                let region = D3D11_BOX {
                                    left: u32::from(r.x),
                                    top: u32::from(r.y),
                                    front: 0,
                                    right: u32::from(r.x) + u32::from(r.w),
                                    bottom: u32::from(r.y) + u32::from(r.h),
                                    back: 1,
                                };
                                if region.right > texture.width || region.bottom > texture.height {
                                    return Err("texture update outside the texture".to_owned());
                                }
                                // SAFETY: the box lies inside the texture
                                // (checked); `rgba` holds w x h tightly
                                // packed RGBA pixels.
                                unsafe {
                                    self.context.UpdateSubresource(
                                        &texture.texture,
                                        0,
                                        Some(&region),
                                        rgba.as_ptr().cast(),
                                        u32::from(r.w) * 4,
                                        0,
                                    );
                                }
                            }
                            texture.upload = upload;
                            request.uploaded(texture.id())
                        }
                    }
                }
                TextureOp::Destroy => {
                    self.destroyed.insert(key);
                    self.textures.remove(&key);
                    request.destroyed()
                }
            };
            feedback.push(result.map_err(|e| e.to_string())?);
        }
        frame
            .reconcile_texture_feedback(feedback)
            .map_err(|e| e.to_string())
    }

    fn create(
        &self,
        width: u32,
        height: u32,
        rgba: &[u8],
        upload: TextureUploadIdentity,
    ) -> Result<Texture, String> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let initial = D3D11_SUBRESOURCE_DATA {
            pSysMem: rgba.as_ptr().cast(),
            SysMemPitch: width * 4,
            SysMemSlicePitch: 0,
        };
        let (mut texture, mut view) = (None, None);
        // SAFETY: valid device; `rgba` holds width x height RGBA pixels and
        // outlives the call; out-pointers are live locals.
        unsafe {
            self.device
                .raw()
                .CreateTexture2D(&desc, Some(&initial), Some(&mut texture))
                .map_err(|e| e.to_string())?;
            let texture = texture.as_ref().expect("texture created");
            self.device
                .raw()
                .CreateShaderResourceView(texture, None, Some(&mut view))
                .map_err(|e| e.to_string())?;
        }
        Ok(Texture {
            texture: texture.expect("texture created"),
            view: view.expect("view created"),
            width,
            height,
            upload,
        })
    }

    /// Releases every texture and the consumer (before the context goes).
    pub fn shutdown(&mut self, context: &mut Context) -> Result<(), String> {
        let Some(consumer) = self.consumer.as_ref() else {
            return Ok(());
        };
        let reset = context
            .prepare_renderer_texture_reset(consumer)
            .map_err(|e| e.to_string())?;
        self.textures.clear();
        reset.commit();
        self.destroyed.clear();
        drop(self.consumer.take());
        context
            .poll_snapshot_completions()
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
