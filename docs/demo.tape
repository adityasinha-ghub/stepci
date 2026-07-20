# Renders docs/demo.gif — run `vhs docs/demo.tape` from the repo root.
# Requires a release build first: `cargo build --release`.

Output docs/demo.gif

Set Shell bash
Set FontSize 20
Set Width 1000
Set Height 560
Set Padding 24
Set Theme "Dracula"
Set WindowBar Colorful
Set TypingSpeed 55ms

# --- invisible setup: put the built binary on PATH, run in a clean workspace ---
Hide
Type `export STEPCI_REPO="$PWD"`
Enter
Type `export PATH="$STEPCI_REPO/target/release:$PATH"`
Enter
Type `cd "$(mktemp -d)" && cp "$STEPCI_REPO/examples/demo/demo.yml" .`
Enter
Type "clear"
Enter
Show

# --- the demo ---
Sleep 600ms
Type "stepci run demo.yml"
Sleep 900ms
Enter
Sleep 4s
