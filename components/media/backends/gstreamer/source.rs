/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::convert::TryFrom;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;
use url::Url;

/// Bytes the source queues before it asks for no more data (enough-data). The media element
/// then stops its fetch and resumes it from the same byte when the queue is half empty
/// (need-data), so a paused or slow player never holds the whole file in memory.
const MAX_SRC_QUEUE_SIZE: u64 = 2 * 1024 * 1024;
/// need-data is signalled when the queue drops to this percentage of MAX_SRC_QUEUE_SIZE.
const SRC_QUEUE_MIN_PERCENT: u32 = 50;

// Implementation sub-module of the GObject
mod imp {
    use std::sync::LazyLock;

    use super::*;

    macro_rules! inner_appsrc_proxy {
        ($fn_name:ident, $return_type:ty) => {
            pub fn $fn_name(&self) -> $return_type {
                self.appsrc.$fn_name()
            }
        };

        ($fn_name:ident, $arg1:ident, $arg1_type:ty, $return_type:ty) => {
            pub fn $fn_name(&self, $arg1: $arg1_type) -> $return_type {
                self.appsrc.$fn_name($arg1)
            }
        };
    }

    #[derive(Debug, Default)]
    struct Position {
        offset: u64,
        requested_offset: u64,
    }

    // The actual data structure that stores our values. This is not accessible
    // directly from the outside.
    pub struct ServoSrc {
        cat: gstreamer::DebugCategory,
        appsrc: gstreamer_app::AppSrc,
        srcpad: gstreamer::GhostPad,
        position: Mutex<Position>,
        seeking: AtomicBool,
        seekable: AtomicBool,
    /// Report the source as bandwidth-limited (network-like), which makes the pipeline buffer.
    bandwidth_limited: AtomicBool,
        size: Mutex<Option<i64>>,
    }

    impl ServoSrc {
        pub fn set_size(&self, size: i64) {
            if self.seeking.load(Ordering::Relaxed) {
                // We ignore set_size requests if we are seeking.
                // The size value is temporarily stored so it
                // is properly set once we are done seeking.
                *self.size.lock().unwrap() = Some(size);
                return;
            }

            if self.appsrc.size() == -1 {
                self.appsrc.set_size(size);
            }
        }

        pub fn set_seekable(&self, seekable: bool) {
            self.seekable.store(seekable, Ordering::Relaxed);
        }

        pub fn set_bandwidth_limited(&self, limited: bool) {
            self.bandwidth_limited.store(limited, Ordering::Relaxed);
        }

        pub fn set_seek_offset<O: IsA<gstreamer::Object>>(&self, parent: &O, offset: u64) -> bool {
            let mut pos = self.position.lock().unwrap();

            if pos.offset == offset || pos.requested_offset != 0 {
                false
            } else {
                self.seeking.store(true, Ordering::Relaxed);
                pos.requested_offset = offset;
                gstreamer::debug!(
                    self.cat,
                    obj = parent,
                    "seeking to offset: {}",
                    pos.requested_offset
                );

                true
            }
        }

        pub fn set_seek_done(&self) {
            self.seeking.store(false, Ordering::Relaxed);

            if let Some(size) = self.size.lock().unwrap().take() &&
                self.appsrc.size() == -1
            {
                self.appsrc.set_size(size);
            }

            let mut pos = self.position.lock().unwrap();
            pos.offset = pos.requested_offset;
            pos.requested_offset = 0;
        }

        pub fn push_buffer<O: IsA<gstreamer::Object>>(
            &self,
            parent: &O,
            data: Vec<u8>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            if self.seeking.load(Ordering::Relaxed) {
                gstreamer::debug!(self.cat, obj = parent, "seek in progress, ignored data");
                return Ok(gstreamer::FlowSuccess::Ok);
            }

            let mut pos = self.position.lock().unwrap(); // will block seeking

            let length = u64::try_from(data.len()).unwrap();
            let buffer_starting_offset = pos.offset;
            pos.offset += length;

            gstreamer::trace!(self.cat, obj = parent, "offset: {}", pos.offset);

            // set the stream size (in bytes) to current offset if
            // size is lesser than it
            if let Ok(size) = u64::try_from(self.appsrc.size()) &&
                pos.offset > size
            {
                gstreamer::debug!(
                    self.cat,
                    obj = parent,
                    "Updating internal size from {} to {}",
                    size,
                    pos.offset
                );
                let new_size = i64::try_from(pos.offset).unwrap();
                self.appsrc.set_size(new_size);
            }

            // One buffer per network chunk, wrapping the data without copying it (it used to be
            // copied into 4 KB buffers, which cost a buffer header per 4 KB and a second copy).
            let mut buffer = gstreamer::Buffer::from_mut_slice(data);
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_offset(buffer_starting_offset);
                buffer.set_offset_end(buffer_starting_offset + length);
            }

            gstreamer::trace!(self.cat, obj = parent, "Pushing buffer {:?}", buffer);

            match self.appsrc.push_buffer(buffer) {
                Ok(success) => Ok(success),
                Err(gstreamer::FlowError::Eos) | Err(gstreamer::FlowError::Flushing) => {
                    Ok(gstreamer::FlowSuccess::Ok)
                },
                Err(error) => Err(error),
            }
        }

        /// Whether the queue of data waiting to enter the pipeline is full.
        pub fn queue_full(&self) -> bool {
            self.appsrc.current_level_bytes() >= self.appsrc.max_bytes()
        }

        inner_appsrc_proxy!(end_of_stream, Result<gstreamer::FlowSuccess, gstreamer::FlowError>);
        inner_appsrc_proxy!(set_callbacks, callbacks, gstreamer_app::AppSrcCallbacks, ());

        fn query(&self, pad: &gstreamer::GhostPad, query: &mut gstreamer::QueryRef) -> bool {
            gstreamer::log!(self.cat, obj = pad, "Handling query {:?}", query);

            // In order to make buffering/downloading work as we want, apart from
            // setting the appropriate flags on the player playbin,
            // the source:
            //
            // 1. Announces seekability when the media element confirmed it.
            // 2. Assumes seekable = true as default.
            // 3. Keeps assuming bandwidth limited.
            // 4. set_seekable is called when range requests are supported or not.
            let ret = match query.view_mut() {
                gstreamer::QueryViewMut::Scheduling(ref mut q) => {
                    let seekability_flag = if self.seekable.load(Ordering::Relaxed) {
                        gstreamer::SchedulingFlags::SEEKABLE
                    } else {
                        gstreamer::SchedulingFlags::SEQUENTIAL
                    };
                    let bandwidth_flag = if self.bandwidth_limited.load(Ordering::Relaxed) {
                        gstreamer::SchedulingFlags::BANDWIDTH_LIMITED
                    } else {
                        gstreamer::SchedulingFlags::empty()
                    };
                    q.set(
                        seekability_flag | bandwidth_flag,
                        1,
                        -1,
                        0,
                    );
                    q.add_scheduling_modes([gstreamer::PadMode::Push]);
                    true
                },
                _ => gstreamer::Pad::query_default(pad, Some(&*self.obj()), query),
            };

            if ret {
                gstreamer::log!(self.cat, obj = pad, "Handled query {:?}", query);
            } else {
                gstreamer::info!(self.cat, obj = pad, "Didn't handle query {:?}", query);
            }
            ret
        }
    }

    // Basic declaration of our type for the GObject type system
    #[glib::object_subclass]
    impl ObjectSubclass for ServoSrc {
        const NAME: &'static str = "ServoSrc";
        type Type = super::ServoSrc;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        // Called once at the very beginning of instantiation of each instance and
        // creates the data structure that contains all our state
        fn with_class(klass: &Self::Class) -> Self {
            let app_src = gstreamer::ElementFactory::make("appsrc")
                .build()
                .map(|elem| elem.downcast::<gstreamer_app::AppSrc>().unwrap())
                .expect("Could not create appsrc element");

            let pad_templ = klass.pad_template("src").unwrap();
            let ghost_pad = gstreamer::GhostPad::builder_from_template(&pad_templ)
                .query_function(|pad, parent, query| {
                    ServoSrc::catch_panic_pad_function(
                        parent,
                        || false,
                        |servosrc| servosrc.query(pad, query),
                    )
                })
                .build();

            Self {
                cat: gstreamer::DebugCategory::new(
                    "servosrc",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo source"),
                ),
                appsrc: app_src,
                srcpad: ghost_pad,
                position: Mutex::new(Default::default()),
                seeking: AtomicBool::new(false),
                seekable: AtomicBool::new(true),
            bandwidth_limited: AtomicBool::new(true),
                size: Mutex::new(None),
            }
        }
    }

    // The ObjectImpl trait provides the setters/getters for GObject properties.
    // Here we need to provide the values that are internally stored back to the
    // caller, or store whatever new value the caller is providing.
    //
    // This maps between the GObject properties and our internal storage of the
    // corresponding values of the properties.
    impl ObjectImpl for ServoSrc {
        // Called right after construction of a new instance
        fn constructed(&self) {
            // Call the parent class' ::constructed() implementation first
            self.parent_constructed();

            self.obj()
                .add(&self.appsrc)
                .expect("Could not add appsrc element to bin");

            let target_pad = self.appsrc.static_pad("src");
            self.srcpad.set_target(target_pad.as_ref()).unwrap();

            self.obj()
                .add_pad(&self.srcpad)
                .expect("Could not add source pad to bin");

            self.appsrc.set_caps(None::<&gstreamer::Caps>);
            self.appsrc.set_max_bytes(MAX_SRC_QUEUE_SIZE);
            self.appsrc.set_min_percent(SRC_QUEUE_MIN_PERCENT);
            self.appsrc.set_block(false);
            self.appsrc.set_format(gstreamer::Format::Bytes);
            self.appsrc
                .set_stream_type(gstreamer_app::AppStreamType::Seekable);

            self.obj()
                .set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoSrc {}

    // Implementation of gstreamer::Element virtual methods
    impl ElementImpl for ServoSrc {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo Media Source",
                        "Source/Audio/Video",
                        "Feed player with media data",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                let caps = gstreamer::Caps::new_any();
                let src_pad_template = gstreamer::PadTemplate::new(
                    "src",
                    gstreamer::PadDirection::Src,
                    gstreamer::PadPresence::Always,
                    &caps,
                )
                .unwrap();

                vec![src_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    // Implementation of gstreamer::Bin virtual methods
    impl BinImpl for ServoSrc {}

    impl URIHandlerImpl for ServoSrc {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servosrc"]
        }

        fn uri(&self) -> Option<String> {
            Some("servosrc://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if let Ok(uri) = Url::parse(uri) &&
                uri.scheme() == "servosrc"
            {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{:?}'", uri,).as_str(),
            ))
        }
    }
}

// Public part of the ServoSrc type. This behaves like a normal
// GObject binding
glib::wrapper! {
    pub struct ServoSrc(ObjectSubclass<imp::ServoSrc>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoSrc {}
unsafe impl Sync for ServoSrc {}

impl ServoSrc {
    pub fn set_size(&self, size: i64) {
        self.imp().set_size(size);
    }

    pub fn set_seekable(&self, seekable: bool) {
        self.imp().set_seekable(seekable);
    }

    pub fn set_bandwidth_limited(&self, limited: bool) {
        self.imp().set_bandwidth_limited(limited);
    }

    pub fn set_seek_offset(&self, offset: u64) -> bool {
        self.imp().set_seek_offset(self, offset)
    }

    pub fn set_seek_done(&self) {
        self.imp().set_seek_done();
    }

    pub fn push_buffer(
        &self,
        data: Vec<u8>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().push_buffer(self, data)
    }

    pub fn queue_full(&self) -> bool {
        self.imp().queue_full()
    }

    pub fn push_end_of_stream(&self) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().end_of_stream()
    }

    pub fn set_callbacks(&self, callbacks: gstreamer_app::AppSrcCallbacks) {
        self.imp().set_callbacks(callbacks)
    }
}

// Registers the type for our element, and then registers in GStreamer
// under the name "servosrc" for being able to instantiate it via e.g.
// gstreamer::ElementFactory::make().
pub fn register_servo_src() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servosrc",
        gstreamer::Rank::NONE,
        ServoSrc::static_type(),
    )
}
