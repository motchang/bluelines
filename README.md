# blueline

Previews markdown in an ordinary desktop browser tab, or with `--term` in a
terminal-browser pane. Embedded mermaid renders too, and saving the file reloads
the page. Nothing goes out to the network - mermaid.js and highlight.js are both
served locally.

Markdown, syntax highlighting, GitHub alerts and the table of contents are
rendered on the Bun side; only mermaid, which needs a DOM, is drawn in the
browser. `.md`, `.markdown` and `.mdown` all count as markdown.

## Install

blueline needs [bun](https://bun.sh), curl and jq, and `open` (macOS) or
`xdg-open` (Linux) to open a browser tab. lsof is how it finds the render server to
stop or replace. `--term` and `blueline --` also need herdr and
[terminal-browser](https://github.com/zenbu-labs/terminal-browser).

With mise, bun is one line:

	mise use -g bun@latest

`mise activate` only puts bun on PATH in an interactive shell. Link the shim
into a directory that is on PATH for every shell - ~/bin here - so that blueline
finds it when it starts the render server from a non-interactive shell, an
agent's Bash for one.

	ln -sfn ~/.local/share/mise/shims/bun ~/bin/bun

Then clone this repository, install the render server's dependencies and put the
command on PATH:

	git clone https://github.com/motchang/bluelines ~/src/github.com/motchang/bluelines
	cd ~/src/github.com/motchang/bluelines/blueline-server && bun install
	ln -sf ~/src/github.com/motchang/bluelines/bin/blueline ~/bin/blueline

Coming from mdpreview in motchang/dotfiles: run `mdpreview --stop` first, then
`rm -f ~/bin/mdpreview`. blueline does not recognise a render server mdpreview
started, so one left on the same port would keep serving with the old code.
`MDPREVIEW_PORT` is no longer read - set `BLUELINE_PORT` if you had changed it.

## Usage

The port is 43128 by default; `BLUELINE_PORT` overrides it.

	blueline README.md            # a desktop browser tab
	blueline --browser README.md  # the same; kept from when a pane was the default
	blueline --browser            # a tab on the file most recently previewed
	blueline --term README.md     # a terminal-browser pane in this herdr tab
	blueline -- snapshot          # a `terminal-browser action` page command, aimed at this tab's pane preview
	blueline -- console
	blueline --pane               # the herdr pane the preview is in
	blueline --reset              # close this tab's pane preview and open a new one on the same file
	blueline --stop               # stop the render server and close this tab's pane preview

A file argument opens a desktop browser tab, from inside a herdr session and from
a plain terminal alike. The render server has no herdr in it - it is plain HTTP
on the loopback port - so the page is the same one wherever it is shown. Opening
the tab needs `open` (macOS) or `xdg-open` (Linux), and blueline refuses
outright with neither; an opener that is installed but cannot reach a display
(`xdg-open` with no `DISPLAY`, say) fails later, and reports whatever it has to
say for itself. The tab takes focus, unlike a pane: a pane is already on screen
in the tab being looked at, whereas asking to see a file is asking to be shown
it.

`--term` puts the preview in a terminal-browser pane instead, which is the one
surface `blueline --` can drive. A preview already in this tab - a
terminal-browser tab showing a page from this render server - is navigated to the
new file. A tab without one gets it as a new tab in whatever terminal-browser the
tab already has, or, with none, a fresh pane split off to the right without
taking focus. A browser in this tab that is not showing a preview keeps its own
pages; the preview goes in beside them. A preview in another tab is not a
candidate for reuse - nobody in this tab can see it. Outside herdr (no
HERDR_TAB_ID / HERDR_PANE_ID) there is no tab to scope to and no pane to split
from, so `--term` refuses rather than opening a browser tab that nothing could
drive.

`--browser` with no file reopens a tab on the most recent file any surface was
pointed at, and says so if there is none. `--reset` with no argument comes back
to the file the pane modes last opened instead, which is the one a pane is
actually showing - and only when this tab has a pane preview to replace. With
none it refuses instead of splitting a pane nobody asked for: a browser tab is
not something it can reset, and the file a pane last opened may be another
tab's. `--reset <file.md>` names the file, and always builds the pane.

Flags go before the file. Anything after it is refused rather than dropped, so
`blueline README.md --term` says so instead of quietly opening a browser tab.
Either way, handing the file back over broadcasts a reload, so a page already
open on a long document scrolls back to the top.

Previews are independent of each other. Each page carries its own file in its
URL - `/?file=<path>` - so a pane in this tab, a pane in another and any number
of browser tabs each hold a different file, and saving one reloads only the pages
showing that one. `blueline B.md` while a pane shows `A.md` leaves that pane on
`A.md`. Only a bare `/`, which is what a hand-typed URL and a page from before
this gets, follows a file it was not told about - and it follows the one the pane
modes last opened, not the most recent of all: `blueline B.md` leaves a bare-`/`
page on `A.md` too, which is the same distinction `--reset` and `--browser` draw
between them above.

Previewing the same file in a browser tab twice does not stack tabs: the server knows whether a live
page is already showing the file and the second run reloads that instead of
opening another. It knows by counting connected reload streams, which leaves two
windows where a spare tab still appears. One is a tab Chrome has discarded or
frozen in the background, which reads as gone. The other is two runs close
enough together that the first tab has not finished loading and connecting its
stream - the easier of the two to hit, and no heuristic is going to tell that
apart from a tab that never opened. A spare tab is the better failure than
refusing to show a preview that is not on screen. Panes are not counted either;
a pane on the file is no reason to withhold a tab from someone who asked for one.

What a tab still cannot do is be driven. `blueline -- <command>` needs a
terminal-browser preview, so inside a herdr session it reports having no preview
in this tab even while a browser tab is open on one, and points at `--term`.

`?file=` only serves paths that were registered by previewing them. A markdown
file sitting on disk that blueline was never pointed at comes back 404, and so
does anything else - the query is not a way to ask this server to read a file.

Left to itself, `terminal-browser action` drives the active tab of this tab's
browser, which is not the preview once the preview shares a browser with other
pages. Naming the preview is blueline's job: `blueline -- <command>` finds this
tab's preview in `terminal-browser ls` and runs `terminal-browser action
--browser <key> --tab <id> -- <command>`, then `action done` to clear the
"agent is acting" marker it leaves on the tab. Nothing on the calling side has
to read the list. Not every agent-browser command is passed on to the page:
terminal-browser turns `open <url>` and `tab new` into a new tab of the
preview's browser - `open` does not navigate the preview - and `tab close` into
closing a tab, and refuses `launch`, `install`, `connect`, `disconnect` and the
`--cdp`, `--session` and similar flags outright. Those commands reach the page over CDP, which gets to a
terminal-browser tab and to nothing else, so outside herdr they say so rather
than answering about a browser in some other tab. A bare word that is not a
markdown path - `blueline snapshot`, the old way of writing this - is refused
with a pointer to `--`. `--pane`, `--reset` and `--term` need a herdr tab to
look in as well.

The pane is split by herdr with `--no-focus` and terminal-browser is started in
it, instead of `terminal-browser open --split`: that command splits with
`--focus`, and herdr's focus pulls the whole UI over to the tab, so an agent
previewing in a background workspace would drag the user there. The pane is
labelled `blueline` when it is split, and the label is what `--reset` and
`--stop` go by: they close a labelled pane whose terminal-browser holds only
previews, and otherwise close just the preview tabs - a browser that holds other
pages too keeps them, and a pane blueline did not split stays up whatever runs
in it, since it may be someone's shell running `terminal-browser open`; a
browser there that held only previews exits with its last tab and leaves the
shell behind. Neither
uses `terminal-browser shutdown`, which closes every terminal-browser in the
session.

The render server lives in `blueline-server/` (bun) and logs to
`${TMPDIR}/blueline-server.log`. It sits at a different layer from the command
(`bin/blueline`), so it carries a `-server` suffix instead of sharing the name.


## Rust port

blueline is being rewritten in Rust (see #7). The crate lives at the root of
this repository - package `bluelines`, command `blueline` - next to the sh/bun
implementation above, which stays the one to install until the cut-over. It
does not preview anything yet.

	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

CI also measures the cognitive and cyclomatic complexity of every function in
`src/` with [cccc](https://github.com/moznion/cccc)
(`.github/workflows/complexity.yml`). It fails when a function goes over a
cognitive complexity of 15 or a cyclomatic complexity of 10 (set in
`.cccc.toml`); split the function if that happens. The 20 most complex
functions are listed in the job summary, and [octocov](https://github.com/k1LoW/octocov)
comments the totals on pull requests, with the difference from `main`. To check
locally, install cccc (`cargo install cccc-cli`) and run
`cccc --config .cccc.toml --table src`.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <https://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <https://opensource.org/licenses/MIT>)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
