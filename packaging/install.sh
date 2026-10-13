#!/bin/bash
# Installs trilium-tui for the current user (no root, nothing outside $HOME).
# Does NOT edit Hyprland config; prints the keybinding line to add if you want one.
set -euo pipefail
cd "$(dirname "$0")/.."
cargo build --release
install -Dm755 target/release/trilium-tui "$HOME/.local/bin/trilium-tui"
install -Dm755 target/release/trilium-cli "$HOME/.local/bin/trilium-cli"
mkdir -p "$HOME/.local/share/bash-completion/completions" "$HOME/.local/share/zsh/site-functions" "$HOME/.config/fish/completions"
"$HOME/.local/bin/trilium-cli" completions bash > "$HOME/.local/share/bash-completion/completions/trilium-cli"
"$HOME/.local/bin/trilium-cli" completions zsh > "$HOME/.local/share/zsh/site-functions/_trilium-cli"
"$HOME/.local/bin/trilium-cli" completions fish > "$HOME/.config/fish/completions/trilium-cli.fish"
install -Dm644 packaging/trilium-tui.desktop "$HOME/.local/share/applications/trilium-tui.desktop"
cat <<'MSG'
Installed to ~/.local/bin and ~/.local/share/applications (launcher entry: "Trilium Notes (TUI)").

First sync:   TRILIUM_PASSWORD=... trilium-cli sync http://your-server:8080
Run:          trilium-tui

Optional Hyprland binding (check `omarchy menu keybindings --print` for conflicts first), in ~/.config/hypr/bindings.lua:
  o.bind("SUPER + ALT + N", "Trilium notes", "omarchy launch or focus tui --app-id=trilium-tui trilium-tui")
MSG
