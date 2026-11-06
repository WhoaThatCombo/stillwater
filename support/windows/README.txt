Stillwater for Windows (x64)
============================

A calm, lightweight browser built on Servo, with vertical tabs and colour-tinted spaces.
This is an early, experimental build: some websites will render incorrectly.

Getting started
---------------
1. Keep this whole folder together (Stillwater.exe needs the DLLs and the
   "resources" folder next to it).
2. Double-click Stillwater.exe, or run it with a URL:
       Stillwater.exe https://servo.org

Windows may show a SmartScreen warning because the build isn't code-signed.
Choose "More info" and then "Run anyway" if you trust this download.

Shortcuts
---------
Ctrl+L / Alt+D     Focus the URL bar
Ctrl+T / Ctrl+W    New tab / close tab (stays in the current space)
Ctrl+1..9          Go to a tab in the current space
Ctrl+PgUp / PgDn   Previous / next tab
Ctrl+Alt+1..9      Switch space
Ctrl+Alt+C         Toggle compact mode

Your spaces are saved in %APPDATA%\Servo\workspaces.json.

Source code and license
-----------------------
Source: https://github.com/WhoaThatCombo/stillwater
Stillwater is licensed under the Mozilla Public License 2.0 (see LICENSE.txt).
It is built on Servo (https://servo.org), and its sidebar styling is adapted from
Zen Browser's MPL-2.0 styles. Stillwater isn't affiliated with or endorsed by either project.

This package also contains third-party libraries shipped the same way as Servo's own
Windows builds: GStreamer and FFmpeg (LGPL), ANGLE (BSD), and the Microsoft Visual C++
runtime. Their source is available from their respective projects.
