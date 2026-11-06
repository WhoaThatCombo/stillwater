/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::codegen::Bindings::LockManagerBinding::{LockMethods, LockMode};
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;

/// <https://w3c.github.io/web-locks/#lock>
#[dom_struct]
pub(crate) struct Lock {
    reflector_: Reflector,
    name: String,
    exclusive: bool,
}

impl Lock {
    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        name: String,
        exclusive: bool,
    ) -> DomRoot<Lock> {
        reflect_dom_object(
            cx,
            Box::new(Lock {
                reflector_: Reflector::new(),
                name,
                exclusive,
            }),
            global,
        )
    }
}

impl LockMethods<crate::DomTypeHolder> for Lock {
    /// <https://w3c.github.io/web-locks/#dom-lock-name>
    fn Name(&self) -> DOMString {
        DOMString::from(self.name.clone())
    }

    /// <https://w3c.github.io/web-locks/#dom-lock-mode>
    fn Mode(&self) -> LockMode {
        if self.exclusive {
            LockMode::Exclusive
        } else {
            LockMode::Shared
        }
    }
}
