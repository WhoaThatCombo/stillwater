/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Compiled WebRender shaders, kept across launches and shared between painters.
//!
//! On Windows every shader goes GLSL -> HLSL -> DXBC through ANGLE, and the first frame of
//! each new WebRender instance used to compile its shaders on the main thread: 350-600 ms for
//! the first painter (the sidebar or the first page), ~100 ms for each further one (the page,
//! the command bar, split view, Glance). Two things remove that:
//!
//! - Painters drawing with the same GL context (every offscreen context of a window shares
//!   its parent's) share one `Shaders` set, so a program is compiled once per process.
//! - Linked program binaries are written to `<config dir>/shader-cache/<GPU>/<digest>.bin`
//!   (off the main thread) and handed back to GL on the next launch, which links in about a
//!   millisecond. ANGLE refuses a binary from another ANGLE or driver version; WebRender then
//!   reports it through `notify_program_binary_failed`, the file is deleted and the program is
//!   compiled from source as before, so a stale cache only costs what no cache costs.

use std::cell::RefCell;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gleam::gl;
use log::warn;
use webrender::{
    ProgramBinary, ProgramCache, ProgramCacheObserver, ProgramSourceDigest, Shaders,
    SharedShaders, WebRenderOptions,
};

/// Saves and loads program binaries in one folder.
struct DiskProgramCache {
    dir: PathBuf,
}

impl DiskProgramCache {
    fn path(dir: &Path, digest: &ProgramSourceDigest) -> PathBuf {
        dir.join(format!("{digest}.bin"))
    }
}

impl ProgramCacheObserver for DiskProgramCache {
    fn save_shaders_to_disk(&self, entries: Vec<Arc<ProgramBinary>>) {
        // Called from the renderer at the end of a frame: serialise and write elsewhere.
        let dir = self.dir.clone();
        let _ = std::thread::Builder::new()
            .name("ShaderCacheWriter".into())
            .spawn(move || {
                if std::fs::create_dir_all(&dir).is_err() {
                    return;
                }
                for entry in entries {
                    let Ok(bytes) = postcard::to_stdvec(&*entry) else {
                        continue;
                    };
                    let path = Self::path(&dir, entry.source_digest());
                    let temporary = path.with_extension("tmp");
                    if std::fs::write(&temporary, bytes).is_ok() {
                        let _ = std::fs::rename(&temporary, &path);
                    }
                }
            });
    }

    fn set_startup_shaders(&self, _entries: Vec<Arc<ProgramBinary>>) {}

    fn try_load_shader_from_disk(
        &self,
        digest: &ProgramSourceDigest,
        program_cache: &Rc<ProgramCache>,
    ) {
        let path = Self::path(&self.dir, digest);
        let Ok(bytes) = std::fs::read(&path) else {
            return;
        };
        match postcard::from_bytes::<ProgramBinary>(&bytes) {
            Ok(binary) => program_cache.load_program_binary(Arc::new(binary)),
            Err(error) => {
                warn!("Discarding unreadable shader cache entry {path:?}: {error}");
                let _ = std::fs::remove_file(&path);
            },
        }
    }

    fn notify_program_binary_failed(&self, binary: &Arc<ProgramBinary>) {
        let _ = std::fs::remove_file(Self::path(&self.dir, binary.source_digest()));
    }
}

/// The process-wide program cache plus one shared `Shaders` set per GL context.
#[derive(Default)]
pub(crate) struct ShaderStore {
    program_cache: RefCell<Option<Rc<ProgramCache>>>,
    /// Keyed by the address of the context's `gleam` API object, which every offscreen
    /// context of a window shares with the window's own context.
    shaders: RefCell<Vec<(usize, SharedShaders)>>,
}

impl ShaderStore {
    /// The program cache, backed by `<config dir>/shader-cache/<GPU>` when Servo has a config
    /// dir (otherwise in memory only, which still saves the second compile of each program).
    pub(crate) fn program_cache(&self, api: &Rc<dyn gl::Gl>) -> Rc<ProgramCache> {
        self.program_cache
            .borrow_mut()
            .get_or_insert_with(|| {
                // TODO(incognito): a private window's process (started with `--incognito`)
                // must not write anything to disk; it keeps compiled shaders in memory only.
                // Reading the normal profile's cache would be harmless but isn't needed.
                let incognito = std::env::args().any(|arg| arg == "--incognito");
                let config_dir = servo_config::opts::get().config_dir.as_ref().filter(|_| !incognito);
                let observer = config_dir.map(|dir| {
                    // Binaries are only valid for one GPU, driver (ANGLE puts the driver version
                    // in GL_RENDERER) and ANGLE build (GL_VERSION), and for this build of the
                    // browser: one folder each. ANGLE also refuses a mismatching binary, which
                    // is then deleted and compiled from source.
                    let mut hasher = DefaultHasher::new();
                    api.get_string(gl::RENDERER).hash(&mut hasher);
                    api.get_string(gl::VERSION).hash(&mut hasher);
                    env!("CARGO_PKG_VERSION").hash(&mut hasher);
                    std::env::current_exe()
                        .and_then(std::fs::metadata)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .hash(&mut hasher);
                    let dir = dir
                        .join("shader-cache")
                        .join(format!("{:016x}", hasher.finish()));
                    // Folders for other GPUs, drivers or builds are stale: remove them (in the
                    // background, they can hold a few MB each).
                    if let Some(parent) = dir.parent().map(Path::to_path_buf) {
                        let keep = dir.clone();
                        let _ = std::thread::Builder::new()
                            .name("ShaderCacheCleanup".into())
                            .spawn(move || {
                                for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
                                    if entry.path() != keep {
                                        let _ = std::fs::remove_dir_all(entry.path());
                                    }
                                }
                            });
                    }
                    Box::new(DiskProgramCache { dir }) as Box<dyn ProgramCacheObserver>
                });
                ProgramCache::new(observer)
            })
            .clone()
    }

    /// The shaders for `api`'s context, created on first use with `options` (only the
    /// options that pick shader features are read).
    pub(crate) fn shaders(
        &self,
        api: &Rc<dyn gl::Gl>,
        options: &WebRenderOptions,
    ) -> Option<SharedShaders> {
        let key = Rc::as_ptr(api) as *const () as usize;
        if let Some((_, shaders)) = self.shaders.borrow().iter().find(|(other, _)| *other == key) {
            return Some(shaders.clone());
        }
        let mut device = webrender::Device::new(
            api.clone(),
            None,
            options.resource_override_path.clone(),
            options.use_optimized_shaders,
            options.upload_method.clone(),
            options.batched_upload_threshold,
            options.cached_programs.clone(),
            options.allow_texture_storage_support,
            options.allow_texture_swizzling,
            None,
            options.surface_origin_is_top_left,
            options.panic_on_gl_error,
        );
        let _ = device.begin_frame();
        let shaders = Shaders::new(&mut device, api.get_type(), options);
        device.end_frame();
        let shaders = match shaders {
            Ok(shaders) => Rc::new(RefCell::new(shaders)),
            Err(error) => {
                warn!("Could not create shared WebRender shaders: {error:?}");
                return None;
            },
        };
        self.shaders.borrow_mut().push((key, shaders.clone()));
        Some(shaders)
    }
}
