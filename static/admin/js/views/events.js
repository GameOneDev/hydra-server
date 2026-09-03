/**
 * The history screen.
 *
 * Everything the server recorded — syncs, operator actions, sign-ins,
 * background jobs — with the filters needed to answer a specific question:
 * what happened to this user, what did I delete last Tuesday, why is that
 * upload failing.
 */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { setQuery } from "/assets/shared/js/router.js";
import {
  card,
  userCell,
  gameCell,
  pill,
  emptyState,
  detailSummary,
  detailPanel,
} from "/assets/shared/js/components/ui.js";
import {
  dataTable,
  toolbar,
  segmented,
  columnMenu,
  chooseColumns,
} from "/assets/shared/js/components/table.js";

const CATEGORY_ICONS = {
  sync: "saves",
  admin: "tools",
  auth: "user",
  system: "storage",
};

const SEVERITY_TONE = { critical: "critical", warning: "warning", info: "" };

/**
 * Every column this screen knows how to draw, in table order.
 *
 * `fixed` columns are the ones that make a row identifiable at all; the rest
 * are the operator's call, and `default: false` keeps the table readable out
 * of the box while leaving the specialised ones a checkbox away.
 */
const COLUMNS = [
  {
    key: "at",
    label: "When",
    fixed: true,
    sortable: true,
    render: (row) =>
      h(
        "div",
        { class: "stack" },
        h("span", { text: fmt.relative(row.at) }),
        h("span", { class: "muted small", text: fmt.dateTime(row.at) }),
      ),
  },
  {
    key: "kind",
    label: "Event",
    fixed: true,
    render: (row) =>
      h(
        "div",
        { class: "row", style: { gap: "8px" } },
        icon(CATEGORY_ICONS[row.category] ?? "dot", 14),
        h(
          "div",
          { class: "stack", style: { minWidth: 0 } },
          h("span", { class: "truncate", text: row.summary }),
          h("span", { class: "mono muted small", text: row.kind }),
        ),
      ),
  },
  {
    key: "user",
    label: "User",
    render: (row) =>
      row.user?.id ? userCell(row.user) : h("span", { class: "muted", text: row.actor ?? "—" }),
  },
  {
    key: "actor",
    label: "Actor",
    default: false,
    /* Who did it, as opposed to who it was about — the same for a sync, very
       much not for an operator deleting someone else's save. */
    render: (row) => h("span", { class: "mono small", text: row.actor ?? "—" }),
  },
  {
    key: "game",
    label: "Game",
    render: (row) =>
      row.game?.objectId ? gameCell(row.game) : h("span", { class: "muted", text: "—" }),
  },
  {
    key: "ip",
    label: "IP address",
    render: (row) =>
      row.ip
        ? h("span", { class: "mono small", text: row.ip })
        : h("span", { class: "muted", text: "—" }),
  },
  {
    key: "category",
    label: "Category",
    default: false,
    render: (row) => pill(row.category),
  },
  {
    key: "size",
    label: "Size",
    align: "right",
    render: (row) => (row.sizeBytes ? fmt.bytes(row.sizeBytes) : ""),
  },
  {
    key: "other",
    label: "Other",
    default: false,
    /* Everything an event kept that no column is shaped for: file counts,
       failure reasons, how many rows a restore moved. Different keys per
       kind, which is exactly why they share one column. */
    render: (row) => detailSummary(row.detail),
  },
  {
    key: "severity",
    label: "",
    title: "Severity",
    render: (row) =>
      row.severity === "info" ? null : pill(row.severity, SEVERITY_TONE[row.severity]),
  },
];

export default {
  title: "History",
  subtitle: "Every recorded event, including the ones whose rows are long gone",

  async render(ctx) {
    const { query } = ctx;
    const [data, kinds] = await Promise.all([
      api.get("/admin/api/events", {
        q: query.q,
        category: query.category,
        severity: query.severity,
        kind: query.kind,
        userId: query.userId,
        from: query.from,
        to: query.to,
        dir: query.dir,
        page: query.page,
        perPage: 50,
      }),
      api.get("/admin/api/events/kinds"),
    ]);

    ctx.setHeader({
      title: "History",
      subtitle: `${fmt.plural(data.total, "event")}${query.userId ? " for this user" : ""}`,
    });

    const kindPicker = h(
      "select",
      {
        class: "select",
        "aria-label": "Event kind",
        onchange: (event) => setQuery({ kind: event.target.value, page: null }),
      },
      h("option", { value: "", text: "All kinds" }),
      ...kinds.map((entry) =>
        h("option", {
          value: entry.kind,
          selected: entry.kind === query.kind,
          text: `${entry.kind} (${entry.count})`,
        }),
      ),
    );

    const dateInput = (name, label) =>
      h("input", {
        class: "input",
        type: "date",
        "aria-label": label,
        value: query[name] ?? "",
        onchange: (event) => setQuery({ [name]: event.target.value, page: null }),
      });

    return card({
      body: h(
        "div",
        {},
        toolbar({
          search: query.q,
          placeholder: "Search summaries, kinds, users…",
          onSearch: (value) => setQuery({ q: value, page: null }),
          children: [
            segmented({
              value: query.category ?? "",
              onChange: (category) => setQuery({ category, page: null }),
              options: [
                { label: "All", value: "" },
                { label: "Sync", value: "sync" },
                { label: "Admin", value: "admin" },
                { label: "Auth", value: "auth" },
                { label: "System", value: "system" },
              ],
            }),
            segmented({
              value: query.severity ?? "",
              onChange: (severity) => setQuery({ severity, page: null }),
              options: [
                { label: "Any", value: "" },
                { label: "Warnings", value: "warning" },
                { label: "Critical", value: "critical" },
              ],
            }),
          ],
        }),
        h(
          "div",
          { class: "card-body tight row wrap", style: { gap: "10px", borderBottom: "1px solid var(--border)" } },
          kindPicker,
          dateInput("from", "From date"),
          h("span", { class: "muted small", text: "to" }),
          dateInput("to", "To date"),
          ...data.bySeverity
            .filter((entry) => entry.severity !== "info")
            .map((entry) => pill(`${entry.count} ${entry.severity}`, SEVERITY_TONE[entry.severity])),
          h("span", { class: "spacer", style: { flex: 1 } }),
          Object.keys(query).length
            ? h("button", {
                class: "btn small ghost",
                text: "Clear filters",
                onclick: () =>
                  setQuery({
                    q: null, category: null, severity: null, kind: null,
                    userId: null, from: null, to: null, page: null,
                  }),
              })
            : null,
          columnMenu({ id: "events", columns: COLUMNS, onChange: () => ctx.refresh() }),
        ),
        dataTable({
          columns: chooseColumns("events", COLUMNS),
          rows: data.rows,
          sort: "at",
          dir: query.dir ?? "desc",
          onSort: (_key, dir) => setQuery({ dir, page: null }),
          page: data,
          onPage: (page) => setQuery({ page }),
          /* The detail blob is where an event keeps what made it specific —
             worth a click, never worth a column. */
          expand: (row) => details(row),
          empty: emptyState(
            "No events match",
            "Try widening the filters, or wait for something to happen.",
            "clock",
          ),
        }),
      ),
    });
  },
};

function details(row) {
  return detailPanel(
    [
      ["Recorded", fmt.dateTime(row.at)],
      ["Kind", row.kind, true],
      ["Category", row.category],
      ["Severity", row.severity],
      ["Actor", row.actor ?? "—"],
      row.ip ? ["Address", row.ip] : null,
      row.user?.id ? ["Subject", row.user.displayName || row.user.id] : null,
      row.game?.objectId ? ["Game", fmt.gameName(row.game)] : null,
      row.sizeBytes ? ["Size", fmt.bytes(row.sizeBytes)] : null,
    ],
    row.detail,
  );
}
