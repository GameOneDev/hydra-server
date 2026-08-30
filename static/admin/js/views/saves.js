/**
 * Every stored save, across users and across all three storage generations.
 *
 * The table is exported so the user and game screens can show the same rows,
 * with the same actions, scoped to one owner — an operator learns it once.
 */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api, download } from "/assets/shared/js/api.js";
import { setQuery } from "/assets/shared/js/router.js";
import {
  card,
  userCell,
  gameCell,
  pill,
  kindPill,
  stateLabel,
  emptyState,
  confirm,
  toast,
  openDrawer,
} from "/assets/shared/js/components/ui.js";
import { dataTable, toolbar, segmented } from "/assets/shared/js/components/table.js";

export default {
  title: "Saves",
  subtitle: "Cloud saves, legacy backups and emulation memory cards",

  async render(ctx) {
    const { query } = ctx;
    const data = await api.get("/admin/api/saves", {
      type: query.type,
      state: query.state,
      userId: query.userId,
      q: query.q,
      sort: query.sort ?? "updated",
      dir: query.dir ?? "desc",
      page: query.page,
      perPage: 25,
    });

    ctx.setHeader({
      title: "Saves",
      subtitle: `${fmt.plural(data.total, "item")} · ${fmt.bytes(data.matchedBytes)} matched`,
    });

    return card({
      body: h(
        "div",
        {},
        toolbar({
          search: query.q,
          placeholder: "Search game, user, host or label…",
          onSearch: (value) => setQuery({ q: value, page: null }),
          children: [
            segmented({
              value: query.type ?? "",
              onChange: (type) => setQuery({ type, page: null }),
              options: [
                { label: "All", value: "" },
                { label: "Cloud v2", value: "cloud" },
                { label: "Legacy", value: "legacy" },
                { label: "Emulation", value: "emulation" },
              ],
            }),
            segmented({
              value: query.state ?? "",
              onChange: (state) => setQuery({ state, page: null }),
              options: [
                { label: "Any state", value: "" },
                { label: "Incomplete", value: "pending" },
                { label: "Older versions", value: "superseded" },
              ],
            }),
          ],
        }),
        summary(data),
        savesTable({ data, ctx, showUser: true }),
      ),
    });
  },
};

function summary(data) {
  if (!data.byKind.length) return null;

  return h(
    "div",
    { class: "card-body tight row wrap", style: { gap: "18px", borderBottom: "1px solid var(--border)" } },
    ...data.byKind.map((entry) =>
      h(
        "div",
        { class: "row", style: { gap: "8px" } },
        kindPill(entry.kind),
        h("span", { class: "small text-2 num", text: `${fmt.number(entry.items)} · ${fmt.bytes(entry.bytes)}` }),
      ),
    ),
  );
}

/**
 * The shared rows. `showUser` is off on a user's own screen, where every row
 * would otherwise repeat the same name.
 */
export function savesTable({ data, ctx, showUser = true }) {
  const columns = [
    { key: "kind", label: "Kind", render: (row) => kindPill(row.kind) },
    { key: "game", label: "Game", sortable: true, render: (row) => gameCell(row.game) },
    showUser ? { key: "user", label: "User", sortable: true, render: (row) => userCell(row.user) } : null,
    {
      key: "size",
      label: "Size",
      sortable: true,
      align: "right",
      render: (row) =>
        h(
          "div",
          { class: "stack", style: { justifyItems: "end" } },
          h("span", { class: "num", text: fmt.bytes(row.sizeBytes) }),
          row.fileCount ? h("span", { class: "muted small", text: fmt.plural(row.fileCount, "file") }) : null,
        ),
    },
    {
      key: "state",
      label: "State",
      render: (row) =>
        h(
          "div",
          { class: "row", style: { gap: "6px" } },
          stateLabel(row.state),
          row.version ? pill(`v${row.version}`) : null,
          row.isFrozen ? pill("frozen", "accent") : null,
          row.shareCount ? pill(`${row.shareCount}× shared`) : null,
        ),
    },
    {
      key: "host",
      label: "Host",
      render: (row) =>
        h(
          "div",
          { class: "stack" },
          h("span", { class: "truncate", text: row.hostname || "—" }),
          h("span", { class: "muted small", text: row.detail || row.platform || "" }),
        ),
    },
    {
      key: "updated",
      label: "Updated",
      sortable: true,
      render: (row) =>
        h("span", { class: "muted", title: fmt.dateTime(row.at), text: fmt.relative(row.at) }),
    },
    { key: "actions", label: "", class: "actions", render: (row) => rowActions(row, ctx) },
  ].filter(Boolean);

  return dataTable({
    columns,
    rows: data.rows,
    sort: ctx.query.sort ?? "updated",
    dir: ctx.query.dir ?? "desc",
    onSort: (sort, dir) => setQuery({ sort, dir, page: null }),
    page: data,
    onPage: (page) => setQuery({ page }),
    expand: (row) => (row.kind === "cloud" ? manifest(row) : null),
    empty: emptyState("No saves here", "Nothing matches these filters.", "saves"),
  });
}

function rowActions(row, ctx) {
  const buttons = [];

  if (row.kind === "cloud") {
    buttons.push(
      h("button", {
        class: "btn small",
        text: "Details",
        onclick: () => snapshotDrawer(row.id),
      }),
    );
  } else {
    const path =
      row.kind === "legacy"
        ? `/admin/api/artifacts/${encodeURIComponent(row.id)}/download`
        : `/admin/api/emulation-saves/${encodeURIComponent(row.id)}/download`;
    buttons.push(
      h(
        "button",
        {
          class: "btn small",
          "aria-label": "Download",
          title: "Download",
          disabled: row.state === "pending",
          onclick: () => download(path),
        },
        icon("download", 14),
      ),
    );
  }

  if (row.kind === "legacy") {
    buttons.push(
      h(
        "button",
        {
          class: "btn small",
          "aria-pressed": String(row.isFrozen),
          title: row.isFrozen ? "Unfreeze (let the launcher rotate it)" : "Freeze (exempt from the per-game limit)",
          onclick: async () => {
            await api.post(`/admin/api/artifacts/${encodeURIComponent(row.id)}/freeze`, {
              frozen: !row.isFrozen,
            });
            toast(row.isFrozen ? "Backup unfrozen" : "Backup frozen", "good");
            ctx.refresh();
          },
        },
        icon("freeze", 14),
      ),
    );
  }

  buttons.push(
    h(
      "button",
      {
        class: "btn small danger",
        "aria-label": "Delete",
        title: "Delete",
        onclick: () => remove(row, ctx),
      },
      icon("trash", 14),
    ),
  );

  return buttons;
}

const DELETE_PATHS = {
  cloud: (id) => `/admin/api/cloud-saves/${encodeURIComponent(id)}`,
  legacy: (id) => `/admin/api/artifacts/${encodeURIComponent(id)}`,
  emulation: (id) => `/admin/api/emulation-saves/${encodeURIComponent(id)}`,
};

async function remove(row, ctx) {
  const name = fmt.gameName(row.game);
  const ok = await confirm({
    title: "Delete this save?",
    body:
      row.kind === "cloud"
        ? `The current cloud save for ${name} is deleted along with any file only it was keeping. The launcher re-uploads on the next sync.`
        : `${fmt.bytes(row.sizeBytes)} for ${name} is deleted from this server. This cannot be undone.`,
    confirmLabel: "Delete",
    danger: true,
  });
  if (!ok) return;

  const result = await api.del(DELETE_PATHS[row.kind](row.id));
  toast(`Deleted — ${fmt.bytes(result?.freedBytes ?? row.sizeBytes)} freed`, "good");
  ctx.refresh();
}

/** Inline manifest for a snapshot row. */
async function manifest(row) {
  const files = await api.get(`/admin/api/cloud-saves/${encodeURIComponent(row.id)}/files`);
  if (!files.length) return emptyState("Empty manifest", null, "file");

  return h(
    "table",
    { class: "sub" },
    h(
      "thead",
      {},
      h(
        "tr",
        {},
        h("th", { text: "Path" }),
        h("th", { class: "num", text: "Size" }),
        h("th", { text: "Modified" }),
        h("th", { text: "Hash" }),
        h("th", {}),
      ),
    ),
    h(
      "tbody",
      {},
      ...files.map((file) =>
        h(
          "tr",
          {},
          h("td", { class: "truncate", title: `${file.rawPath}/${file.relativePath}` },
            h("span", { text: `${file.rawPath}/${file.relativePath}` })),
          h(
            "td",
            { class: "num" },
            fmt.bytes(file.sizeBytes),
            file.stored ? null : h("span", { style: { marginLeft: "6px" } }, pill("no bytes", "critical")),
          ),
          h("td", { class: "muted", text: fmt.dateTime(file.lastModifiedAt) }),
          h("td", { class: "mono muted", title: file.hash, text: fmt.shortHash(file.hash) }),
          h(
            "td",
            { class: "actions" },
            file.stored
              ? h(
                  "button",
                  {
                    class: "btn small",
                    title: "Download this file",
                    onclick: () =>
                      download(
                        `/admin/api/cloud-saves/${encodeURIComponent(row.id)}/files/${file.hash}/download`,
                      ),
                  },
                  icon("download", 13),
                )
              : null,
          ),
        ),
      ),
    ),
  );
}

/** Full snapshot metadata — the fields the launcher round-trips. */
async function snapshotDrawer(id) {
  const snapshot = await api.get(`/admin/api/cloud-saves/${encodeURIComponent(id)}`);

  openDrawer({
    title: fmt.gameName(snapshot.game),
    subtitle: `${fmt.gameSub(snapshot.game)} · v${snapshot.version}`,
    body: h(
      "div",
      { class: "grid" },
      h(
        "div",
        { class: "row wrap" },
        stateLabel(snapshot.status),
        pill(`v${snapshot.version}`),
        pill(fmt.plural(snapshot.fileCount, "file")),
        pill(fmt.bytes(snapshot.sizeBytes)),
      ),
      h(
        "dl",
        { class: "kv" },
        h("dt", { text: "Owner" }),
        h("dd", {}, userCell(snapshot.user)),
        h("dt", { text: "Uploaded from" }),
        h("dd", { text: `${snapshot.hostname || "unknown host"}${snapshot.platform ? ` · ${snapshot.platform}` : ""}` }),
        h("dt", { text: "Created" }),
        h("dd", { text: fmt.dateTime(snapshot.createdAt) }),
        h("dt", { text: "Updated" }),
        h("dd", { text: fmt.dateTime(snapshot.updatedAt) }),
        h("dt", { text: "Aggregate hash" }),
        h("dd", { class: "mono", text: snapshot.aggregateHash }),
        h("dt", { text: "Custom paths" }),
        h("dd", {
          text: snapshot.customPathRawPaths.length ? snapshot.customPathRawPaths.join(", ") : "none",
        }),
        h("dt", { text: "Variants" }),
        h("dd", { class: "mono small", text: JSON.stringify(snapshot.variants) }),
      ),
    ),
  });
}
