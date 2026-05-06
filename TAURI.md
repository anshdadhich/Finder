# FastSeek Tauri GUI

This project now includes a Tauri Spotlight-style GUI implementation.

## Run (development)

```powershell
cargo run --manifest-path .\src-tauri\Cargo.toml
```

Note: this app is configured to require Administrator privileges for full NTFS indexing via MFT/USN APIs. Run the terminal as Administrator when launching in dev mode.

## Build

```powershell
cargo build --release --manifest-path .\src-tauri\Cargo.toml
```

## UI location

- Frontend: `tauri-ui/`
- Tauri backend + commands: `src-tauri/src/main.rs`

## Implemented commands

- `search(query, limit, caseSensitive)`
- `open_result(path, folderOnly)`
- `get_settings()`
- `save_settings({ excludedPaths, caseSensitive })`

## Keyboard behavior

- Global shortcut: `Alt+Space` toggles the window.
- In search box:
  - `Up/Down`: selection
  - `Enter`: open selected result
  - `Ctrl+Enter`: open parent folder
  - `Esc`: hide window

## Tray behavior

- Tray icon left click: toggle window.
- Tray menu:
  - `Show / Hide`
  - `Quit`
