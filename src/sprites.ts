// Session characters: a small pixel crab in place of a status dot. Its pose says
// the session's state, what it holds says the tool in use, and it takes one step
// per tool call. A step only swaps an attribute: nothing moves between two calls,
// so the window never repaints for it (see the note on `.sprite` in styles.css).

import type { SessionState } from "./types";

export type Prop = "pen" | "wrench" | "loupe" | "globe" | "team";

export interface SpriteView {
  state: SessionState;
  prop: Prop | null;
  /** Tool calls seen so far: the feet swap at each one. */
  steps: number;
  /** A waiting session whose question is only guessed: its sign fades. */
  tentative: boolean;
}

/** x, y, width and height on the 15 × 11 grid. */
type Rect = readonly [x: number, y: number, w: number, h: number];
/** A CSS class and what it fills. */
type Layer = readonly [cls: string, rects: readonly Rect[]];

const SVG_NS = "http://www.w3.org/2000/svg";

const BODY: readonly Rect[] = [
  [2, 3, 8, 6],
  [0, 5, 2, 2],
  [2, 9, 1, 1],
  [4, 9, 1, 1],
  [7, 9, 1, 1],
  [9, 9, 1, 1],
];
const CLAW: readonly Rect[] = [[10, 5, 2, 2]];
const RAISED_CLAW: readonly Rect[] = [[10, 3, 2, 2]];
const EYES: readonly Rect[] = [
  [4, 5, 1, 2],
  [7, 5, 1, 2],
];
const SHUT_EYES: readonly Rect[] = [
  [3, 6, 2, 1],
  [7, 6, 2, 1],
];
/** Frowning, with a mouth: crossed eyes turn into a mesh at this size. */
const UPSET_FACE: readonly Rect[] = [
  [3, 4, 1, 1],
  [4, 5, 1, 1],
  [8, 4, 1, 1],
  [7, 5, 1, 1],
  [5, 7, 2, 1],
];
/** The feet of each pair, down in turn while the crab walks. */
const FEET_A: readonly Rect[] = [
  [2, 10, 1, 1],
  [7, 10, 1, 1],
];
const FEET_B: readonly Rect[] = [
  [4, 10, 1, 1],
  [9, 10, 1, 1],
];
const STANDING: readonly Layer[] = [
  ["sp-body sp-step-a", FEET_A],
  ["sp-body sp-step-b", FEET_B],
];

const PROPS: Record<Prop, readonly Layer[]> = {
  pen: [
    ["sp-tip", [[14, 1, 1, 2], [13, 2, 1, 2], [12, 3, 1, 2]]],
    ["sp-prop", [[11, 4, 1, 1]]],
  ],
  wrench: [["sp-prop", [[12, 1, 1, 2], [14, 1, 1, 2], [12, 3, 3, 1], [13, 4, 1, 3]]]],
  loupe: [
    ["sp-prop", [[12, 0, 2, 1], [11, 1, 1, 2], [14, 1, 1, 2], [12, 3, 2, 1], [12, 4, 1, 1]]],
    ["sp-glass", [[12, 1, 2, 2]]],
  ],
  // A ring with its equator and a meridian.
  globe: [["sp-tip", [[11, 0, 3, 1], [10, 1, 1, 3], [14, 1, 1, 3], [11, 4, 3, 1], [12, 1, 1, 3], [11, 2, 3, 1]]]],
  // Someone else on the job: a smaller crab at its feet.
  team: [["sp-body", [[12, 7, 3, 2], [12, 9, 1, 1], [14, 9, 1, 1]]]],
};

/** Tools by what the crab holds for them, with their short names (`mcp__…` removed). */
const TOOL_PROPS: ReadonlyArray<readonly [Prop, RegExp]> = [
  ["pen", /^(Edit|MultiEdit|Write|NotebookEdit)$/],
  ["wrench", /^(Bash|PowerShell|run_in_terminal)$/],
  ["loupe", /^(Read|Grep|Glob|LS)$/],
  ["globe", /^(WebFetch|WebSearch|navigate|read_page|get_page_text|computer)$/],
  ["team", /^(Agent|Task|Workflow)$/],
];

/** What the crab holds for a tool; null for the tools that are not work of their own. */
export function propOf(tool: string): Prop | null {
  return TOOL_PROPS.find(([, names]) => names.test(tool))?.[0] ?? null;
}

function layersOf(state: SessionState, prop: Prop | null): readonly Layer[] {
  switch (state) {
    case "working":
      return [["sp-body", [...BODY, ...CLAW]], ["sp-ink", EYES], ...STANDING, ...(prop ? PROPS[prop] : [])];
    case "waiting":
      return [
        ["sp-body", [...BODY, ...RAISED_CLAW]],
        ["sp-ink", EYES],
        ...STANDING,
        ["sp-sign", [[12, 0, 3, 1], [14, 1, 1, 1], [13, 2, 2, 1], [13, 4, 1, 1]]],
      ];
    case "error":
      return [
        ["sp-body", [...BODY, ...CLAW]],
        ["sp-ink", UPSET_FACE],
        ...STANDING,
        ["sp-alert", [[12, 0, 2, 4], [12, 5, 2, 1]]],
      ];
    case "unseen":
      return [
        ["sp-body", [...BODY, ...CLAW]],
        ["sp-ink", EYES],
        ...STANDING,
        ["sp-prop", [[12, 0, 1, 10]]],
        ["sp-flag", [[13, 0, 2, 3]]],
      ];
    case "idle":
      return [["sp-body", [...BODY, ...CLAW]], ["sp-ink", SHUT_EYES]];
  }
}

function spriteSvg(state: SessionState, prop: Prop | null): SVGSVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("viewBox", "0 0 15 11");
  svg.setAttribute("shape-rendering", "crispEdges");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const [cls, rects] of layersOf(state, prop)) {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("class", cls);
    path.setAttribute("d", rects.map(([x, y, w, h]) => `M${x} ${y}h${w}v${h}h${-w}z`).join(""));
    svg.append(path);
  }
  return svg;
}

/** Brings a `.sprite` element up to date: the drawing is rebuilt only for a new pose. */
export function paintSprite(el: HTMLElement, view: SpriteView): void {
  const prop = view.state === "working" ? view.prop : null;
  const pose = `${view.state}:${prop ?? ""}`;
  if (el.dataset.pose !== pose) {
    el.dataset.pose = pose;
    el.replaceChildren(spriteSvg(view.state, prop));
  }
  el.classList.toggle("sprite--idle", view.state === "idle");
  el.classList.toggle("is-tentative", view.tentative);
  // Only a working crab steps; the others stand on all their feet.
  if (view.state === "working") el.dataset.step = String(view.steps % 2);
  else delete el.dataset.step;
}
