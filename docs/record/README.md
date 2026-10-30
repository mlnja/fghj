# Recording the demo GIF

The demo starts from a machine with fghj installed and nothing else, and
ends with four repos running on real HTTPS names.

Nothing here is recorded live in one take. The terminal clips are typed by
[VHS](https://github.com/charmbracelet/vhs) from a script, so there are no
typos and they can be regenerated at will. The browser is one take of its
own. `stitch.sh` then cuts and joins everything. If a take goes wrong, redo
that clip only. There is no switching between windows during a recording:
the cuts do that.

VHS writes `.mp4`, not GIFs, on purpose: those are intermediate clips.
`stitch.sh` makes the one GIF at the end, from all of them.

## The story (~30 seconds)

| # | What you see | Recorded with |
|---|---|---|
| 1 | `mkdir ~/files && cd ~/files`, `fghj wire https://github.com/fghj-examples/files-web.git`, then the cloned repo's `.fghj.yaml`: `include:` of two repos that aren't on disk, and `flows:` | VHS, `wire.tape` |
| 2 | The UI at `https://fghj.internal/`: select the `files-web/full` flow, the repos it needs show as placeholders; **Download all**, and the graph fills in (sped up) | `record-browser.sh`, first half |
| 3 | `ls`: four repos, where there was one | VHS, `ls.tape` |
| 4 | Back in the UI: **Switch to flow**, every node goes green (sped up), then `https://files.test` with the padlock | `record-browser.sh`, second half |
| 5 | `curl -s $URL` on the host, and the same URL through `fghj exec` inside a container. Same answer. | VHS, `same-url.tape` |

## 0. Install once

```bash
brew install vhs gifski         # vhs brings ttyd and ffmpeg
```

Give your terminal (Terminal, iTerm, Ghostty) screen recording permission:
System Settings → Privacy & Security → Screen Recording.

## 1. Warm the caches (once, off camera)

The first run builds three images and pulls five, which takes minutes. Do it
once so that on camera it takes seconds:

```bash
mkdir ~/files && cd ~/files
fghj wire https://github.com/fghj-examples/files-web.git
fghj pull                       # clones the other 3 repos and pulls images
fghj up files-web/full          # builds everything
open https://files.test         # check it loads with the padlock
```

Run clip 5's commands by hand and check both answers match:

```bash
URL=https://web.files-web.files.fghj.internal/api/health
curl -s $URL
fghj exec api.files-api -- node -e "fetch('$URL').then(r=>r.text()).then(console.log)"
```

Then go back to an empty machine:

```bash
./reset.sh                      # from docs/record in the fghj checkout
```

`reset.sh` unwires `~/files` and deletes it. Docker keeps its images and
build cache, so the next download and build are quick. It refuses to delete
`~/files` if anything other than the example repos is in it.

## 2. Record

Run everything from this folder, `docs/record`. Clips and build output are
git-ignored.

**Clip 1** (hands off). It runs `reset.sh` first, so every take starts from
nothing:

```bash
vhs wire.tape                 # -> wire.mp4
```

**Clips 2 and 4**, one browser take:

```bash
./record-browser.sh             # -> browser.mov
```

The script opens a clean Chrome profile (no bookmarks or extensions) at
exactly 1200×720 in a fixed spot. Press Enter, and only that rectangle is
recorded. Keep every pause short, about 1.5 seconds; the waits get sped up,
the pauses don't.

1. Select the `files-web/full` flow. Pause.
2. Press **Download all** on the Repos tab, and wait for the graph to fill in.
   Pause.
3. Press **Switch to flow**, and wait for every node to go green. Pause.
4. Open `https://files.test` (Cmd+L, paste, Enter), wait for it to load.
   Pause.
5. Press `q` in the terminal. ffmpeg finishes the file and saves `browser.mov`.

Nothing needs doing between steps 2 and 3: `stitch.sh` puts the `ls` clip
there.

If the recording is cut wrong (not a Retina screen), run it as
`SCALE=1 ./record-browser.sh`. If it won't record at all: Cmd+Shift+5 →
Record Selected Portion, select the Chrome window, and save the file as
`browser.mov` in this folder.

**Clip 3** (any time after Download all):

```bash
vhs ls.tape                   # -> ls.mp4
```

**Clip 5** (with the flow up):

```bash
vhs same-url.tape             # -> same-url.mp4
```

For another browser take, run `vhs wire.tape` again first: it resets and
re-wires, so the UI starts from one repo again.

## 3. Stitch

Open `browser.mov` in QuickTime and note seven times, in seconds:

| Variable | Moment |
|---|---|
| `B_START` | start of what you want to keep |
| `B_DL_FROM` | pressed Download all |
| `B_DL_TO` | the graph has filled in |
| `B_CUT` | where to cut to the `ls` clip |
| `B_UP_FROM` | pressed Switch to flow |
| `B_UP_TO` | the last node went green |
| `B_END` | `files.test` has loaded |

```bash
B_START=1 B_DL_FROM=4 B_DL_TO=19 B_CUT=21 B_UP_FROM=23 B_UP_TO=70 B_END=76 ./stitch.sh
```

This writes `demo.mp4` here and `demo.gif` to `docs/public/demo.gif`. It
shrinks the GIF (1200 → 960 px, fewer frames) until it fits in 5 MB. If the
result runs long, raise `B_FAST_X` (default 10) or trim the pauses; it takes
seconds to re-run.

## 4. Where each goes

- **README:** commit `docs/public/demo.gif` and add this under the tagline:
  `![fghj: wire one repo, download the rest, start one flow, same URL inside and outside Docker](docs/public/demo.gif)`
- **Reddit:** upload `demo.mp4`, not the GIF. Reddit re-encodes video, and
  it comes out sharper and lighter than a GIF.

## When something goes wrong

- **A typo or wrong output in the terminal:** edit the `.tape` and run `vhs`
  again. Nothing in it is typed by hand.
- **`fghj wire` takes longer than a minute:** clip 1 waits up to 60 s for the
  prompt. Raise `Wait@60s` in `wire.tape`.
- **A fumble in the browser:** run `vhs wire.tape` and take the browser
  again. Extra footage at either end is cut with `B_START`/`B_END`.
- **Terminal text too small:** raise `Set FontSize` (now 20) in the tapes.
  If lines stop fitting, raise `Set Height` there too; `stitch.sh` fits any
  size.
- **Another terminal theme:** `vhs themes` lists them; change `Set Theme`.
