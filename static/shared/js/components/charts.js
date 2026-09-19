/**
 * Charts, hand-drawn in SVG.
 *
 * Rules that hold across all of them:
 *
 * * Colour is assigned by the job it does — the categorical `--series-*`
 *   slots for parts-of-a-whole (fixed order, so a category keeps its colour),
 *   one hue for magnitude (bar lists, heatmap), never a rainbow.
 * * Every value the eye has to compare is also written down: legends and bar
 *   lists carry the number, so nothing depends on discriminating two fills.
 * * Everything with marks has a hover layer, because a chart on screen that
 *   can't be interrogated is just a picture.
 */

import { h, s } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";

/** The categorical slots, in assignment order. Never cycled: a ninth
 *  category folds into the eighth rather than repeating slot 1's hue, which
 *  would make two different things look like the same thing. */
export const SERIES = Array.from({ length: 8 }, (_, index) => `var(--series-${index + 1})`);

// ---------------------------------------------------------------- tooltip

let tip = null;

function showTip(event, content) {
  if (!tip) {
    tip = h("div", { class: "tooltip" });
    document.body.append(tip);
  }
  tip.replaceChildren(content);
  tip.style.visibility = "hidden";
  tip.style.left = "0px";
  tip.style.top = "0px";

  const box = tip.getBoundingClientRect();
  const x = Math.min(event.clientX + 14, innerWidth - box.width - 8);
  const y = Math.max(8, event.clientY - box.height - 12);
  tip.style.left = `${x}px`;
  tip.style.top = `${y}px`;
  tip.style.visibility = "visible";
}

export function hideTip() {
  tip?.remove();
  tip = null;
}

function tipContent(title, rows) {
  return h(
    "div",
    { class: "stack" },
    h("div", { class: "title", text: title }),
    ...rows.map(([label, value]) =>
      h(
        "div",
        { class: "row" },
        h("span", { class: "muted", text: label }),
        h("span", { class: "num strong", text: value }),
      ),
    ),
  );
}

/**
 * Renders into the container at its real pixel width and re-renders when that
 * changes: text and strokes stay at their intended size instead of being
 * scaled by a viewBox.
 */
function responsive(render, height) {
  const host = h("div", { style: { minHeight: `${height}px` } });

  const paint = () => {
    const width = host.clientWidth || host.parentElement?.clientWidth || 600;
    if (width < 40) return;
    host.replaceChildren(render(width));
  };

  const observer = new ResizeObserver(() => paint());
  /* Observe once attached; ResizeObserver fires an initial callback itself. */
  queueMicrotask(() => observer.observe(host));
  return host;
}

// ------------------------------------------------------------ stacked bar

/**
 * Parts of a whole. Segments keep their slot colour regardless of size, and
 * the legend spells out every value — the categorical palette's light-mode
 * contrast relief depends on those labels being there.
 */
export function stackedBar(segments, { formatValue = fmt.bytes } = {}) {
  const total = segments.reduce((sum, segment) => sum + Number(segment.value || 0), 0);

  const bar = h(
    "div",
    { class: "stackbar" },
    ...segments.map((segment, index) => {
      const share = total ? Number(segment.value || 0) / total : 0;
      if (share <= 0) return null;
      const node = h("span", {
        style: { width: `${Math.max(share * 100, 0.6)}%`, background: SERIES[Math.min(index, SERIES.length - 1)] },
        title: `${segment.label}: ${formatValue(segment.value)}`,
      });
      node.addEventListener("mousemove", (event) =>
        showTip(event, tipContent(segment.label, [[fmt.percent(share, 1), formatValue(segment.value)]])),
      );
      node.addEventListener("mouseleave", hideTip);
      return node;
    }),
  );

  const legend = h(
    "div",
    { class: "legend", style: { marginTop: "12px", display: "grid", gap: "6px" } },
    ...segments.map((segment, index) =>
      h(
        "div",
        { class: "item" },
        h("span", { class: "swatch", style: { background: SERIES[Math.min(index, SERIES.length - 1)] } }),
        h("span", { class: "truncate", text: segment.label }),
        h("span", { class: "value", text: formatValue(segment.value) }),
      ),
    ),
  );

  return h("div", {}, bar, legend, total ? null : h("div", { class: "muted small", style: { marginTop: "8px" }, text: "Nothing stored yet." }));
}

// ------------------------------------------------------------- area chart

/**
 * One measure over time. Single series on purpose: the daily breakdown lives
 * in the tooltip, where it can be read exactly instead of guessed from eight
 * overlapping lines.
 */
export function areaChart(points, { formatValue = fmt.number, breakdown } = {}) {
  const height = 168;

  return responsive((width) => {
    const pad = { top: 12, right: 12, bottom: 22, left: 44 };
    const plotWidth = Math.max(10, width - pad.left - pad.right);
    const plotHeight = height - pad.top - pad.bottom;

    const values = points.map((point) => Number(point.value) || 0);
    const max = Math.max(1, ...values);
    const stepX = points.length > 1 ? plotWidth / (points.length - 1) : 0;
    const x = (index) => pad.left + index * stepX;
    const y = (value) => pad.top + plotHeight - (value / max) * plotHeight;

    const line = points.map((point, index) => `${index ? "L" : "M"}${x(index)},${y(values[index])}`).join(" ");
    const area = `${line} L${x(points.length - 1)},${pad.top + plotHeight} L${pad.left},${pad.top + plotHeight} Z`;

    const svg = s("svg", { class: "chart", width, height, viewBox: `0 0 ${width} ${height}` });

    /* Recessive grid: two lines, labelled, nothing more. */
    for (const fraction of [0, 0.5, 1]) {
      const value = max * fraction;
      const yPosition = y(value);
      svg.append(
        s("line", { class: "grid-line", x1: pad.left, x2: width - pad.right, y1: yPosition, y2: yPosition }),
        s("text", { x: pad.left - 8, y: yPosition + 3.5, "text-anchor": "end", text: fmt.compact(value) }),
      );
    }

    svg.append(
      s("path", { class: "area", d: area, fill: SERIES[0] }),
      s("path", { class: "line", d: line, stroke: SERIES[0] }),
    );

    /* A one- or two-day window draws no visible stroke — a lone point needs a
       mark of its own, or a brand-new server looks like a broken chart. */
    if (points.length <= 2) {
      for (const [index, value] of values.entries()) {
        svg.append(s("circle", { cx: x(index), cy: y(value), r: 4, fill: SERIES[0] }));
      }
    }

    /* X labels: first, middle, last — enough to place the window. */
    for (const index of new Set([0, Math.floor(points.length / 2), points.length - 1])) {
      const point = points[index];
      if (!point) continue;
      svg.append(
        s("text", {
          x: Math.min(Math.max(x(index), pad.left + 12), width - pad.right - 12),
          y: height - 6,
          "text-anchor": "middle",
          text: fmt.date(point.day),
        }),
      );
    }

    const marker = s("circle", { class: "dot", r: 4, fill: SERIES[0], opacity: 0 });
    const crosshair = s("line", { class: "crosshair", y1: pad.top, y2: pad.top + plotHeight, opacity: 0 });
    svg.append(crosshair, marker);

    const hit = s("rect", {
      class: "hit",
      x: pad.left,
      y: pad.top,
      width: plotWidth,
      height: plotHeight,
    });

    hit.addEventListener("mousemove", (event) => {
      const bounds = svg.getBoundingClientRect();
      const index = stepX
        ? Math.round((event.clientX - bounds.left - pad.left) / stepX)
        : 0;
      const point = points[Math.max(0, Math.min(points.length - 1, index))];
      if (!point) return;

      const position = points.indexOf(point);
      marker.setAttribute("cx", x(position));
      marker.setAttribute("cy", y(Number(point.value) || 0));
      marker.setAttribute("opacity", 1);
      crosshair.setAttribute("x1", x(position));
      crosshair.setAttribute("x2", x(position));
      crosshair.setAttribute("opacity", 1);

      showTip(
        event,
        tipContent(fmt.date(point.day), [
          ["total", formatValue(point.value)],
          ...(breakdown?.(point) ?? []),
        ]),
      );
    });

    hit.addEventListener("mouseleave", () => {
      marker.setAttribute("opacity", 0);
      crosshair.setAttribute("opacity", 0);
      hideTip();
    });

    svg.append(hit);
    return svg;
  }, height);
}

// --------------------------------------------------------------- bar list

/** Top-N by one measure: magnitude, so one hue and every value written out. */
export function barList(items, { formatValue = fmt.bytes } = {}) {
  const max = Math.max(1, ...items.map((item) => Number(item.value) || 0));

  return h(
    "div",
    { class: "barlist" },
    ...items.map((item) =>
      h(
        "div",
        { class: "item" },
        item.label,
        h("span", { class: "num text-2", text: formatValue(item.value) }),
        h(
          "div",
          { class: "track" },
          h("span", { style: { width: `${((Number(item.value) || 0) / max) * 100}%` } }),
        ),
      ),
    ),
  );
}

// ---------------------------------------------------------------- heatmap

const HEATMAP_WEEKS = 52;
const LEVELS = 4;

const dayKey = (date) =>
  `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, "0")}-${String(date.getDate()).padStart(2, "0")}`;

/** GitHub-style calendar. Sequential ramp: further from the surface is more. */
export function heatmap(entries, { aggregate = false } = {}) {
  const root = h("div", { class: "heatmap" });
  const byDay = new Map(entries.map((entry) => [entry.day, entry]));
  const max = Math.max(0, ...entries.map((entry) => entry.totalSeconds));
  const level = (seconds) =>
    seconds <= 0 || max <= 0 ? 0 : Math.min(LEVELS, Math.max(1, Math.ceil((seconds / max) * LEVELS)));

  const today = new Date();
  today.setHours(0, 0, 0, 0);
  const start = new Date(today);
  start.setDate(start.getDate() - (HEATMAP_WEEKS * 7 - 1));
  start.setDate(start.getDate() - start.getDay());

  const weeks = [];
  const cursor = new Date(start);
  while (cursor <= today) {
    const week = [];
    for (let index = 0; index < 7; index += 1) {
      week.push(new Date(cursor));
      cursor.setDate(cursor.getDate() + 1);
    }
    weeks.push(week);
  }

  const monthFormat = new Intl.DateTimeFormat(undefined, { month: "short" });
  const weekdayFormat = new Intl.DateTimeFormat(undefined, { weekday: "short" });
  const dateFormat = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });

  const weekdays = h(
    "div",
    { class: "heatmap-weekdays" },
    ...weeks[0].map((date, row) =>
      h("div", { class: "heatmap-weekday", text: row % 2 === 1 ? weekdayFormat.format(date) : "" }),
    ),
  );

  const months = h(
    "div",
    { class: "heatmap-months" },
    ...weeks.map((week, index) =>
      h("div", {
        class: "heatmap-month",
        text:
          index === 0 || weeks[index - 1][0].getMonth() !== week[0].getMonth()
            ? monthFormat.format(week[0])
            : "",
      }),
    ),
  );

  const grid = h(
    "div",
    { class: "heatmap-grid" },
    ...weeks.map((week) =>
      h(
        "div",
        { class: "heatmap-week" },
        ...week.map((date) => {
          if (date > today) return h("div", { class: "heatmap-cell future" });

          const entry = byDay.get(dayKey(date));
          const seconds = entry?.totalSeconds ?? 0;
          const cell = h("div", { class: `heatmap-cell l${level(seconds)}` });

          cell.addEventListener("mousemove", (event) =>
            showTip(
              event,
              tipContent(dateFormat.format(date), [
                ["played", seconds ? fmt.duration(seconds) : "nothing"],
                ...(aggregate && entry?.playerCount
                  ? [["players", fmt.number(entry.playerCount)]]
                  : []),
                ...(entry?.games ?? [])
                  .slice(0, 3)
                  .map((game) => [
                    fmt.isGameNamed(game) ? game.name.trim() : `${game.shop}/${game.objectId}`,
                    fmt.duration(game.seconds),
                  ]),
              ]),
            ),
          );
          cell.addEventListener("mouseleave", hideTip);
          return cell;
        }),
      ),
    ),
  );

  root.append(
    h("div", { class: "heatmap-chart" }, weekdays, h("div", { class: "heatmap-weeks" }, months, grid)),
    h(
      "div",
      { class: "heatmap-legend" },
      h("span", { text: "Less" }),
      ...Array.from({ length: LEVELS + 1 }, (_, index) => h("div", { class: `heatmap-cell l${index}` })),
      h("span", { text: "More" }),
    ),
  );

  return root;
}
