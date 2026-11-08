/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

"use strict";

// Tab audio indicator: a speaker icon on tabs that are playing audio or video
// (tab.playing, from the media progress the engine reports).
function mediaIndicator() {
  const icon = document.createElement("span");
  icon.className = "tab-media icon";
  icon.textContent = "";
  icon.title = "Playing audio";
  return icon;
}
