/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The Web Locks API: <https://w3c.github.io/web-locks/>.
//!
//! The lock manager state (held locks and request queues) is shared by every global of an origin
//! in this process, so tabs and workers of the same site coordinate through it. Each request
//! remembers the event loop of the global that made it; granting a lock queues a task there,
//! where the request's callback runs. Requests from globals that have gone away are dropped and
//! their locks released whenever the queue for their name is processed.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::realm::CurrentRealm;
use js::rust::HandleValue;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_url::ImmutableOrigin;

use crate::dom::abortsignal::AbortAlgorithm;
use crate::dom::bindings::callback::{ExceptionHandling, RootedCallback, TracedCallback};
use crate::dom::bindings::codegen::Bindings::AbortSignalBinding::AbortSignalMethods;
use crate::dom::bindings::codegen::Bindings::LockManagerBinding::{
    LockGrantedCallback, LockInfo, LockManagerMethods, LockManagerSnapshot, LockMode, LockOptions,
};
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::Error;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::globalscope::GlobalScope;
use crate::dom::lock::Lock;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::promisenativehandler::{Callback, PromiseNativeHandler};
use crate::dom::window::Window;
use crate::realms::enter_auto_realm;
use crate::tasks::task_source::SendableTaskSource;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The lock state of every origin in this process.
static LOCK_STATE: LazyLock<Mutex<HashMap<ImmutableOrigin, OriginLocks>>> =
    LazyLock::new(Default::default);

/// <https://w3c.github.io/web-locks/#lock-manager>
#[derive(Default)]
struct OriginLocks {
    /// <https://w3c.github.io/web-locks/#lock-manager-held-lock-set>
    held: Vec<Record>,
    /// <https://w3c.github.io/web-locks/#lock-request-queue-map>
    queues: HashMap<String, VecDeque<Record>>,
}

/// A lock request, or a lock once it has been granted.
struct Record {
    id: u64,
    name: String,
    exclusive: bool,
    client_id: String,
    owner: Owner,
}

/// Where to deliver news about a request: the `LockManager` that made it, on its own event loop.
#[derive(Clone)]
struct Owner {
    task_source: SendableTaskSource,
    manager: Trusted<LockManager>,
}

impl Owner {
    fn alive(&self) -> bool {
        !self.task_source.canceller.cancelled()
    }
}

enum Notice {
    /// The lock was granted, or (`lock` false) it was unavailable to an `ifAvailable` request.
    Granted { owner: Owner, id: u64, lock: bool },
    /// A held lock was stolen by another request.
    Stolen { owner: Owner, id: u64 },
}

fn with_state<R>(origin: &ImmutableOrigin, f: impl FnOnce(&mut OriginLocks) -> R) -> R {
    let mut state = LOCK_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let locks = state.entry(origin.clone()).or_default();
    let result = f(locks);
    if locks.held.is_empty() && locks.queues.is_empty() {
        state.remove(origin);
    }
    result
}

/// <https://w3c.github.io/web-locks/#process-the-lock-request-queue>
fn process_queue(locks: &mut OriginLocks, name: &str, notices: &mut Vec<Notice>) {
    // Locks of globals that have been destroyed can never be released by them.
    locks.held.retain(|held| held.owner.alive());
    let Some(queue) = locks.queues.get_mut(name) else {
        return;
    };
    queue.retain(|request| request.owner.alive());
    while let Some(request) = queue.front() {
        // <https://w3c.github.io/web-locks/#lock-request-grantable>
        let grantable = !locks
            .held
            .iter()
            .any(|held| held.name == name && (request.exclusive || held.exclusive));
        if !grantable {
            break;
        }
        let request = queue.pop_front().expect("queue has a front request");
        notices.push(Notice::Granted {
            owner: request.owner.clone(),
            id: request.id,
            lock: true,
        });
        locks.held.push(request);
    }
    if queue.is_empty() {
        locks.queues.remove(name);
    }
}

fn dispatch(notices: Vec<Notice>) {
    for notice in notices {
        match notice {
            Notice::Granted { owner, id, lock } => {
                let manager = owner.manager.clone();
                owner.task_source.queue(task!(lock_granted: move |cx| {
                    manager.root().run_callback(cx, id, lock);
                }));
            },
            Notice::Stolen { owner, id } => {
                let manager = owner.manager.clone();
                owner.task_source.queue(task!(lock_stolen: move |cx| {
                    manager.root().lock_stolen(cx, id);
                }));
            },
        }
    }
}

/// A request made through a `LockManager` whose promise isn't settled yet.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct PendingRequest {
    id: u64,
    name: String,
    exclusive: bool,
    callback: TracedCallback<LockGrantedCallback>,
    /// <https://w3c.github.io/web-locks/#lock-concept-released-promise>
    promise: TracedPromise,
}

/// <https://w3c.github.io/web-locks/#lockmanager>
#[dom_struct]
pub(crate) struct LockManager {
    reflector_: Reflector,
    #[no_trace]
    origin: ImmutableOrigin,
    client_id: String,
    pending: DomRefCell<Vec<PendingRequest>>,
}

impl LockManager {
    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<LockManager> {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        reflect_dom_object(
            cx,
            Box::new(LockManager {
                reflector_: Reflector::new(),
                origin: global.origin().immutable().clone(),
                client_id: format!("{:?}-{id}", global.pipeline_id()),
                pending: Default::default(),
            }),
            global,
        )
    }

    fn owner(&self) -> Owner {
        Owner {
            task_source: self
                .global()
                .task_manager()
                .dom_manipulation_task_source()
                .to_sendable(),
            manager: Trusted::new(self),
        }
    }

    /// <https://w3c.github.io/web-locks/#dom-lockmanager-request>
    fn request(
        &self,
        cx: &mut CurrentRealm,
        name: DOMString,
        options: &LockOptions,
        callback: RootedCallback<LockGrantedCallback>,
    ) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        let global = self.global();

        // Step 3. If environment's relevant global object's associated Document is not fully
        // active, then return a promise rejected with a "InvalidStateError" DOMException.
        if let Some(window) = global.downcast::<Window>() &&
            !window.Document().is_fully_active()
        {
            promise.reject_error(cx, Error::InvalidState(None));
            return promise;
        }

        // Step 5. If origin is an opaque origin, then return a promise rejected with a
        // "SecurityError" DOMException.
        if !self.origin.is_tuple() {
            promise.reject_error(cx, Error::Security(None));
            return promise;
        }

        // Step 6. If name starts with U+002D HYPHEN-MINUS (-), then return a promise rejected
        // with a "NotSupportedError" DOMException.
        let name = name.to_string();
        let exclusive = options.mode == LockMode::Exclusive;
        let not_supported = name.starts_with('-') ||
            // Step 7. If both options["steal"] and options["ifAvailable"] are true ...
            (options.steal && options.ifAvailable) ||
            // Step 8. If options["steal"] is true and options["mode"] is not "exclusive" ...
            (options.steal && !exclusive) ||
            // Step 9. If options["signal"] exists, and either of options["steal"] or
            // options["ifAvailable"] is true ...
            (options.signal.is_some() && (options.steal || options.ifAvailable));
        if not_supported {
            promise.reject_error(cx, Error::NotSupported(None));
            return promise;
        }

        // Step 10. If options["signal"] exists and is aborted, then return a promise rejected
        // with options["signal"]'s abort reason.
        if let Some(signal) = &options.signal &&
            signal.aborted()
        {
            rooted!(&in(cx) let mut reason = js::jsval::UndefinedValue());
            signal.Reason(reason.handle_mut());
            promise.reject(cx, reason.handle());
            return promise;
        }

        // Step 12. Request a lock with promise, the current agent, environment's id, manager,
        // callback, name, options["mode"], options["ifAvailable"], options["steal"], and
        // options["signal"].
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        self.pending.borrow_mut().push(PendingRequest {
            id,
            name: name.clone(),
            exclusive,
            callback: callback.to_traced(),
            promise: promise.to_traced(),
        });

        let record = Record {
            id,
            name: name.clone(),
            exclusive,
            client_id: self.client_id.clone(),
            owner: self.owner(),
        };
        let mut notices = vec![];
        with_state(&self.origin, |locks| {
            // <https://w3c.github.io/web-locks/#request-a-lock>
            if options.steal {
                // Step 7.1. For each lock of held, if lock's name is name: remove it and reject
                // its released promise with an "AbortError" DOMException.
                locks.held.retain(|held| {
                    if held.name != name {
                        return true;
                    }
                    notices.push(Notice::Stolen {
                        owner: held.owner.clone(),
                        id: held.id,
                    });
                    false
                });
                // Step 7.2. Prepend request in queue.
                locks
                    .queues
                    .entry(name.clone())
                    .or_default()
                    .push_front(record);
            } else if options.ifAvailable &&
                (locks
                    .queues
                    .get(&name)
                    .is_some_and(|queue| !queue.is_empty()) ||
                    locks
                        .held
                        .iter()
                        .any(|held| held.name == name && (exclusive || held.exclusive)))
            {
                // Step 8.1. If ifAvailable is true and request is not grantable, then enqueue
                // the following steps on callback's relevant settings object's responsible event
                // loop: invoke callback with null, and resolve promise with the result.
                notices.push(Notice::Granted {
                    owner: record.owner,
                    id,
                    lock: false,
                });
                return;
            } else {
                // Step 9. Otherwise, enqueue request in queue.
                locks
                    .queues
                    .entry(name.clone())
                    .or_default()
                    .push_back(record);
            }
            // Step 10. Process the lock request queue queue.
            process_queue(locks, &name, &mut notices);
        });
        dispatch(notices);

        // Step 11. If signal is present, then add the following abort steps to signal.
        if let Some(signal) = &options.signal {
            signal.add(&AbortAlgorithm::LockRequest(Dom::from_ref(self), id));
        }

        promise
    }

    fn take_pending(&self, id: u64) -> Option<PendingRequest> {
        let mut pending = self.pending.borrow_mut();
        let index = pending.iter().position(|request| request.id == id)?;
        Some(pending.remove(index))
    }

    /// Invoke a request's callback once its lock is granted (or, with `lock` false, found to
    /// be unavailable), and wait for the callback's promise to settle.
    fn run_callback(&self, cx: &mut JSContext, id: u64, lock: bool) {
        let Some((name, exclusive, callback)) = self
            .pending
            .borrow()
            .iter()
            .find(|request| request.id == id)
            .map(|request| {
                (
                    request.name.clone(),
                    request.exclusive,
                    request.callback.root(cx),
                )
            })
        else {
            // The request went away in the meantime; don't keep the lock.
            self.release(id);
            return;
        };

        let global = self.global();
        let mut realm = enter_auto_realm(cx, &*global);
        let cx = &mut realm.current_realm();

        // <https://w3c.github.io/web-locks/#snapshot-the-lock-state> Step 9.3 (or 8.1 for an
        // unavailable lock). Let r be the result of invoking callback with a new Lock object.
        let lock = lock.then(|| Lock::new(cx, &global, name, exclusive));
        let result = callback
            .Call__(cx, lock.as_deref(), ExceptionHandling::Rethrow)
            .unwrap_or_else(|error| {
                let rejected = Promise::new_in_realm(cx);
                rejected.reject_error(cx, error);
                rejected
            });

        // Step 9.4. Upon fulfillment or rejection of r, release the lock and resolve the
        // released promise with r.
        let handler = PromiseNativeHandler::new(
            cx,
            &global,
            Some(Box::new(LockCallbackSettled {
                manager: Dom::from_ref(self),
                id,
                fulfilled: true,
            })),
            Some(Box::new(LockCallbackSettled {
                manager: Dom::from_ref(self),
                id,
                fulfilled: false,
            })),
        );
        result.append_native_handler(cx, &handler);
        // A rejection is reported through the released promise instead.
        result.set_promise_is_handled(cx);
    }

    fn callback_settled(&self, cx: &mut JSContext, id: u64, fulfilled: bool, value: HandleValue) {
        if let Some(request) = self.take_pending(id) {
            let promise = request.promise.root(cx);
            if promise.is_pending() {
                if fulfilled {
                    promise.resolve(cx, value);
                } else {
                    promise.reject(cx, value);
                }
            }
        }
        self.release(id);
    }

    /// <https://w3c.github.io/web-locks/#release-the-lock>
    fn release(&self, id: u64) {
        let mut notices = vec![];
        with_state(&self.origin, |locks| {
            let Some(index) = locks.held.iter().position(|held| held.id == id) else {
                return;
            };
            let lock = locks.held.remove(index);
            process_queue(locks, &lock.name, &mut notices);
        });
        dispatch(notices);
    }

    /// A lock held through this manager was stolen: reject its released promise.
    fn lock_stolen(&self, cx: &mut JSContext, id: u64) {
        let pending = self.pending.borrow();
        if let Some(request) = pending.iter().find(|request| request.id == id) {
            let promise = request.promise.root(cx);
            drop(pending);
            if promise.is_pending() {
                promise.reject_error(cx, Error::Abort(None));
            }
        }
    }

    /// <https://w3c.github.io/web-locks/#request-a-lock> Step 11: the request's signal was
    /// aborted. A request that is still queued is withdrawn and its promise rejected.
    pub(crate) fn abort_request(&self, cx: &mut JSContext, id: u64, reason: HandleValue) {
        let mut notices = vec![];
        let withdrawn = with_state(&self.origin, |locks| {
            let Some((name, index)) = locks.queues.iter().find_map(|(name, queue)| {
                queue
                    .iter()
                    .position(|request| request.id == id)
                    .map(|index| (name.clone(), index))
            }) else {
                return false;
            };
            if let Some(queue) = locks.queues.get_mut(&name) {
                queue.remove(index);
            }
            process_queue(locks, &name, &mut notices);
            true
        });
        dispatch(notices);
        if withdrawn && let Some(request) = self.take_pending(id) {
            request.promise.root(cx).reject(cx, reason);
        }
    }
}

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct LockCallbackSettled {
    manager: Dom<LockManager>,
    id: u64,
    fulfilled: bool,
}

impl Callback for LockCallbackSettled {
    fn callback(&self, cx: &mut CurrentRealm, value: HandleValue) {
        self.manager
            .callback_settled(cx, self.id, self.fulfilled, value);
    }
}

impl LockManagerMethods<crate::DomTypeHolder> for LockManager {
    /// <https://w3c.github.io/web-locks/#dom-lockmanager-request>
    fn Request(
        &self,
        cx: &mut CurrentRealm,
        name: DOMString,
        callback: RootedCallback<LockGrantedCallback>,
    ) -> RootedPromise {
        self.request(cx, name, &LockOptions::empty(), callback)
    }

    /// <https://w3c.github.io/web-locks/#dom-lockmanager-request-name-options-callback>
    fn Request_(
        &self,
        cx: &mut CurrentRealm,
        name: DOMString,
        options: &LockOptions,
        callback: RootedCallback<LockGrantedCallback>,
    ) -> RootedPromise {
        self.request(cx, name, options, callback)
    }

    /// <https://w3c.github.io/web-locks/#dom-lockmanager-query>
    fn Query(&self, cx: &mut CurrentRealm) -> RootedPromise {
        let promise = Promise::new_in_realm(cx);
        if !self.origin.is_tuple() {
            promise.reject_error(cx, Error::Security(None));
            return promise;
        }
        let info = |record: &Record| LockInfo {
            name: Some(DOMString::from(record.name.clone())),
            mode: Some(if record.exclusive {
                LockMode::Exclusive
            } else {
                LockMode::Shared
            }),
            clientId: Some(DOMString::from(record.client_id.clone())),
        };
        let snapshot = with_state(&self.origin, |locks| LockManagerSnapshot {
            held: Some(
                locks
                    .held
                    .iter()
                    .filter(|held| held.owner.alive())
                    .map(info)
                    .collect(),
            ),
            pending: Some(locks.queues.values().flatten().map(info).collect()),
        });
        promise.resolve_native(cx, &snapshot);
        promise
    }
}
