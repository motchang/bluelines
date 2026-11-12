---
name: blueline
description: Preview a markdown file (mermaid diagrams, syntax highlighting, GitHub alerts) with the `blueline` command, and check how it renders. Use when the user asks to preview, show or open a markdown file, or when you need to confirm that markdown or a mermaid diagram you wrote renders correctly.
allowed-tools: Bash(blueline:*)
---

# blueline

`blueline` renders a markdown file to HTML on a local server and shows it in a
browser. The page reloads whenever the file is saved, so open a preview once
and keep editing the file.

## Before you start

Check that the command is installed:

	command -v blueline && blueline --version

If it is missing, tell the user and suggest one of:

	brew install motchang/tap/blueline
	cargo install --locked bluelines

Do not install it yourself unless the user asks.

## Pick a surface

Look at the environment first. `HERDR_TAB_ID` and `HERDR_PANE_ID` are set when
you run inside a herdr session.

- **Inside herdr**: use `blueline --term <file.md>`. The preview opens in a
  terminal-browser pane beside you, without taking focus, and it is the only
  surface you can inspect with `blueline -- <command>`.
- **Outside herdr**: use `blueline <file.md>`. This opens a desktop browser
  tab for the user to look at. You cannot read that tab back, so do not
  claim to have checked how it renders.

Flags go before the file: `blueline --term README.md`, never
`blueline README.md --term` (blueline refuses the latter).

## Commands

	blueline <file.md>          # desktop browser tab (focuses it)
	blueline --browser          # reopen a tab on the most recently previewed file
	blueline --term <file.md>   # terminal-browser pane in this herdr tab
	blueline --reset [file.md]  # replace this tab's pane preview with a fresh one
	blueline --pane             # print the herdr pane holding the preview
	blueline --stop             # stop the render server and close this tab's pane preview

`.md`, `.markdown` and `.mdown` count as markdown. The server listens on
loopback port 43128 by default; `BLUELINE_PORT` changes it.

## Inspecting the page (herdr only)

After `blueline --term <file.md>`, drive the preview with `blueline --`
followed by a `terminal-browser action` command. blueline finds this tab's
preview and aims the command at it.

	blueline -- snapshot        # accessibility tree of the rendered page
	blueline -- console         # browser console messages

Use these to verify your own work:

- After writing or editing markdown, take a `snapshot` and check that the
  headings, lists, tables and code blocks came out as intended.
- Mermaid is drawn in the browser, not on the server. After adding or changing
  a mermaid block, check `snapshot` and `console` for a mermaid syntax error
  rather than assuming the diagram rendered.
- The page reloads on save, so after an edit just run `snapshot` again; there
  is no need to re-run `blueline --term`.

Things to avoid:

- `blueline -- open <url>` opens a new tab in the preview's browser; it does
  not navigate the preview. To show another file, run
  `blueline --term <other.md>`.
- `blueline snapshot` (without `--`) is refused. Always put `--` before the
  page command.
- Outside herdr, `blueline --` has no page to reach and says so. Do not retry
  it there.

## When something goes wrong

- "no preview in this tab": run `blueline --term <file.md>` first.
- A file path that does not exist, or is not markdown, is refused. Check the
  path.
- If the port is held by something that is not blueline, blueline refuses to
  use it. Tell the user, and suggest setting `BLUELINE_PORT` to another port.
- The server log is `server.log` in `$XDG_STATE_HOME/blueline`
  (`~/.local/state/blueline` when that is unset).
