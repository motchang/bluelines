# blueline

A markdown previewer built for terminal and agent workflows. With `--term`,
blueline puts the preview in a
[terminal-browser](https://github.com/zenbu-labs/terminal-browser) pane right
inside your herdr tab, and an agent can drive that page with
`blueline -- <command>`: snapshot it, read its console, click through it.
Everything renders offline - mermaid.js and highlight.js ship inside the
binary, so nothing goes out to the network.

Without `--term`, a file opens in an ordinary desktop browser tab. Embedded
mermaid renders too, and saving the file reloads the page.

Markdown, syntax highlighting, GitHub alerts and the table of contents are
rendered by the server; only mermaid, which needs a DOM, is drawn in the
browser. `.md`, `.markdown` and `.mdown` all count as markdown.

## Install

blueline is a single binary. The command is `blueline`; the crate is called
`bluelines`. Install it with any of:

	cargo install --locked bluelines
	brew install motchang/tap/blueline

The Homebrew formula installs a prebuilt binary, so it needs no Rust
toolchain. `cargo install` needs [Rust](https://rustup.rs) 1.85 or newer.

Each [GitHub Release](https://github.com/motchang/bluelines/releases) also has
prebuilt binaries for macOS (Apple silicon and Intel) and Linux (x86_64 and
aarch64, statically linked): unpack the archive for your platform and put
`blueline` on PATH.

To build the latest `main` instead:

	cargo install --locked --git https://github.com/motchang/bluelines

`cargo install` puts the binary in `~/.cargo/bin`. Make sure that directory is
on PATH for every shell, not only interactive ones, so that an agent's Bash
finds it too.

At run time blueline needs `open` (macOS) or `xdg-open` (Linux) to open a
browser tab. `--term`, `--pane`, `--reset` and `blueline --` also need herdr and
terminal-browser.

## Usage

The port is 43128 by default; `BLUELINE_PORT` overrides it.

	blueline README.md            # a desktop browser tab
	blueline --browser README.md  # the same, spelled out
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
`A.md`. Only a bare `/`, such as a hand-typed URL, follows a file it was not told
about - and it follows the one the pane
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

What a tab cannot do is be driven. `blueline -- <command>` needs a
terminal-browser preview, so inside a herdr session it reports having no preview
in this tab even while a browser tab is open on one, and points at `--term`.

`?file=` only serves paths that were registered by previewing them. A markdown
file sitting on disk that blueline was never pointed at comes back 404, and so
does anything else - the query is not a way to ask this server to read a file.

The loopback port is open to every user and process on the machine, so the
server trusts none of them for being local. When it starts it writes a random
token to `blueline-<uid>/server-<port>.token` under `$XDG_RUNTIME_DIR`, or
`$TMPDIR` or `/tmp` without one, in a directory and file only you can read.
Registering a file, stopping the server and asking for a page ticket all need
that token. Pages, their images and their reload streams need a view key
instead, which a browser gets by visiting a preview URL with a single-use ticket
in it: the server swaps the ticket for the key in the address, and the page's
own links, images and reload stream carry it from there. It is not a cookie,
because a browser sends cookies for 127.0.0.1 to every port on it, and pages are
served with `Referrer-Policy: no-referrer` so that following a link does not
pass the key on either. A browser tab blueline did not open, or one left over
from an earlier server, is refused until `blueline <file.md>` opens it again. A
bare `/` is a page like any other and needs the key too.

Before blueline sends anything to the server on the port, the server has to
answer a fresh challenge with an HMAC keyed by the token. A process that took
the port first cannot, so it is never sent a path, the token or anything else.

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
markdown path, such as `blueline snapshot`, is refused
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

The render server is the same binary, started in the background as
`blueline serve`, and logs to `${TMPDIR}/blueline-server.log`. A `blueline` from
another install or version holding the port is asked to stop and replaced, so
the page always renders with the code of the command that asked for it. One that
cannot answer the challenge is asked to stop without being sent the token, and
if it does not, blueline says so and leaves it running: no process is ever
signalled. Something on the port that does not say it is blueline is left alone
too, and blueline refuses to preview through it; stop it, or set
`BLUELINE_PORT` to another port. The CLI and the server find the token through
the same environment variables, so a server started from a shell with another
`XDG_RUNTIME_DIR` or `TMPDIR` cannot be told apart from an impostor.

## Development

The crate lives at the root of this repository - package `bluelines`, command
`blueline`.

	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	cargo test

Coverage is measured with [cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov)
(`cargo install cargo-llvm-cov` and `rustup component add llvm-tools-preview`):

	cargo llvm-cov --open

CI writes `coverage/lcov.info` and an HTML report (uploaded as the `coverage`
artifact), and [octocov](https://github.com/k1LoW/octocov) (configured in
`.octocov.yml`) comments the coverage on each pull request. The report for
`main` is kept as an Actions artifact, so pull requests show the diff against
`main`.

CI also measures the cognitive and cyclomatic complexity of every function in
`src/` with [cccc](https://github.com/moznion/cccc)
(`.github/workflows/complexity.yml`). It fails when a function goes over a
cognitive complexity of 15 or a cyclomatic complexity of 10 (set in
`.cccc.toml`); split the function if that happens. The 20 most complex
functions are listed in the job summary, and [octocov](https://github.com/k1LoW/octocov)
comments the totals on pull requests, with the difference from `main`. To check
locally, install cccc (`cargo install cccc-cli`) and run
`cccc --config .cccc.toml --table src`.

## Releasing

Releases are built by `.github/workflows/release.yml`. To cut one:

1. Bump `version` in `Cargo.toml`, run `cargo check` so that `Cargo.lock`
   follows, and merge that change to `main`.
2. Tag the merge commit with the same version and push the tag:

		git tag v0.1.0
		git push origin v0.1.0

The workflow refuses a tag that does not match `Cargo.toml`. It builds
`blueline` for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`
(static, so they run on any Linux), `x86_64-apple-darwin` and
`aarch64-apple-darwin`, and publishes a GitHub Release with release notes
generated from the merged pull requests. Each target gets
`blueline-vX.Y.Z-<target>.tar.gz` (the binary, this README and the licenses)
and a matching `.sha256`; `SHA256SUMS` lists them all. A tag with a suffix,
such as `v0.2.0-rc.1`, is published as a pre-release.

Pull requests that change the workflow run the builds too and keep the
archives as workflow artifacts, without publishing anything.

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
