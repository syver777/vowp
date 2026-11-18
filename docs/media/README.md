# Explainer video and diagrams

Everything here is drawn by code: a hand-made 5×7 pixel font, the StackSpirit "Meadow" palette and a
small set of pixel primitives, rendered at 640×360 and scaled up with nearest-neighbour so every pixel
stays sharp. The README diagrams are single frames of the video, so the two never disagree. The
soundtrack is code too: a small chiptune synth plays a 100 BPM arrangement whose chord changes land on
every scene cut, and 199 sound effects are placed at the exact times the scenes define.

| File | What |
|---|---|
| [`vowp-explainer.mp4`](vowp-explainer.mp4) | The two-minute explainer, 1920×1080, 30 fps, H.264, AAC stereo at −14 LUFS. Sized for X and other social posts. |
| [`vowp-loop.gif`](vowp-loop.gif) | The verify → fix → inbox scene as a looping GIF. |
| [`../images/`](../images/) | One 1280×720 still per scene, used by the README and the wiki. |
| [`src/pixkit.py`](src/pixkit.py) | Palette, font and primitives: panels, chips, DB cylinders, servers, laptops, job nodes. |
| [`src/scenes.py`](src/scenes.py) | The thirteen scenes, each a function of time, and the still time used for each diagram. |
| [`src/render_stills.py`](src/render_stills.py) | Writes `docs/images/vowp-*.png`. |
| [`src/render_video.py`](src/render_video.py) | Pipes frames to `ffmpeg` with dither dissolves between scenes. |
| [`src/sound.py`](src/sound.py) | The cue sheet (read from the scenes), the chiptune synth, the effects and the mixer. |

## Scenes

| # | Scene | Still |
|---|---|---|
| 01 | Why a VPS: a laptop sleeps, the VPS stays on | `vowp-why.png` |
| 02 | The home: supervisor, global DB, project DBs, session logs, snapshot, workers | `vowp-home.png` |
| 03 | Universal sessions: one log across harnesses, machines and surfaces | `vowp-sessions.png` |
| 04 | Orchestrate on the VPS or on your machine (agent node) | `vowp-orchestrate.png` |
| 05 | Databases: global and per project, with a live job queue | `vowp-databases.png` |
| 06 | Job graph: nodes, edges, states and workers | `vowp-graph.png` |
| 07 | Verify → fix → fix → inbox, with the explanation you get | `vowp-loop.png` |
| 08 | Long-horizon work over a week | `vowp-long-horizon.png` |
| 09 | Secrets by name | `vowp-secrets.png` |
| 10 | Merge queue and Work → Production | `vowp-delivery.png` |
| 11 | The whole picture | `vowp-overview.png` |

## Rebuild

Needs Python 3 with Pillow and NumPy, and `ffmpeg` on `PATH`.

```sh
python3 -I docs/media/src/render_stills.py docs/images
python3 -I docs/media/src/render_video.py /tmp/vowp-silent.mp4 --gif loop docs/media/vowp-loop.gif
python3 -I docs/media/src/sound.py /tmp/vowp-sound --only A --mux /tmp/vowp-silent.mp4 docs/media/vowp-explainer.mp4
```

Each step takes under a minute on a laptop. To change a picture, edit its scene in `scenes.py`; the
fifth field of each `SCENES` row is the time used for its still. The sound follows automatically, because
`sound.py` reads its cue times from the same scene timelines.

## The soundtrack

| Part | How it is made |
|---|---|
| Tempo | About 100 BPM. Each scene holds a whole number of beats (96–103 BPM per scene), so every cut lands on a downbeat with a new chord and a soft crash. |
| Harmony | C–Am–F–G; the verify/fix loop switches to Am–F–Dm–E for tension, and the outro resolves on one C chord. |
| Melody | A four-bar lead motif returns in the home, orchestrate, databases, long-horizon, delivery and overview scenes; the overview doubles it an octave down as the final chorus. |
| Lifts | A snare-and-tom fill lands on the inbox moment and another leads into the final chorus. |
| Effects | Pops, blips, key clicks, success and fail tones, the laptop lid, the inbox chime, the vault lock, the crash-and-reboot glitch and the server hum, all synthesized. The music ducks briefly under each one. |

`sound.py` can also build two model-assisted variants (`--mcp DIR`, versions B and C) from music and
effects generated elsewhere; the published video uses version A, which needs no downloads.
