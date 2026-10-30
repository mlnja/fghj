# Recording the demo GIF

Nothing here is recorded live in one take. The terminal clips are typed by
[VHS](https://github.com/charmbracelet/vhs) from a script, so there are no
typos and they can be regenerated at will. The browser is a separate short
clip. `stitch.sh` then joins the three with a short crossfade. If one take
goes wrong, redo that clip only. There is no switching between windows during
a recording: the cut between clips does that.

## The story (~22–25 seconds)

| # | What you see | Recorded with | Length |
|---|---|---|---|
| 1 | `files-web/.fghj.yaml` without comments: `include:` of two other repos, and `flows:`. "That's the whole config." | VHS, `01-config.tape` | ~5 s |
| 2 | The UI at `https://fghj.internal/`: a graph of 4 repos, pick the `files-web/full` flow, nodes turn green (sped up), then a new tab at `https://files.test` with the padlock | screen recording, `record-browser.sh` | ~10 s |
| 3 | `curl -s $URL` on the host, and the same URL through `fghj exec api.files-api -- node -e "fetch(...)"` inside a container. Same answer. | VHS, `03-same-url.tape` | ~7 s |

These are the three ideas from the README: a graph no repo owns, flows, and
one hostname inside and outside Docker. The example is the tutorial's
(`fghj-examples/files-*`), so anyone watching can reproduce it.

## 0. Install once

```bash
brew install vhs gifski bat     # vhs brings ttyd and ffmpeg
```

Give your terminal (Terminal, iTerm, Ghostty) screen recording permission:
System Settings → Privacy & Security → Screen Recording.

## 1. Prepare the workspace (beforehand, off camera)

Name the workspace folder `files`, so the domains match the tutorial's
(`web.files-web.files.fghj.internal`).

```bash
mkdir -p ~/files && cd ~/files
fghj wire https://github.com/fghj-examples/files-web.git
fghj pull                       # clones the other 3 repos and pulls images
fghj up files-web/full          # the first run builds everything; this can take minutes
open https://files.test         # check it loads with the padlock
```

Run clip 3's commands by hand and check both answers match:

```bash
URL=https://web.files-web.files.fghj.internal/api/health
curl -s $URL
fghj exec api.files-api -- node -e "fetch('$URL').then(r=>r.text()).then(console.log)"
```

Before recording clip 2, switch the workspace to almost nothing, so the
nodes start on camera, but from cache and quickly:

```bash
fghj up files-platform/cache    # leaves only Redis running
```

If a command's output differs from what the tapes expect, fix the tape
before recording.

## 2. Record the clips

Run everything below from this folder, `docs/record`. Clips and build output
are git-ignored.

**Clip 1** (about 10 seconds, hands off):

```bash
vhs 01-config.tape              # -> 01-config.mp4
```

**Clip 2**, the browser. The only clip done by hand:

```bash
./record-browser.sh
```

The script opens a clean Chrome profile (no bookmarks or extensions) at
exactly 1200×720 in a fixed spot. Press Enter, and only that rectangle is
recorded. Then:

1. Pause 2 seconds on the graph.
2. Pick the `files-web/full` flow and wait for everything to go green. The
   wait doesn't matter: `stitch.sh` speeds it up.
3. Open `https://files.test` (Cmd+L, paste, Enter), wait for it to load,
   pause 2 seconds.
4. Ctrl-C in the terminal (or it stops by itself after 60 s).

Before another take, run `fghj up files-platform/cache` again.

If `screencapture -R` misbehaves: Cmd+Shift+5 → Record Selected Portion,
select the Chrome window, and save the file as `02-browser.mov` in this
folder.

**Clip 3** (after clip 2, with the `full` flow up):

```bash
vhs 03-same-url.tape            # -> 03-same-url.mp4
```

## 3. Stitch

Open `02-browser.mov` in QuickTime and note four times in seconds: where to
start, when you picked the flow, when the last node went green, and when the
page finished loading. Pass them in:

```bash
B_START=1 B_FAST_FROM=4 B_FAST_TO=31 B_FAST_X=8 B_END=38 ./stitch.sh
```

This writes `demo.mp4` here and `demo.gif` to `docs/public/demo.gif`. It
shrinks the GIF (1200 → 960 px, fewer frames) until it fits in 5 MB. If you
don't like the result, change the numbers and run it again; it takes seconds.

## 4. Where each goes

- **README:** commit `docs/public/demo.gif` and add this under the tagline:
  `![fghj: wire two repos, start one flow, same URL inside and outside Docker](docs/public/demo.gif)`
- **Reddit:** upload `demo.mp4`, not the GIF. Reddit re-encodes video, and
  it comes out sharper and lighter than a GIF.

## When something goes wrong

- **A typo or wrong output in the terminal:** edit the `.tape` and run `vhs`
  again. Nothing in it is typed by hand.
- **A fumble in the browser:** record clip 2 again. Extra footage at either
  end is cut with `B_START`/`B_END`, and the long wait is sped up.
- **Terminal text too small:** raise `Set FontSize` (now 20) in both tapes.
  If lines stop fitting, raise `Set Height` there too; `stitch.sh` fits any
  size.
- **Another terminal theme:** `vhs themes` lists them; change `Set Theme`.
