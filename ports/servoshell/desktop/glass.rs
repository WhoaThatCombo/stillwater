/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Windows 11 "Mica" glass window background, like Zen Browser's on Windows 11.
//!
//! A glass window is created with `WS_EX_NOREDIRECTIONBITMAP`, so (with the patched surfman in
//! `third_party/surfman`) ANGLE presents it through DirectComposition with a premultiplied-alpha
//! swap chain, and DWM draws its Mica backdrop (`DWMWA_SYSTEMBACKDROP_TYPE`) behind whatever
//! Stillwater leaves translucent: the window is cleared to transparent, the sidebar and the
//! frame around the page are a light tint, and pages stay opaque. Windows dims Mica itself
//! while the window is inactive.
//!
//! Solid (the fallback, and the default before Windows 11 22H2) is exactly the old look.
//! Settings → Appearance → Window background chooses; it applies to windows opened after the
//! change. `STILLWATER_GLASS=0/1` overrides the setting.

use std::cell::RefCell;
use std::sync::Arc;

use egui::{Color32, PaintCallback, Rect};
use egui_glow::CallbackFn;

/// Windows 11 22H2 (build 22621) brought `DWMWA_SYSTEMBACKDROP_TYPE`.
#[cfg(target_os = "windows")]
const FIRST_MICA_BUILD: u32 = 22621;

/// Whether this Windows can draw a Mica backdrop (Windows 11 22H2 or later).
pub(crate) fn supported() -> bool {
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::SystemInformation::{GetVersionExW, OSVERSIONINFOW};
        // The app manifest declares Windows 10/11 support, so this reports the real build.
        let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
        info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
        (unsafe { GetVersionExW(&mut info) } != 0) && info.dwBuildNumber >= FIRST_MICA_BUILD
    }
    #[cfg(not(target_os = "windows"))]
    false
}

/// Whether a new window should be glass: `STILLWATER_GLASS`, else the setting, else the
/// default for this Windows.
pub(crate) fn wanted() -> bool {
    if !cfg!(target_os = "windows") {
        return false;
    }
    match std::env::var("STILLWATER_GLASS").as_deref() {
        Ok("0") => return false,
        Ok("1") => return true,
        _ => {},
    }
    crate::desktop::settings::saved_window_background()
        .map_or_else(supported, |choice| choice == "glass")
}

/// Turn on the Mica backdrop of a glass window and match its tint to the theme.
#[cfg(target_os = "windows")]
pub(crate) fn enable_backdrop(hwnd: isize, dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::{
        DWMSBT_MAINWINDOW, DWMWA_SYSTEMBACKDROP_TYPE, DwmExtendFrameIntoClientArea,
        DwmSetWindowAttribute,
    };
    use windows_sys::Win32::UI::Controls::MARGINS;
    let backdrop = DWMSBT_MAINWINDOW;
    let margins = MARGINS {
        cxLeftWidth: -1,
        cxRightWidth: -1,
        cyTopHeight: -1,
        cyBottomHeight: -1,
    };
    unsafe {
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_SYSTEMBACKDROP_TYPE as u32,
            &backdrop as *const _ as *const _,
            std::mem::size_of_val(&backdrop) as u32,
        );
        DwmExtendFrameIntoClientArea(hwnd as _, &margins);
    }
    set_dark(hwnd, dark);
}

/// Dark or light Mica (`DWMWA_USE_IMMERSIVE_DARK_MODE`).
#[cfg(target_os = "windows")]
pub(crate) fn set_dark(hwnd: isize, dark: bool) {
    use windows_sys::Win32::Graphics::Dwm::{DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute};
    let value: i32 = dark.into();
    unsafe {
        DwmSetWindowAttribute(
            hwnd as _,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            &value as *const _ as *const _,
            std::mem::size_of_val(&value) as u32,
        );
    }
}

/// The window's backdrop and dark-mode attributes as DWM reports them, for the log.
#[cfg(target_os = "windows")]
pub(crate) fn describe(hwnd: isize) -> String {
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmGetWindowAttribute,
    };
    let get = |attribute: i32| {
        let mut value: i32 = -1;
        let result = unsafe {
            DwmGetWindowAttribute(
                hwnd as _,
                attribute as u32,
                &mut value as *mut _ as *mut _,
                std::mem::size_of_val(&value) as u32,
            )
        };
        if result < 0 { format!("error 0x{result:08x}") } else { value.to_string() }
    };
    let ex_style = unsafe {
        windows_sys::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd as _,
            windows_sys::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
        )
    };
    format!(
        "systembackdrop_type={} immersive_dark_mode={} no_redirection_bitmap={} direct_composition={}",
        get(DWMWA_SYSTEMBACKDROP_TYPE),
        get(DWMWA_USE_IMMERSIVE_DARK_MODE),
        ex_style as u32 & windows_sys::Win32::UI::WindowsAndMessaging::WS_EX_NOREDIRECTIONBITMAP != 0,
        surfman::angle::composition::window_surface_uses_direct_composition(),
    )
}

/// The window tint for glass: Zen's backdrop colours made translucent, so Mica shows through.
/// `rgb` is the solid colour (already tinted by the space); the result is unmultiplied RGBA.
pub(crate) fn tint(rgb: [u8; 3], dark: bool, incognito: bool) -> [u8; 4] {
    let alpha = match (incognito, dark) {
        (true, _) => 0.55,
        (false, true) => 0.30,
        (false, false) => 0.40,
    };
    [rgb[0], rgb[1], rgb[2], (alpha * 255.0f32).round() as u8]
}

/// The program that `fill` draws with, made once per GL context.
struct FillProgram {
    gl: std::sync::Weak<glow::Context>,
    program: glow::Program,
    rect: Option<glow::UniformLocation>,
    radius: Option<glow::UniformLocation>,
    color: Option<glow::UniformLocation>,
    amount: Option<glow::UniformLocation>,
}

thread_local! {
    static PROGRAMS: RefCell<Vec<FillProgram>> = const { RefCell::new(Vec::new()) };
}

const VERTEX_SHADER: &str = "#version 300 es
void main() {
    vec2 corner = vec2(float(gl_VertexID & 1), float((gl_VertexID >> 1) & 1));
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
";

/// Coverage: the whole rect, or (with a radius) only what lies outside the rect's rounded
/// corners, anti-aliased.
const FRAGMENT_SHADER: &str = "#version 300 es
precision highp float;
uniform vec4 u_rect;
uniform float u_radius;
uniform vec4 u_color;
uniform float u_amount;
out vec4 color;
void main() {
    float cover = 1.0;
    if (u_radius > 0.0) {
        vec2 half_size = u_rect.zw * 0.5;
        vec2 p = gl_FragCoord.xy - (u_rect.xy + half_size);
        vec2 q = abs(p) - (half_size - vec2(u_radius));
        float d = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - u_radius;
        cover = clamp(d + 0.5, 0.0, 1.0);
    }
    float c = cover * u_amount;
    if (c <= 0.0) discard;
    color = vec4(u_color.rgb * c, c);
}
";

/// A paint callback that moves the pixels under `rect` towards `color` (premultiplied) by
/// `amount`: `dst = color * a + dst * (1 - a)`, alpha included. Unlike egui's blending this
/// replaces what is underneath, so a translucent glass tint covers the page (the top bar, the
/// startup animation) and the page's corners (with `corner_radius`) instead of mixing with it.
pub(crate) fn fill(rect: Rect, color: Color32, amount: f32, corner_radius: Option<f32>) -> PaintCallback {
    let rgba = color.to_normalized_gamma_f32();
    PaintCallback {
        rect,
        callback: Arc::new(CallbackFn::new(move |info, painter| {
            let viewport = info.viewport_in_pixels();
            let radius = corner_radius.map_or(0.0, |radius| radius * info.pixels_per_point);
            draw(
                painter.gl(),
                [
                    viewport.left_px as f32,
                    viewport.from_bottom_px as f32,
                    viewport.width_px as f32,
                    viewport.height_px as f32,
                ],
                radius,
                rgba,
                amount.clamp(0.0, 1.0),
            );
        })),
    }
}

fn draw(gl: &Arc<glow::Context>, rect: [f32; 4], radius: f32, rgba: [f32; 4], amount: f32) {
    use glow::HasContext as _;
    PROGRAMS.with_borrow_mut(|programs| {
        programs.retain(|program| program.gl.strong_count() > 0);
        let index = match programs
            .iter()
            .position(|program| std::ptr::eq(program.gl.as_ptr(), Arc::as_ptr(gl)))
        {
            Some(index) => index,
            None => {
                let Some(program) = compile(gl) else {
                    return;
                };
                programs.push(program);
                programs.len() - 1
            },
        };
        let program = &programs[index];
        unsafe {
            gl.use_program(Some(program.program));
            gl.uniform_4_f32(program.rect.as_ref(), rect[0], rect[1], rect[2], rect[3]);
            gl.uniform_1_f32(program.radius.as_ref(), radius);
            gl.uniform_4_f32(program.color.as_ref(), rgba[0], rgba[1], rgba[2], rgba[3]);
            gl.uniform_1_f32(program.amount.as_ref(), amount);
            gl.enable(glow::BLEND);
            gl.blend_equation(glow::FUNC_ADD);
            gl.blend_func_separate(
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::CONSTANT_ALPHA,
                glow::ONE_MINUS_SRC_ALPHA,
            );
            gl.blend_color(0.0, 0.0, 0.0, rgba[3]);
            gl.bind_vertex_array(None);
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            // egui restores its own state after a callback; this one is not part of it.
            gl.blend_color(0.0, 0.0, 0.0, 0.0);
        }
    });
}

fn compile(gl: &Arc<glow::Context>) -> Option<FillProgram> {
    use glow::HasContext as _;
    unsafe {
        let program = gl.create_program().ok()?;
        let mut shaders = Vec::new();
        for (kind, source) in [
            (glow::VERTEX_SHADER, VERTEX_SHADER),
            (glow::FRAGMENT_SHADER, FRAGMENT_SHADER),
        ] {
            let shader = gl.create_shader(kind).ok()?;
            gl.shader_source(shader, source);
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                log::warn!("glass fill shader: {}", gl.get_shader_info_log(shader));
                return None;
            }
            gl.attach_shader(program, shader);
            shaders.push(shader);
        }
        gl.link_program(program);
        for shader in shaders {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if !gl.get_program_link_status(program) {
            log::warn!("glass fill program: {}", gl.get_program_info_log(program));
            return None;
        }
        Some(FillProgram {
            gl: Arc::downgrade(gl),
            rect: gl.get_uniform_location(program, "u_rect"),
            radius: gl.get_uniform_location(program, "u_radius"),
            color: gl.get_uniform_location(program, "u_color"),
            amount: gl.get_uniform_location(program, "u_amount"),
            program,
        })
    }
}

/// Clear the window to transparent before egui draws, so only what Stillwater paints covers
/// the backdrop (the swap chain keeps old frames).
pub(crate) fn clear(gl: &glow::Context) {
    use glow::HasContext as _;
    unsafe {
        gl.disable(glow::SCISSOR_TEST);
        gl.clear_color(0.0, 0.0, 0.0, 0.0);
        gl.clear(glow::COLOR_BUFFER_BIT);
    }
}
