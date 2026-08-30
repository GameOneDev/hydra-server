/** The user directory. */

import { h } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { setQuery, navigate } from "/assets/shared/js/router.js";
import { card, userCell, pill, meter, emptyState, toast } from "/assets/shared/js/components/ui.js";
import { dataTable, toolbar, segmented } from "/assets/shared/js/components/table.js";

export default {
  title: "Users",
  subtitle: "Everyone who has signed in to this server",

  async render(ctx) {
    const { query } = ctx;
    const data = await api.get("/admin/api/users", {
      q: query.q,
      status: query.status,
      sort: query.sort ?? "lastSeen",
      dir: query.dir ?? "desc",
      page: query.page,
      perPage: 25,
    });

    ctx.setHeader({
      title: "Users",
      subtitle: `${fmt.plural(data.total, "account")}${query.q ? ` matching “${query.q}”` : ""}`,
    });

    const table = dataTable({
      columns: [
        {
          key: "name",
          label: "User",
          sortable: true,
          render: (row) => userCell(row),
        },
        {
          key: "status",
          label: "Status",
          render: (row) =>
            h(
              "div",
              { class: "row wrap", style: { gap: "6px" } },
              row.isBlocked ? pill("blocked", "critical") : pill("active", "good"),
              /* Worth seeing from the directory: this account is not on the
                 server's limits, so the Settings screen doesn't explain it. */
              row.limits?.customised ? pill("custom limits", "accent") : null,
            ),
        },
        {
          key: "storage",
          label: "Storage",
          sortable: true,
          align: "right",
          render: (row) =>
            h(
              "div",
              { class: "stack", style: { justifyItems: "end", gap: "5px" } },
              h("span", { class: "num", text: fmt.bytes(row.usedBytes) }),
              row.quotaBytes
                ? h(
                    "div",
                    { class: "row", style: { gap: "6px" } },
                    meter(row.quotaRatio),
                    h("span", { class: "muted small num", text: fmt.percent(row.quotaRatio) }),
                  )
                : null,
            ),
        },
        {
          key: "cloudSaves",
          label: "Cloud saves",
          sortable: true,
          align: "right",
          render: (row) => fmt.number(row.counts.cloudSaves),
        },
        {
          key: "backups",
          label: "Backups",
          sortable: true,
          align: "right",
          render: (row) => fmt.number(row.counts.backups),
        },
        {
          key: "emulation",
          label: "Emulation",
          align: "right",
          render: (row) => fmt.number(row.counts.emulationSaves),
        },
        {
          key: "playtime",
          label: "Playtime",
          sortable: true,
          align: "right",
          render: (row) => fmt.duration(row.playtimeSeconds),
        },
        {
          key: "lastSeen",
          label: "Last seen",
          sortable: true,
          render: (row) =>
            h("span", { class: "muted", title: fmt.dateTime(row.lastSeenAt), text: fmt.relative(row.lastSeenAt) }),
        },
        {
          key: "actions",
          label: "",
          class: "actions",
          render: (row) =>
            h("button", {
              class: "btn small",
              text: row.isBlocked ? "Unblock" : "Block",
              onclick: async (event) => {
                event.stopPropagation();
                await api.post(`/admin/api/users/${encodeURIComponent(row.id)}/block`, {
                  blocked: !row.isBlocked,
                });
                toast(
                  `${row.displayName || row.id} ${row.isBlocked ? "unblocked" : "blocked"}`,
                  "good",
                );
                ctx.refresh();
              },
            }),
        },
      ],
      rows: data.rows,
      sort: query.sort ?? "lastSeen",
      dir: query.dir ?? "desc",
      onSort: (sort, dir) => setQuery({ sort, dir, page: null }),
      page: data,
      onPage: (page) => setQuery({ page }),
      onRow: (row) => navigate(`/users/${encodeURIComponent(row.id)}`),
      empty: emptyState(
        query.q ? "No users match that search" : "No users yet",
        query.q ? null : "Accounts appear the first time a launcher syncs against this server.",
        "users",
      ),
    });

    return card({
      className: "",
      body: h(
        "div",
        {},
        toolbar({
          search: query.q,
          placeholder: "Search name, username or id…",
          onSearch: (value) => setQuery({ q: value, page: null }),
          children: [
            segmented({
              value: query.status ?? "",
              onChange: (status) => setQuery({ status, page: null }),
              options: [
                { label: "All", value: "" },
                { label: "Active", value: "active" },
                { label: "Blocked", value: "blocked" },
              ],
            }),
          ],
        }),
        table,
      ),
    });
  },
};
