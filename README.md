# ClipStack

A tiny clipboard-history popup for Windows. Middle-click anywhere, pick from your last 50 clips, and it pastes straight into whatever field you were typing in.

Written in Rust — raw Win32, no UI framework, single file. Built with **agentic engineering**: AI agents I built and direct write most of the code, and every change is human-reviewed before it lands.

## What it does

- Keeps your last **50 clips** (text *and* images)
- **Middle-click** pops a small list right at your cursor — no window stealing focus
- **Left-click** an item → copies it *and* pastes it into the field that had focus
- **Right-click** an item → pin it (with a label) to a persistent section; right-click a pin to remove it
- **Pinned secrets are masked on screen and stored DPAPI-encrypted on disk** — passwords/tokens never sit in plaintext
- **Tray icon**: pause capture, clear history, quit
- **Tiny binary** — optimized release build (LTO, stripped, `panic = abort`)

## Build

Needs [Rust](https://rustup.rs/) and Windows.

```sh
cargo build --release
```

The binary lands at `target/release/clipstack.exe`. Run it; it lives in the tray. Middle-click anywhere to use it.

## How it works (the short version)

Everything runs single-threaded on one Win32 message loop. The low-level mouse hook and the window procedure both run on that one thread, so the global state is only ever touched there — borrows are scoped so the `&mut` is never held across a call that pumps messages (menus, dialogs). Clips are deduped by hash; pinned secrets go through `CryptProtectData` (DPAPI).

## Why I built it

I wanted a clipboard manager that was instant, stayed out of the way, and didn't ship my clipboard off to a cloud service — so I built one, in Rust, with my agentic engineering setup. It's small, fast, and mine.

## License

MIT — see [LICENSE](LICENSE).
