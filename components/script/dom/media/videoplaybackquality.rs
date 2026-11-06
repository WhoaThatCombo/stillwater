/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::PerformanceBinding::DOMHighResTimeStamp;
use crate::dom::bindings::codegen::Bindings::VideoPlaybackQualityBinding::VideoPlaybackQualityMethods;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::DomRoot;
use crate::dom::window::Window;

/// <https://w3c.github.io/media-playback-quality/#videoplaybackquality-interface>
#[dom_struct]
pub(crate) struct VideoPlaybackQuality {
    reflector_: Reflector,
    creation_time: f64,
    dropped: u32,
    total: u32,
}

impl VideoPlaybackQuality {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        creation_time: f64,
        total: u64,
        dropped: u64,
    ) -> DomRoot<VideoPlaybackQuality> {
        reflect_dom_object(
            cx,
            Box::new(VideoPlaybackQuality {
                reflector_: Reflector::new(),
                creation_time,
                dropped: dropped.min(u32::MAX as u64) as u32,
                total: total.min(u32::MAX as u64) as u32,
            }),
            window,
        )
    }
}

impl VideoPlaybackQualityMethods<crate::DomTypeHolder> for VideoPlaybackQuality {
    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-creationtime>
    fn CreationTime(&self) -> DOMHighResTimeStamp {
        Finite::wrap(self.creation_time)
    }

    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-droppedvideoframes>
    fn DroppedVideoFrames(&self) -> u32 {
        self.dropped
    }

    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-totalvideoframes>
    fn TotalVideoFrames(&self) -> u32 {
        self.total
    }

    /// <https://w3c.github.io/media-playback-quality/#dom-videoplaybackquality-corruptedvideoframes>
    fn CorruptedVideoFrames(&self) -> u32 {
        0
    }
}
