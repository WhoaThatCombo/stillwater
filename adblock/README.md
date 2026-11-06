# Stillwater adblock data

`scriptlets.json` holds uBlock Origin's scriptlets (the code that `##+js(...)` filters inject into pages), converted to the resource format of [adblock-rust](https://github.com/brave/adblock-rust) by [`tools/adblock/build-scriptlets.mjs`](https://github.com/WhoaThatCombo/stillwater/blob/stillwater/tools/adblock/build-scriptlets.mjs). Stillwater downloads it at runtime, alongside uBlock Origin's filter lists.

The scriptlet code is from [uBlock Origin](https://github.com/gorhill/uBlock), © Raymond Hill and contributors, licensed under the [GNU General Public License v3.0](https://github.com/gorhill/uBlock/blob/master/LICENSE.txt). The source is `src/js/resources/` in uBlock Origin at commit `f3e5e5e0efa4b01035b928a084b8fe018cbcbe99`. Each entry's `content` is that source, base64-encoded.

This file is not part of Stillwater's own code (MPL-2.0) and is not built into it.
