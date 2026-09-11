//! The Direct3D 11 device both capture paths share, and the staging texture
//! that carries a captured surface off the GPU into a plain `Vec<u8>`.

use windows::Win32::Foundation::{E_POINTER, HMODULE};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::Win32::Graphics::Dxgi::IDXGIAdapter;
use windows::core::Result as WinResult;

use crate::Unavailable;

/// A Direct3D 11 device and its immediate context. Neither is thread-safe, so
/// one of these belongs to exactly one capture thread for its whole life.
pub(super) struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
}

impl Gpu {
    /// Naming an `adapter` pins the device to the GPU that owns a particular
    /// output, which is what Desktop Duplication needs on a machine with more
    /// than one. `D3D11CreateDevice` insists the driver type be `UNKNOWN`
    /// whenever an adapter is given, and picks one itself otherwise.
    pub(super) fn create(adapter: Option<&IDXGIAdapter>) -> Result<Gpu, Unavailable> {
        let driver = if adapter.is_some() {
            D3D_DRIVER_TYPE_UNKNOWN
        } else {
            D3D_DRIVER_TYPE_HARDWARE
        };
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        // SAFETY: every out parameter outlives the call, and none is read
        // unless the call reports success.
        unsafe {
            D3D11CreateDevice(
                adapter,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .map_err(|error| Unavailable::Failed(format!("no Direct3D 11 device: {error}")))?;

        match (device, context) {
            (Some(device), Some(context)) => Ok(Gpu { device, context }),
            _ => Err(Unavailable::Failed(
                "Direct3D 11 returned no device".to_string(),
            )),
        }
    }

    pub(super) fn device(&self) -> &ID3D11Device {
        &self.device
    }
}

/// The CPU-readable texture a frame is copied through. Kept between frames:
/// allocating one per frame costs more than the copy itself.
pub(super) struct Staging {
    texture: Option<ID3D11Texture2D>,
    size: (u32, u32),
}

/// One frame off the GPU, in tightly packed BGRA rows.
pub(super) struct Pixels {
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) bgra: Vec<u8>,
}

impl Staging {
    pub(super) fn new() -> Staging {
        Staging {
            texture: None,
            size: (0, 0),
        }
    }

    /// Copies `source` into the staging texture and reads it back with the
    /// row padding the driver chose taken out, so the stride is `width * 4`.
    pub(super) fn read(&mut self, gpu: &Gpu, source: &ID3D11Texture2D) -> WinResult<Pixels> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `GetDesc` only writes the descriptor it is handed.
        unsafe { source.GetDesc(&mut desc) };
        let (width, height) = (desc.Width, desc.Height);

        if self.texture.is_none() || self.size != (width, height) {
            self.texture = Some(Self::allocate(gpu, &desc)?);
            self.size = (width, height);
        }
        let staging = self
            .texture
            .as_ref()
            .ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))?;

        // SAFETY: both textures belong to `gpu` and, having been described the
        // same way, are the same size and format.
        unsafe { gpu.context.CopyResource(staging, source) };

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture was created for CPU reads, and `mapped`
        // outlives the call.
        unsafe {
            gpu.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }?;

        let stride = width as usize * 4;
        let mut bgra = vec![0u8; stride * height as usize];
        // SAFETY: the mapping covers `RowPitch * height` bytes and `RowPitch`
        // is never below the `stride` bytes copied out of each row; the
        // destination was allocated at exactly `stride * height`.
        unsafe {
            for row in 0..height as usize {
                let from = mapped
                    .pData
                    .cast::<u8>()
                    .add(row * mapped.RowPitch as usize);
                std::ptr::copy_nonoverlapping(from, bgra.as_mut_ptr().add(row * stride), stride);
            }
            gpu.context.Unmap(staging, 0);
        }

        Ok(Pixels {
            width,
            height,
            bgra,
        })
    }

    fn allocate(gpu: &Gpu, source: &D3D11_TEXTURE2D_DESC) -> WinResult<ID3D11Texture2D> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: source.Width,
            Height: source.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: source.Format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut texture: Option<ID3D11Texture2D> = None;
        // SAFETY: the descriptor and the out parameter both outlive the call.
        unsafe { gpu.device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
        texture.ok_or_else(|| windows::core::Error::from_hresult(E_POINTER))
    }
}
