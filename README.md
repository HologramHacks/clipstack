<p align="center">
  <img src="assets/banner.png" alt="ClipStack, clipboard manager for Windows" width="820">
</p>

# ClipStack

[![CI](https://github.com/HologramHacks/clipstack/actions/workflows/ci.yml/badge.svg)](https://github.com/HologramHacks/clipstack/actions/workflows/ci.yml)

A tiny clipboard-history popup for Windows. Middle-click anywhere, pick from your last 50 clips, and it pastes straight into whatever field you were typing in.

Written in Rust, raw Win32, no UI framework, single file, **zero third-party dependencies** (just the Windows API bindings).

<p align="center">
  <img src="assets/screenshot.png" alt="The ClipStack popup: clipboard history with image thumbnails, dev snippets, and a masked pinned secret" width="460">
</p>

## Download & run (no Rust needed)

Grab the latest `clipstack.exe` from the [**Releases**](https://github.com/HologramHacks/clipstack/releases/latest) page and run it. That's it, no installer, no runtime, no dependencies. It's a single ~0.25 MB file that lives in your system tray.

First launch shows a Windows SmartScreen warning (it's unsigned). Click **More info → Run anyway**.

> **Verify the download (optional):** every release ships a SHA-256 checksum (`clipstack.exe.sha256`) and a build-provenance attestation: proof the binary was built from this source by CI. With the [GitHub CLI](https://cli.github.com/): `gh attestation verify clipstack.exe --repo HologramHacks/clipstack`.

## Using it

ClipStack sits in your system tray. Middle-click is the default opener (you can remap it, see below):

| Action | What happens |
|---|---|
| **Middle-click** anywhere | Opens the history popup at your cursor |
| **Ctrl+Shift+V** anywhere | Opens the popup at your text caret, ready for the keyboard: arrows move, Enter pastes |
| **Mouse wheel** (popup open) | Scrolls through older clips |
| **Rest on an image clip** (hover or arrow-key selection) | Pops a larger preview beside the popup so lookalike screenshots are tellable apart |
| **Left-click** a row | Pastes that clip (or pin) into the field you were in |
| **Click the ✕** on a history clip | Removes it from history (one click) |
| **Hover a text clip → click the green pin** (or right-click the clip) | Pins it, masked on screen. The label comes pre-filled from the clip, shown selected: Enter keeps it, typing replaces it. Up to 99 pins |
| **Right-click** a pin | Renames it (edit the label inline) |
| **Hover a pin → up/down arrows** | Reorders it: plain click moves one step, Shift+click sends it to the top or bottom |
| **Click a pin's ✕, then click again** | Deletes the pin. The first click arms a red confirm button so you can't delete by accident |
| **Right-click the tray icon** | Trigger · Theme (6 of them) · Launch at startup · Remember history · Auto-copy on highlight · Open with Ctrl+Shift+V · Pause · Clear history · About · Quit |

### Fully hands-free

Press **Ctrl+Shift+V** while typing and the popup opens right at your caret with the first clip selected. **Up/Down** move through the list (scrolling when you reach the edge), **Tab** jumps between history and pins, **Enter** pastes the selection back into the field you came from, and **Esc** (or pressing the hotkey again) dismisses without touching anything. Tab to your next form field and repeat: no mouse involved at any point. It's always on alongside your mouse trigger; turn it off from the tray if the combo clashes with another app.

### Changing the trigger

Middle-click is the default, but you can remap it from the tray: **right-click the tray icon → Trigger**. Pick a preset (middle, Mouse 4 / Mouse 5), or choose **Set custom trigger…** and press any modifier+key combo or mouse button. When a mouse button is your trigger, ClipStack takes it over completely, so e.g. a Back/Forward button stops navigating while it's mapped. A keyboard trigger leaves the mouse untouched until you press it (handy if you use the middle/thumb buttons in apps like games or 3D tools). Your choice is remembered across restarts.

> Got a mouse with extra buttons? Windows only exposes five buttons to apps (left, right, middle, and the two thumb buttons), so the rest aren't visible to ClipStack, or any app. Map one to a key combo in your mouse software, then set that combo as a custom trigger.

### Start with Windows

Right-click the tray icon and tick **Launch at startup** to have ClipStack open when you log in. It's **off by default** and just adds a per-user `Run` entry. Untick it to remove. (No admin rights, nothing system-wide.)

### Remember history (opt-in)

By default your history is **memory-only**: it vanishes when ClipStack closes. If you'd rather it be there after a reboot or crash (like an editor restoring your tabs), tick **Remember history** in the tray. While it's on, your **text** clips are saved **DPAPI-encrypted** to `%LOCALAPPDATA%\ClipStack\history.dat` and reloaded on launch; it's flushed continuously, so a crash loses at most the last half-second. It's **off by default**. The trade-off is that copied text (including anything sensitive) then lives on disk, encrypted at rest, until you clear it. Untick it (or **Clear history**) to delete the file. Images stay memory-only either way.

### Auto-copy on highlight (opt-in)

Tick **Auto-copy on highlight** in the tray and ClipStack copies whatever you select with the mouse the moment you finish the drag, so highlighted text lands in your history without pressing Ctrl+C. It's **off by default**. Heads up: it's a heuristic (any click-drag counts, not just text) and it overwrites your clipboard on every selection, so leave it off unless you specifically want that behavior.

### Themes

Six built-in themes: Dark (the default), Grey, Light, Nord, Solarized, and High Contrast. Switch from **right-click the tray icon → Theme**. The popup and the About panel repaint instantly, and your pick sticks across restarts.

## What it does

- Keeps your last **50 clips** (text *and* images)
- **Pin up to 99 items.** Reorder by hovering a pin's arrows (Shift+click sends it to the top or bottom), and the block scrolls once it overflows. Pins get the room they earn: up to 20 pin rows stay visible, with the history section ceding space as your pin list grows (with few pins the classic layout is unchanged)
- The popup opens right at your cursor and **never steals focus** from what you're doing
- **Pinned secrets** are masked on screen, **encrypted in memory**, and DPAPI-encrypted on disk, decrypted only for the instant you paste them
- **Auto-copy on highlight** (opt-in): grab whatever you select with the mouse, no Ctrl+C needed
- **Six color themes** (Dark, Grey, Light, Nord, Solarized, High Contrast) in the tray, dark by default
- **No cloud, no network.** Your clipboard never leaves your machine
- **Tiny:** a single ~0.25 MB exe, no runtime, no dependencies

## Build from source

Only needed if you want to compile or change it yourself. Needs [Rust](https://rustup.rs/) and Windows.

```sh
cargo build --release
```

The binary lands at `target/release/clipstack.exe`. Run it; it lives in the tray. Middle-click anywhere to use it.

## How it works (the short version)

Everything runs single-threaded on one Win32 message loop. The low-level mouse hook and the window procedure both run on that one thread, so the global state is only ever touched there. Borrows are scoped so the `&mut` is never held across a call that pumps messages (menus, dialogs). Clips are deduped by hash. Reading and writing the clipboard goes straight through the Win32 API: `CF_UNICODETEXT` for text, `CF_DIB` for images (with a bounds-checked DIB parser for the untrusted clipboard data). No clipboard library, no third-party crates at all. Pinned secrets are held encrypted in RAM with `CryptProtectMemory` (pages `VirtualLock`'d out of the pagefile) and DPAPI-encrypted (`CryptProtectData`) on disk.

## Why I built it

I wanted a clipboard manager that was instant, stayed out of the way, and didn't ship my clipboard off to a cloud service, so I built one, in Rust. It's small, fast, and mine.

## Security notes

- Clipboard **history lives only in memory by default**, never written to disk, wiped on exit. (The opt-in **Remember history** setting changes this: while it's on, text clips are saved DPAPI-encrypted to `%LOCALAPPDATA%\ClipStack\history.dat` until you clear them. It's off unless you turn it on.)
- **Pinned secrets are encrypted in memory *and* at rest.** In RAM they're held encrypted with `CryptProtectMemory` (per-process key) and their pages are locked out of the pagefile (`VirtualLock`); they're decrypted only for the instant you paste them, then the plaintext is wiped. On disk they're DPAPI-encrypted (per-user) in `%LOCALAPPDATA%\ClipStack\pins.dat`. And when you paste a pin, ClipStack tags the clipboard so Windows keeps it **out of clipboard history (Win+V) and cloud sync**, the way password managers do. Honest limits: a program running as the *same Windows user* can still ask DPAPI to decrypt the file, and pasting a pin necessarily lands it on the Windows clipboard in cleartext for the target app to read (that's the point of pasting). So: encrypted in memory, encrypted at rest, kept out of clipboard history, decrypted only at the moment of use, not hidden from everything. One rule to remember: the pin's **value** is the protected part; its **label is public** (it's drawn on screen every time the list opens). The suggested label is taken from the clip to save typing, so if the clip is sensitive, replace the suggestion (it comes preselected, so just start typing) and keep the secret only in the value.
- ClipStack uses a **global mouse hook** (to catch your open shortcut) and **reads the clipboard on a short timer** (that's how history is built), the same behaviors some malware uses, so an unsigned build may trip a SmartScreen or antivirus warning. It's all local: no keystroke logging, nothing leaves your machine, and the full source is right here to check.
- Your trigger choice is saved in plaintext at `%LOCALAPPDATA%\ClipStack\settings.txt`. It's just a key/button name, not a secret.
- **No network, no telemetry, no analytics.** ClipStack never opens a network connection.

## License

MIT, see [LICENSE](LICENSE).

---

Built by Brian Jones, [hologramhacks.com](https://hologramhacks.com)
