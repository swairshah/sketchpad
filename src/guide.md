
# Sketchpad: draw together with the human

`sketchpad` is a Paper-style drawing app on this machine (stylus + touchscreen).
You can pop it open full-screen with a question card and an optional figure; the
human draws with their pencil and taps **Send** (or **Skip**). The command blocks
until then and prints one JSON line.

## Ask for a drawing

```bash
sketchpad ask "Sketch the screen layout you have in mind" --from Claude
sketchpad ask "Circle the part of the chart that looks wrong" --image /abs/or/rel/chart.png --from Claude
```

Options:
- `--image <png|jpg>` — a figure mounted in the middle of the page to draw on
  (render diagrams/plots to PNG first, e.g. with matplotlib or graphviz).
- `--from <name>` — shown on the card as "<NAME> ASKS". Use your own name.
- `--keep` — keep the current ink instead of starting a fresh page
  (use for follow-up rounds on the same drawing).
- `--timeout <secs>` — stop waiting after this long.

Run it with a generous tool timeout (drawing takes the human a while), or
`--timeout 900`.

## Result

```json
{"status":"sent","image":"/home/…/.local/share/sketchpad/drawings/drawing-…png","width":1920,"height":1200,"strokes":14,"prompt":"…"}
```

- `sent` (exit 0): **read the PNG at `image`** to see what they drew. It is the
  whole page: warm paper background, your figure (if any) at its on-screen
  position, and their ink. Colors carry meaning if you asked for it.
- `skipped` (exit 3): they declined — continue without it, don't re-ask at once.
- `superseded` / `closed` (exit 3): another request replaced it or the app closed.
- `error` (exit 1): message in `error`.

## Draw, write and erase on the page yourself

```bash
sketchpad draw '{"draw":[{"type":"circle","cx":640,"cy":420,"r":90},
                         {"type":"arrow","from":[900,250],"to":[740,380]},
                         {"type":"text","x":910,"y":210,"text":"this one","size":30}],
                 "erase":[{"x":100,"y":100,"w":200,"h":80}]}'
echo '{...}' | sketchpad draw -
```

Coordinates are **pixels of the page PNG** you got from `ask`/`snapshot`
(origin top-left; the reply includes `width`/`height`). Erases run before
drawing, so you can erase a region and redraw it. Your ink is drawn in
Claude terracotta by default and animates in like a hand drawing it; the
human can undo your whole turn with one undo.

- draw types: `line`/`arrow` (`from`,`to`), `circle` (`cx`,`cy`,`r`),
  `ellipse` (`cx`,`cy`,`rx`,`ry`), `rect` (`x`,`y`,`w`,`h`),
  `polyline` (`points`, `closed`), `curve` (`points`, smooth spline — use for
  organic shapes), `text` (`x`,`y` top-left, `text`, `size`), `highlight`
  (`x`,`y`,`w`,`h`: a yellow highlighter band over exactly that box — use it to
  highlight a word or line, one per line).
- optional on each: `color` (`#rrggbb` or black|red|blue|green|yellow|purple|brown),
  `tool` (`pen` default, `pencil`, `marker` = thin translucent line).
- After drawing, `sketchpad snapshot` and look; erase and redraw anything misplaced.
- `erase`: rectangles; rubs out ALL ink inside (the human's too) — only when
  correcting or asked.
- `notes`: `[{"text","x","y"}]` short margin notes signed with the Claude mascot.
- No emojis — they are stripped.

A good loop: `snapshot` → look at the PNG → `draw` your contribution →
`ask --keep "your turn: ..."` to hand the pen back.

## Peek without interrupting

```bash
sketchpad snapshot   # {"status":"ok","image":".../snapshot-….png","strokes":n,"notes":[…]}
```

## Collaborating in rounds

1. Ask with a clear, specific prompt (one task per card; it shows ~2 lines).
2. Read the returned PNG, respond in chat.
3. To iterate, either `--keep` to continue on the same ink, or generate an
   annotated figure (e.g. their PNG with your marks drawn on it) and ask again
   with `--image`.

Keep prompts short and concrete: "Draw the data flow between these 3 boxes",
"Label each axis", "Mark where the button should go".
