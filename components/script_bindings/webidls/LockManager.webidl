/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/web-locks/#api

[SecureContext]
interface mixin NavigatorLocks {
  readonly attribute LockManager locks;
};
Navigator includes NavigatorLocks;
WorkerNavigator includes NavigatorLocks;

[SecureContext, Exposed=(Window,Worker)]
interface LockManager {
  Promise<any> request(DOMString name,
                       LockGrantedCallback callback);
  Promise<any> request(DOMString name,
                       LockOptions options,
                       LockGrantedCallback callback);

  Promise<LockManagerSnapshot> query();
};

callback LockGrantedCallback = Promise<any> (Lock? lock);

enum LockMode { "shared", "exclusive" };

dictionary LockOptions {
  LockMode mode = "exclusive";
  boolean ifAvailable = false;
  boolean steal = false;
  AbortSignal signal;
};

dictionary LockManagerSnapshot {
  sequence<LockInfo> held;
  sequence<LockInfo> pending;
};

dictionary LockInfo {
  DOMString name;
  LockMode mode;
  DOMString clientId;
};

[SecureContext, Exposed=(Window,Worker)]
interface Lock {
  readonly attribute DOMString name;
  readonly attribute LockMode mode;
};
