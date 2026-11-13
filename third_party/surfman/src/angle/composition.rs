// surfman/src/angle/composition.rs
//
//! Stillwater: window surfaces presented through DirectComposition, so the window can be
//! translucent over a DWM system backdrop (Windows 11 Mica).
//!
//! ANGLE's own `EGL_ANGLE_direct_composition` is not available here: surfman hands ANGLE its
//! D3D11 device (`EGL_PLATFORM_DEVICE_EXT`), and ANGLE only offers the extension for devices it
//! created itself. So for a window created with `WS_EX_NOREDIRECTIONBITMAP` surfman makes a
//! premultiplied-alpha composition swap chain on its device, shows it in a DirectComposition
//! visual on the window, and renders through an EGL pbuffer wrapping the swap chain's back
//! buffer (`EGL_D3D_TEXTURE_ANGLE`). GL draws bottom-up, so the visual flips it vertically.

use euclid::default::Size2D;
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use winapi::shared::dxgi::IDXGIDevice;
use winapi::shared::dxgi1_2::{
    IDXGIFactory2, IDXGISwapChain1, DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1,
};
use winapi::shared::dxgiformat::DXGI_FORMAT_B8G8R8A8_UNORM;
use winapi::shared::dxgitype::{
    DXGI_SAMPLE_DESC, DXGI_USAGE_RENDER_TARGET_OUTPUT, DXGI_USAGE_SHADER_INPUT,
};
use winapi::shared::dxgi::DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL;
use winapi::shared::minwindef::{FALSE, TRUE};
use winapi::shared::windef::{HWND, RECT};
use winapi::shared::winerror::{HRESULT, SUCCEEDED};
use winapi::um::dcommon::D2D1_MATRIX_3X2_F;
use winapi::um::d3d11::{ID3D11Device, ID3D11Texture2D};
use winapi::um::dcomp::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use winapi::um::unknwnbase::IUnknown;
use winapi::um::winuser::{GetClientRect, GetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOREDIRECTIONBITMAP};
use winapi::Interface;
use wio::com::ComPtr;

static USED: AtomicBool = AtomicBool::new(false);

/// Whether any window surface so far presents through DirectComposition (with alpha).
pub fn window_surface_uses_direct_composition() -> bool {
    USED.load(Ordering::Relaxed)
}

/// A window without a redirection bitmap gets a composition surface.
pub(crate) fn wanted(hwnd: HWND) -> bool {
    if hwnd.is_null() {
        return false;
    }
    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    style & WS_EX_NOREDIRECTIONBITMAP != 0
}

pub(crate) fn client_size(hwnd: HWND) -> Size2D<i32> {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    unsafe { GetClientRect(hwnd, &mut rect) };
    Size2D::new((rect.right - rect.left).max(1), (rect.bottom - rect.top).max(1))
}

fn check(result: HRESULT) -> Result<(), HRESULT> {
    if SUCCEEDED(result) {
        Ok(())
    } else {
        Err(result)
    }
}

pub(crate) struct Composition {
    dcomp: ComPtr<IDCompositionDevice>,
    _target: ComPtr<IDCompositionTarget>,
    visual: ComPtr<IDCompositionVisual>,
    pub(crate) swap_chain: ComPtr<IDXGISwapChain1>,
    pub(crate) back_buffer: ComPtr<ID3D11Texture2D>,
    /// The visual changed (a new swap chain): commit with the next present.
    needs_commit: bool,
}

impl Composition {
    pub(crate) unsafe fn new(
        d3d11: &ComPtr<ID3D11Device>,
        hwnd: HWND,
        size: Size2D<i32>,
    ) -> Result<Composition, HRESULT> {
        let dxgi_device: ComPtr<IDXGIDevice> = d3d11.cast()?;
        let mut dcomp: *mut IDCompositionDevice = ptr::null_mut();
        check(DCompositionCreateDevice(
            dxgi_device.as_raw(),
            &IDCompositionDevice::uuidof(),
            &mut dcomp as *mut _ as *mut _,
        ))?;
        let dcomp = ComPtr::from_raw(dcomp);
        let mut target: *mut IDCompositionTarget = ptr::null_mut();
        check(dcomp.CreateTargetForHwnd(hwnd, TRUE, &mut target))?;
        let target = ComPtr::from_raw(target);
        let mut visual: *mut IDCompositionVisual = ptr::null_mut();
        check(dcomp.CreateVisual(&mut visual))?;
        let visual = ComPtr::from_raw(visual);
        let (swap_chain, back_buffer) = Self::create_swap_chain(d3d11, size)?;
        check(visual.SetContent(swap_chain.as_raw() as *const IUnknown))?;
        check(target.SetRoot(visual.as_raw()))?;
        let composition = Composition {
            dcomp,
            _target: target,
            visual,
            swap_chain,
            back_buffer,
            needs_commit: false,
        };
        composition.flip(size.height)?;
        check(composition.dcomp.Commit())?;
        USED.store(true, Ordering::Relaxed);
        Ok(composition)
    }

    unsafe fn create_swap_chain(
        d3d11: &ComPtr<ID3D11Device>,
        size: Size2D<i32>,
    ) -> Result<(ComPtr<IDXGISwapChain1>, ComPtr<ID3D11Texture2D>), HRESULT> {
        let dxgi_device: ComPtr<IDXGIDevice> = d3d11.cast()?;
        let mut adapter = ptr::null_mut();
        check(dxgi_device.GetAdapter(&mut adapter))?;
        let adapter = ComPtr::from_raw(adapter);
        let mut factory: *mut IDXGIFactory2 = ptr::null_mut();
        check(adapter.GetParent(
            &IDXGIFactory2::uuidof(),
            &mut factory as *mut _ as *mut _,
        ))?;
        let factory = ComPtr::from_raw(factory);
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: size.width.max(1) as u32,
            Height: size.height.max(1) as u32,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            Stereo: FALSE,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT | DXGI_USAGE_SHADER_INPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: 0,
        };
        let mut swap_chain: *mut IDXGISwapChain1 = ptr::null_mut();
        check(factory.CreateSwapChainForComposition(
            d3d11.as_raw() as *mut IUnknown,
            &desc,
            ptr::null_mut(),
            &mut swap_chain,
        ))?;
        let swap_chain = ComPtr::from_raw(swap_chain);
        let mut back_buffer: *mut ID3D11Texture2D = ptr::null_mut();
        check(swap_chain.GetBuffer(
            0,
            &ID3D11Texture2D::uuidof(),
            &mut back_buffer as *mut _ as *mut _,
        ))?;
        Ok((swap_chain, ComPtr::from_raw(back_buffer)))
    }

    /// GL's rows run bottom-up in the buffer: show it upside down, which is the right way up.
    unsafe fn flip(&self, height: i32) -> Result<(), HRESULT> {
        let matrix = D2D1_MATRIX_3X2_F {
            matrix: [[1.0, 0.0], [0.0, -1.0], [0.0, height as f32]],
        };
        check(self.visual.SetTransform_1(&matrix))
    }

    /// A new swap chain at `size` (the old one goes once nothing uses its buffer). Simpler
    /// than `ResizeBuffers`, which needs every reference ANGLE holds to the buffers gone.
    pub(crate) unsafe fn resize(
        &mut self,
        d3d11: &ComPtr<ID3D11Device>,
        size: Size2D<i32>,
    ) -> Result<(), HRESULT> {
        let (swap_chain, back_buffer) = Self::create_swap_chain(d3d11, size)?;
        check(self.visual.SetContent(swap_chain.as_raw() as *const IUnknown))?;
        self.flip(size.height)?;
        self.swap_chain = swap_chain;
        self.back_buffer = back_buffer;
        self.needs_commit = true;
        Ok(())
    }

    pub(crate) unsafe fn present(&mut self) -> Result<(), HRESULT> {
        check(self.swap_chain.Present(1, 0))?;
        if std::mem::take(&mut self.needs_commit) {
            check(self.dcomp.Commit())?;
        }
        Ok(())
    }
}
