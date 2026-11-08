# winaudiopan

Dynamically pans audio per-application (and per browser tab!) based on where each window is positioned on your screen.

---

## Justification

![chinese bible on the left, mozart on the right](docs/shitpost.png)
*chinese bible on the left, mozart on the right*

This is a shitpost idea I've had in my head for years now (like, at _least_ 2018), but could never justify actually spending the time to implement it.

Well, thanks to LLMs, I don't have to manually wade through all those obtuse Windows and browser audio APIs, and I can just ask my 'bot to hack together some hot garbage in a morning's worth of prompting lol.

So, uhh, yeah.

Enjoy?

> [!NOTE]
>
> Everything above this note was written by a human.
>
> The rest of this README is 100% LLM generated.

---

## Features

* **Real-Time Native Audio Panning:** Tracks top-level desktop windows and adjusts individual application stereo channels via Windows Core Audio (WASAPI `IChannelAudioVolume`). No virtual cables, drivers, or kernel hooks needed.
* **Edge-Aware Panning:** Rather than just tracking midpoints, panning scales based on the window's available travel range on screen:
  * Snapping a window to the left half (50% width) produces **100% hard left** audio.
  * Snapping a window to the right half produces **100% hard right** audio.
  * Skinny windows pan subtly near the center and ramp up aggressively as they reach the screen bezels.
  * Windows stretched across the monitor or maximized default to **centered (0.0)**.
  * Off-screen windows cleanly clamp to hard left/right without audio glitching.
* **Per-Monitor Aware:** Multi-monitor setups evaluate panning against the **specific monitor** the window is currently resting on (`MonitorFromWindow`).
* **Multi-Window Disambiguation:** Resolves multi-window applications (like Chrome, Edge, or Discord) by prioritizing actively dragged windows, media-titled windows, and sticky focus tracking.
* **Independent Per-Tab Browser Panning:** Includes a companion userscript (`winaudiopan.user.js`) that uses Web Audio's `StereoPannerNode` to pan individual browser tabs independently, bypassing Chromium's internal single-stream audio mixer.
* **Seamless Coordination:** The desktop app and userscript coordinate via a local heartbeat:
  * When a userscript tab starts playing audio, `winaudiopan` automatically steps aside and restores Chrome's master balance to `[1.0, 1.0]` to prevent double-panning.
  * When the browser tabs pause or close, `winaudiopan` seamlessly reclaims OS-level control.
  * If `winaudiopan` isn't running, the userscript stays completely dormant and centers audio.
* **Clean Exit:** Pressing `Ctrl+C` immediately restores all audio session balances across your system back to `[1.0, 1.0]`.

---

## Getting Started

### 1. Desktop Application

Requires a stable Rust toolchain on Windows:

```pwsh
cargo run --release
```

To run with verbose logging:

```pwsh
$env:RUST_LOG="debug"
cargo run --release
```

### 2. Browser Userscript (Optional, for Per-Tab Panning)

Because Chromium mixes all tabs into a single audio stream before handing it to Windows, managing tabs in separate windows independently requires in-tab Web Audio assistance:

1. Install a userscript manager like **Violentmonkey** or **Tampermonkey**.
2. Import [`winaudiopan.user.js`](winaudiopan.user.js).
3. Open two YouTube tabs in separate windows on opposite sides of your screen—each will pan completely independently!

---

## Technical Overview

* **Audio Control:** [`IMMDeviceEnumerator`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nn-mmdeviceapi-immdeviceenumerator) activates [`IAudioSessionManager2`](https://learn.microsoft.com/en-us/windows/win32/api/audiopolicy/nn-audiopolicy-iaudiosessionmanager2), which enumerates active sessions and adjusts channel volumes via [`IChannelAudioVolume::SetAllVolumes`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-ichannelaudiovolume-setallvolumes).
* **Window Discovery:** Win32 `EnumWindows` + DWM `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` extracts precise visible frame bounds without drop-shadow padding.
* **Process Resolution:** ToolHelp32 process snapshots walk child processes (e.g. browser audio utility processes) up to their root GUI window.
* **Local Coordination:** A lightweight embedded HTTP server on `127.0.0.1:38400` tracks active in-tab userscripts per `tab_id`.
