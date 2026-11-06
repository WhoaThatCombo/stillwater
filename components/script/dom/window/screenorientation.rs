/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use script_bindings::reflector::reflect_dom_object;

use crate::dom::bindings::codegen::Bindings::ScreenOrientationBinding::{
    OrientationLockType, OrientationType, ScreenOrientationMethods,
};
use crate::dom::bindings::error::Error;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::promise::{Promise, RootedPromise};
use crate::dom::screen::Screen;
use crate::dom::window::Window;

/// <https://w3c.github.io/screen-orientation/#screenorientation-interface>
///
/// Desktop screens don't rotate, so the orientation follows the screen's shape and can't be
/// locked, as in other desktop browsers.
#[dom_struct]
pub(crate) struct ScreenOrientation {
    eventtarget: EventTarget,
    screen: Dom<Screen>,
}

impl ScreenOrientation {
    pub(crate) fn new(cx: &mut JSContext, window: &Window, screen: &Screen) -> DomRoot<Self> {
        reflect_dom_object(
            cx,
            Box::new(ScreenOrientation {
                eventtarget: EventTarget::new_inherited(),
                screen: Dom::from_ref(screen),
            }),
            window,
        )
    }

    fn is_portrait(&self) -> bool {
        let size = self.screen.screen_metrics().screen_size;
        size.height > size.width
    }
}

impl ScreenOrientationMethods<crate::DomTypeHolder> for ScreenOrientation {
    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-lock>
    fn Lock(&self, cx: &mut CurrentRealm, _orientation: OrientationLockType) -> RootedPromise {
        // Locking the orientation isn't supported on desktop screens.
        let promise = Promise::new_in_realm(cx);
        promise.reject_error(
            cx,
            Error::NotSupported(Some(
                "Locking the screen orientation isn't supported on this device".into(),
            )),
        );
        promise
    }

    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-unlock>
    fn Unlock(&self) {}

    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-type>
    fn Type(&self) -> OrientationType {
        if self.is_portrait() {
            OrientationType::Portrait_primary
        } else {
            OrientationType::Landscape_primary
        }
    }

    /// <https://w3c.github.io/screen-orientation/#dom-screenorientation-angle>
    fn Angle(&self) -> u16 {
        0
    }

    // <https://w3c.github.io/screen-orientation/#dom-screenorientation-onchange>
    event_handler!(change, GetOnchange, SetOnchange);
}
