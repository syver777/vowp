# Explainer video and diagrams

Everything here is drawn by code: a hand-made 5×7 pixel font, the StackSpirit "Meadow" palette and a
small set of pixel primitives, rendered at 640×360 and scaled up with nearest-neighbour so every pixel
stays sharp. The README diagrams are single frames of the video, so the two never disagree.

| File | What |
|---|---|
| [`vowp-explainer.mp4`](vowp-explainer.mp4) | The two-minute explainer, 1920×1080, 30 fps, H.264, no sound. Sized for X and other social posts. |
| [`vowp-loop.gif`](vowp-loop.gif) | The verify → fix → inbox scene as a looping GIF. |
| [`../images/`](../images/) | One 1280×720 still per scene, used by the README and the wiki. |
| [`src/pixkit.py`](src/pixkit.py) | Palette, font and primitives: panels, chips, DB cylinders, servers, laptops, job nodes. |
| [`src/scenes.py`](src/scenes.py) | The thirteen scenes, each a function of time, and the still time used for each diagram. |
| [`src/render_stills.py`](src/render_stills.py) | Writes `docs/images/vowp-*.png`. |
| [`src/render_video.py`](src/render_video.py) | Pipes frames to `ffmpeg` with dither dissolves between scenes. |

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
python3 -I docs/media/src/render_video.py docs/media/vowp-explainer.mp4 --gif loop docs/media/vowp-loop.gif
```

The video takes under a minute on a laptop. To change a picture, edit its scene in `scenes.py`; the
fifth field of each `SCENES` row is the time used for its still.
