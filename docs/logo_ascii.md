# TUI Logo from PNG (ASCII)

You can render the real Void-Box image in terminal TUI as ASCII art.

## 1) Convert image to ASCII

```bash
scripts/logo_to_ascii.sh /path/to/void-box-logo.png assets/logo/void-box.txt 80
```

The script tries:

- `chafa` (preferred)
- `jp2a` (fallback)

## 2) Run TUI with logo

Default path (auto-loaded):

- `assets/logo/void-box.txt`

Run:

```bash
cargo run --bin voidbox -- tui
```

Or explicit path:

```bash
cargo run --bin voidbox -- tui --logo-ascii assets/logo/void-box.txt
```

Or env var:

```bash
VOIDBOX_LOGO_ASCII_PATH=assets/logo/void-box.txt cargo run --bin voidbox -- tui
```

## Notes

- `--logo-ascii` is supported by the TUI command (`voidbox tui`).
- If no file exists, TUI falls back to `⬢ VOID-BOX`.
