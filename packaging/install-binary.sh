#!/bin/bash
# Installs the prebuilt trilium-tui from this folder for the current user (no root, nothing outside $HOME).
# Does NOT edit Hyprland config; prints the keybinding line to add if you want one.
set -euo pipefail
cd "$(dirname "$0")"
install -Dm755 trilium-tui "$HOME/.local/bin/trilium-tui"
install -Dm755 trilium-cli "$HOME/.local/bin/trilium-cli"
install -Dm644 completions/trilium-cli.bash "$HOME/.local/share/bash-completion/completions/trilium-cli"
install -Dm644 completions/_trilium-cli "$HOME/.local/share/zsh/site-functions/_trilium-cli"
install -Dm644 completions/trilium-cli.fish "$HOME/.config/fish/completions/trilium-cli.fish"
launcher="; no launcher entry (its Exec line needs Omarchy)"
if command -v omarchy >/dev/null 2>&1; then
  install -Dm644 trilium-tui.desktop "$HOME/.local/share/applications/trilium-tui.desktop"
  launcher=" and ~/.local/share/applications (launcher entry: \"Trilium Notes (TUI)\")"
fi
echo "Installed to ~/.local/bin$launcher."
cat <<'MSG'
Make sure ~/.local/bin is on your PATH.

First sync:   TRILIUM_PASSWORD=... trilium-cli sync http://your-server:8080
Run:          trilium-tui          (or `trilium-tui --new` to try it without a server)

Optional Hyprland binding (check `omarchy menu keybindings --print` for conflicts first), in ~/.config/hypr/bindings.lua:
  o.bind("SUPER + ALT + N", "Trilium notes", "omarchy launch or focus tui --app-id=trilium-tui trilium-tui")
MSG
