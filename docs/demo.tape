# Renders docs/demo.gif — run `vhs docs/demo.tape` from the repo root.
# Requires a release build first: `cargo build --release`.

Output docs/demo.gif

Set Shell bash
Set FontSize 19
Set Width 1080
Set Height 760
Set Padding 26
Set Theme "Dracula"
Set WindowBar Colorful
Set TypingSpeed 55ms

# --- invisible setup: built binary on PATH, clean workspace, tidy shell ---
Hide
Type `export STEPCI_REPO="$PWD"`
Enter
Type `export PATH="$STEPCI_REPO/target/release:$PATH" SHELL=/bin/bash PS1="\\[\\e[36m\\]sandbox\\[\\e[0m\\]$ "`
Enter
Type `cd "$(mktemp -d)" && cp "$STEPCI_REPO/examples/demo/deploy.yml" .`
Enter
Type "clear"
Enter
Show

# --- step through a deploy, pausing to inspect ---
Sleep 600ms
Type "stepci run deploy.yml --step --secret DEPLOY_TOKEN=sk-live-9f3a2b7c1e"
Sleep 700ms
Enter
Sleep 1.7s

# paused before Build — continue and watch the diff
Type "c"
Enter
Sleep 2s

# paused before Deploy — drop into a shell with the step's live env
Type "s"
Enter
Sleep 1.4s
Type "echo endpoint is now $ENDPOINT"
Sleep 400ms
Enter
Sleep 1.4s
Type "exit"
Enter
Sleep 1s

# continue — the secret is masked in the step's own output
Type "c"
Enter
Sleep 2.6s
