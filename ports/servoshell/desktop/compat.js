// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Stillwater compatibility shims, injected into every page before its own scripts.
// Each one fills a small web API that Servo doesn't implement yet but that many sites rely
// on (found with tools/sitecheck). Keep them tiny and spec-shaped, and remove a shim once
// Servo implements the real API (each is skipped when the API already exists).

(function () {
  "use strict";

  // requestIdleCallback / cancelIdleCallback: missing on ~90% of sites' pages in sitecheck.
  // Run the callback soon, with a deadline that reports ~50 ms of idle time.
  if (typeof window.requestIdleCallback !== "function") {
    window.requestIdleCallback = function (callback, options) {
      const start = Date.now();
      const timeout = options && options.timeout > 0 ? Math.min(options.timeout, 50) : 1;
      return setTimeout(function () {
        callback({
          didTimeout: false,
          timeRemaining: function () { return Math.max(0, 50 - (Date.now() - start)); },
        });
      }, timeout);
    };
    window.cancelIdleCallback = function (id) { clearTimeout(id); };
  }

  // HTMLVideoElement.getVideoPlaybackQuality(): streaming players (e.g. YouTube) poll it while
  // playing to pick a quality and schedule downloads. Servo doesn't count decoded frames yet,
  // so report frames estimated from the playback position, with none dropped.
  if (typeof HTMLVideoElement !== "undefined" &&
      typeof HTMLVideoElement.prototype.getVideoPlaybackQuality !== "function") {
    HTMLVideoElement.prototype.getVideoPlaybackQuality = function () {
      const frames = Math.floor((this.played && this.played.length ? this.currentTime : 0) * 30);
      return {
        creationTime: performance.now(),
        totalVideoFrames: frames,
        droppedVideoFrames: 0,
        corruptedVideoFrames: 0,
        totalFrameDelay: 0,
      };
    };
  }

  // Element.checkVisibility(): whether the element is rendered and not hidden.
  if (typeof Element !== "undefined" && typeof Element.prototype.checkVisibility !== "function") {
    Element.prototype.checkVisibility = function (options) {
      if (!this.isConnected) return false;
      if (this.getClientRects().length === 0) return false;
      const style = getComputedStyle(this);
      if (options && (options.visibilityProperty || options.checkVisibilityCSS) &&
          style.visibility !== "visible") return false;
      if (options && (options.opacityProperty || options.checkOpacity) &&
          parseFloat(style.opacity) === 0) return false;
      return style.display !== "none";
    };
  }
})();
