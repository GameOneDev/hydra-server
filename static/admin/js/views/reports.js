/**
 * Souvenirs players reported.
 *
 * One row per picture, however many people reported it, because the decision
 * is about the picture: dismiss the reports, hide it from the owner's
 * profile, or delete it. Expanding a row shows the picture and every report.
 */

import { h } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { setQuery, navigate } from "/assets/shared/js/router.js";
import { card, userCell, gameCell, pill, emptyState, confirm, withBusy } from "/assets/shared/js/components/ui.js";
import { dataTable, segmented } from "/assets/shared/js/components/table.js";
import { invalidate } from "/assets/admin/js/store.js";

const REASONS = {
  hate: "Hate",
  sexual_content: "Sexual content",
  violence: "Violence",
  spam: "Spam",
  other: "Other",
};

const STATUS = {
  open: ["open", "warning"],
  dismissed: ["dismissed", ""],
  hidden: ["hidden", "accent"],
  removed: ["deleted", "critical"],
};

const reasonLabel = (reason) => REASONS[reason] ?? reason;

function statusPill(status) {
  const [text, kind] = STATUS[status] ?? [status, ""];
  return pill(text, kind);
}

function shot(row, size = "") {
  if (!row.present || !row.url) {
    return h("div", { class: `report-shot missing ${size}`, text: "gone" });
  }
  return h(
    "a",
    { href: row.url, target: "_blank", rel: "noreferrer", title: "Open the full picture" },
    h("img", { class: `report-shot ${size}`, src: row.url, alt: "", loading: "lazy" }),
  );
}

/** The decisions available for a row, as buttons. */
function actions(row, ctx) {
  const act = (button, action, done) =>
    withBusy(
      button,
      () => api.post(`/admin/api/reports/${encodeURIComponent(row.souvenirId)}/resolve`, { action }),
      { done },
    ).then(() => {
      invalidate();
      ctx.refresh();
    });

  const button = (label, kind, onclick) => {
    const node = h("button", { class: `btn small ${kind}`, text: label });
    node.addEventListener("click", (event) => {
      event.stopPropagation();
      onclick(node);
    });
    return node;
  };

  if (row.status !== "open") {
    return [button("Reopen", "", (node) => act(node, "reopen", "Reports reopened"))];
  }

  return [
    button("Dismiss", "", (node) => act(node, "dismiss", "Reports dismissed")),
    row.present && row.visibility !== "PRIVATE"
      ? button("Hide", "", (node) => act(node, "hide", "Picture hidden from the profile"))
      : null,
    row.present
      ? button("Delete", "danger", async (node) => {
          const sure = await confirm({
            title: "Delete this souvenir?",
            body: `The picture is removed from ${row.owner.displayName || row.owner.id}'s profile and from disk. The reports are kept.`,
            confirmLabel: "Delete picture",
            danger: true,
          });
          if (sure) await act(node, "delete", "Souvenir deleted");
        })
      : null,
  ].filter(Boolean);
}

/** Expanded row: the picture at a readable size, and every report on it. */
async function details(row) {
  const data = await api.get(`/admin/api/reports/${encodeURIComponent(row.souvenirId)}`);

  return h(
    "div",
    { class: "report-detail" },
    shot(data, "lg"),
    h(
      "div",
      { class: "stack", style: { gap: "10px", minWidth: 0 } },
      h(
        "div",
        { class: "muted small" },
        data.achievementName ? `${data.achievementName} · ` : "",
        data.present
          ? data.visibility === "PRIVATE"
            ? "private — not on the owner's profile"
            : "public on the owner's profile"
          : "the picture no longer exists",
      ),
      ...data.reports.map((report) =>
        h(
          "div",
          { class: "report-entry" },
          h(
            "div",
            { class: "row wrap", style: { gap: "8px" } },
            userCell(report.reporter),
            pill(reasonLabel(report.reason), "warning"),
            h("span", { class: "muted small", title: fmt.dateTime(report.createdAt), text: fmt.relative(report.createdAt) }),
          ),
          report.description ? h("p", { class: "report-text", text: report.description }) : null,
        ),
      ),
    ),
  );
}

export default {
  title: "Reports",
  subtitle: "Souvenirs players flagged",

  async render(ctx) {
    const { query } = ctx;
    const status = query.status ?? "open";
    const data = await api.get("/admin/api/reports", {
      status,
      page: query.page,
      perPage: 25,
    });

    ctx.setHeader({
      title: "Reports",
      subtitle: data.openTotal
        ? `${fmt.plural(data.openTotal, "souvenir")} waiting for a decision`
        : "Nothing waiting for a decision",
    });

    const table = dataTable({
      columns: [
        { key: "picture", label: "Picture", width: "92px", render: (row) => shot(row) },
        {
          key: "owner",
          label: "Owner",
          render: (row) => userCell(row.owner),
        },
        {
          key: "game",
          label: "Game",
          render: (row) => (row.game.shop ? gameCell(row.game) : h("span", { class: "muted", text: "—" })),
        },
        {
          key: "reasons",
          label: "Reasons",
          render: (row) =>
            h(
              "div",
              { class: "row wrap", style: { gap: "6px" } },
              ...row.reasons.map((reason) => pill(reasonLabel(reason), "warning")),
            ),
        },
        {
          key: "reports",
          label: "Reports",
          align: "right",
          render: (row) => h("span", { class: "num", text: fmt.number(row.reportCount) }),
        },
        {
          key: "when",
          label: "Last reported",
          render: (row) =>
            h("span", { class: "muted", title: fmt.dateTime(row.lastReportedAt), text: fmt.relative(row.lastReportedAt) }),
        },
        { key: "status", label: "Status", render: (row) => statusPill(row.status) },
        {
          key: "actions",
          label: "",
          class: "actions",
          render: (row) => h("div", { class: "row", style: { gap: "6px", justifyContent: "flex-end" } }, ...actions(row, ctx)),
        },
      ],
      rows: data.rows,
      page: data,
      onPage: (page) => setQuery({ page }),
      expand: details,
      empty: emptyState(
        status === "open" ? "No reports waiting" : "No reports here",
        status === "open"
          ? "When a player reports a souvenir on someone's profile, it shows up here."
          : null,
        "good",
      ),
    });

    return card({
      body: h(
        "div",
        {},
        h(
          "div",
          { class: "card-head" },
          segmented({
            value: status,
            onChange: (value) => setQuery({ status: value === "open" ? null : value, page: null }),
            options: [
              { label: "Open", value: "open" },
              { label: "Resolved", value: "resolved" },
              { label: "All", value: "all" },
            ],
          }),
          h("span", { class: "spacer" }),
          h("button", {
            class: "btn small",
            text: "Report history",
            title: "Every report in the event log",
            onclick: () => navigate("/events?kind=souvenir.reported"),
          }),
        ),
        table,
      ),
    });
  },
};
