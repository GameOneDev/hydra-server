/** Disk usage, and whether disk and database still agree. */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { navigate } from "/assets/shared/js/router.js";
import { card, statTile, pill, emptyState, toast, confirm } from "/assets/shared/js/components/ui.js";
import { stackedBar } from "/assets/shared/js/components/charts.js";
import { dataTable } from "/assets/shared/js/components/table.js";

export default {
  title: "Storage",
  subtitle: "Measured from disk, not from the database",

  async render(ctx) {
    const data = await api.get("/admin/api/storage");

    ctx.setHeader({
      title: "Storage",
      subtitle: `${fmt.bytes(data.diskBytes)} on disk · ${fmt.bytes(data.database.bytes)} database`,
    });

    const scanCard = card({
      title: "Integrity",
      subtitle: "Reconciles every stored file against the database",
      actions: h("button", { class: "btn primary", text: "Run scan", onclick: (event) => runScan(event.target) }),
      body: h(
        "div",
        { class: "card-body" },
        h("p", {
          class: "muted small",
          style: { margin: 0 },
          text: "Finds rows whose bytes are missing (a restore would come back short) and files nothing points at (space nothing will ever reclaim). Read-only — deleting is a separate, explicit step.",
        }),
      ),
    });

    const results = h("div", {});
    const runScan = async (button) => {
      button.disabled = true;
      button.textContent = "Scanning…";
      try {
        const report = await api.get("/admin/api/storage/integrity");
        results.replaceChildren(scanResults(report, ctx));
      } catch (error) {
        toast(error.message, "critical");
      } finally {
        button.disabled = false;
        button.textContent = "Run scan";
      }
    };

    return h(
      "div",
      { class: "grid" },
      h(
        "div",
        { class: "grid cols-4" },
        statTile({ label: "On disk", value: fmt.bytes(data.diskBytes) }),
        statTile({
          label: "Database",
          value: fmt.bytes(data.database.bytes),
          sub: data.database.files.map((file) => file.name).join(" · "),
        }),
        statTile({
          label: "Files",
          value: fmt.number(data.areas.reduce((sum, area) => sum + area.files, 0)),
          sub: "across every storage area",
        }),
        statTile({ label: "Storage root", value: h("span", { class: "mono small", text: data.root }) }),
      ),
      h(
        "div",
        { class: "grid split" },
        card({
          title: "By area",
          body: h(
            "div",
            { class: "card-body" },
            stackedBar(data.areas.map((area) => ({ label: area.label, value: area.bytes }))),
          ),
        }),
        card({
          title: "Disk vs database",
          body: dataTable({
            columns: [
              { key: "area", label: "Area", render: (row) => h("div", { class: "stack" },
                h("span", { class: "strong", text: row.label }),
                h("span", { class: "mono muted small", text: row.path })) },
              { key: "files", label: "Files", align: "right", render: (row) => fmt.number(row.files) },
              { key: "disk", label: "On disk", align: "right", render: (row) => fmt.bytes(row.bytes) },
              {
                key: "expected",
                label: "Database says",
                align: "right",
                render: (row) =>
                  row.tracked
                    ? h(
                        "div",
                        { class: "stack", style: { justifyItems: "end" } },
                        h("span", { class: "num", text: fmt.bytes(row.expectedBytes) }),
                        drift(row),
                      )
                    : h("span", { class: "muted", text: "not tracked" }),
              },
            ],
            rows: data.areas,
          }),
        }),
      ),
      scanCard,
      results,
    );
  },
};

/** How far disk and database have drifted for one area. */
function drift(area) {
  const delta = area.bytes - area.expectedBytes;
  /* Small differences are normal: a file is rounded up to a block, an upload
     landed a moment ago. Only call out a gap worth investigating. */
  const tolerance = Math.max(64 * 1024, area.expectedBytes * 0.01);
  if (Math.abs(delta) <= tolerance) return pill("in sync", "good");
  return pill(`${delta > 0 ? "+" : "−"}${fmt.bytes(Math.abs(delta))} on disk`, "warning");
}

function scanResults(report, ctx) {
  if (report.healthy) {
    return card({
      title: "Scan result",
      body: h(
        "div",
        { class: "empty" },
        icon("good", 22),
        h("div", { class: "title", text: "Everything reconciles" }),
        h("div", { class: "small muted", text: `Checked ${fmt.dateTime(report.checkedAt)} — every row has its bytes and every file has a row.` }),
      ),
    });
  }

  const blocks = [];

  if (report.missingCount) {
    blocks.push(
      card({
        className: "danger-zone",
        title: `${fmt.plural(report.missingCount, "missing file")}`,
        subtitle: "Rows whose bytes are not on disk",
        body: h(
          "div",
          {},
          h("div", { class: "card-body tight muted small" },
            h("span", { text: "A restore that needs one of these comes back incomplete. Deleting the owning save is usually the honest fix — the launcher then re-uploads from the machine that still has the files." })),
          dataTable({
            columns: [
              { key: "kind", label: "Kind", render: (row) => pill(row.kind, "critical") },
              { key: "key", label: "Storage key", render: (row) => h("span", { class: "mono small", text: row.key }) },
              {
                key: "owner",
                label: "Owner",
                render: (row) =>
                  row.detail.userId
                    ? h("a", { href: `#/users/${encodeURIComponent(row.detail.userId)}`, text: row.detail.userId })
                    : h("span", { class: "muted", text: "—" }),
              },
              {
                key: "bytes",
                label: "Size",
                align: "right",
                render: (row) => (row.detail.bytes ? fmt.bytes(row.detail.bytes) : "—"),
              },
            ],
            rows: report.missing,
          }),
        ),
      }),
    );
  }

  if (report.orphanCount) {
    blocks.push(
      card({
        title: `${fmt.plural(report.orphanCount, "orphaned file")}`,
        subtitle: `${fmt.bytes(report.orphanBytes)} nothing points at`,
        actions: h("button", {
          class: "btn danger",
          text: "Delete all orphans",
          onclick: async (event) => {
            const ok = await confirm({
              title: "Delete orphaned files?",
              body: `${fmt.plural(report.orphanCount, "file")} totalling ${fmt.bytes(report.orphanBytes)} will be removed from disk. Each one is re-checked against the database first, so anything claimed since the scan is skipped.`,
              confirmLabel: "Delete files",
              danger: true,
            });
            if (!ok) return;

            event.target.disabled = true;
            const result = await api.post("/admin/api/maintenance/delete-orphan-files", {
              keys: report.orphans.map((entry) => entry.key),
            });
            toast(
              `${result.result.summary} ${fmt.bytes(result.result.freedBytes)} freed.`,
              "good",
            );
            ctx.refresh();
          },
        }),
        body: dataTable({
          columns: [
            { key: "key", label: "Storage key", render: (row) => h("span", { class: "mono small", text: row.key }) },
            { key: "bytes", label: "Size", align: "right", render: (row) => fmt.bytes(row.bytes) },
          ],
          rows: report.orphans.slice(0, 100),
          empty: emptyState("No orphans", null, "good"),
        }),
      }),
    );
  }

  blocks.push(
    h(
      "div",
      { class: "row" },
      h("span", { class: "muted small", text: `Scanned ${fmt.dateTime(report.checkedAt)}` }),
      h("span", { class: "spacer", style: { flex: 1 } }),
      h("button", { class: "btn small", text: "Schedule", onclick: () => navigate("/schedule") }),
    ),
  );

  return h("div", { class: "grid" }, ...blocks);
}
