/** Operations that would otherwise only happen lazily, on demand. */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api, download, upload } from "/assets/shared/js/api.js";
import { card, pill, confirm, toast, emptyState } from "/assets/shared/js/components/ui.js";
import { dataTable } from "/assets/shared/js/components/table.js";
import { navigate } from "/assets/shared/js/router.js";

/**
 * A restore report, held across the refresh that the restore itself triggers.
 *
 * The list has to be re-fetched afterwards — the safety backup is a new row —
 * but the report is the only record of what just happened, so it survives one
 * render rather than being wiped by the reload it caused.
 */
let restoreReport = null;

export default {
  title: "Maintenance",
  subtitle: "Run the housekeeping this server normally does on its own",

  async render(ctx) {
    const [{ actions }, backups] = await Promise.all([
      api.get("/admin/api/maintenance"),
      api.get("/admin/api/backups"),
    ]);

    const report = restoreReport;
    restoreReport = null;

    return h(
      "div",
      { class: "grid" },
      backupsCard(backups, ctx, report),
      h(
        "div",
        { class: "grid cols-2" },
        ...actions.map((action) => actionCard(action, ctx)),
        exportCard(),
      ),
    );
  },
};

/**
 * Database backups.
 *
 * The save files on disk are easy to copy with any tool; the database is the
 * part that maps them back to games and users, and losing it turns every blob
 * into an unidentifiable file.
 */
function backupsCard(data, ctx, report) {
  const disk = data.disk.totalBytes
    ? `${fmt.bytes(data.disk.freeBytes)} free of ${fmt.bytes(data.disk.totalBytes)}`
    : "";

  /* Hidden, and clicked by the button beside it: a bare file input can't be
     styled to sit with the other actions without fighting the browser. */
  const picker = h("input", {
    type: "file",
    accept: ".db,.sqlite,.sqlite3,application/octet-stream",
    style: { display: "none" },
    onchange: async (event) => {
      const file = event.target.files?.[0];
      event.target.value = "";
      if (!file) return;

      const pending = toast(`Uploading ${file.name}…`);
      try {
        const result = await upload("/admin/api/backups/upload", file);
        toast(`Stored as ${result.backup.name}`, "good");
        ctx.refresh();
      } catch (error) {
        toast(error.message, "critical");
      } finally {
        pending.remove();
      }
    },
  });

  return card({
    title: "Database backups",
    subtitle: data.schedule.enabled
      ? `${data.schedule.label}, keeping ${data.schedule.keep}`
      : "automatic backups are off",
    actions: h(
      "div",
      { class: "row wrap", style: { gap: "8px" } },
      picker,
      h("button", {
        class: "btn",
        text: "Schedule",
        title: "When this runs on its own",
        onclick: () => navigate("/schedule"),
      }),
      h("button", {
        class: "btn",
        text: "Upload backup",
        title: "Add a backup file taken from this server elsewhere",
        onclick: () => picker.click(),
      }),
      h("button", {
        class: "btn primary",
        text: "Back up now",
        onclick: async (event) => {
          event.target.disabled = true;
          try {
            const result = await api.post("/admin/api/backups");
            toast(`Backup written — ${fmt.bytes(result.backup.bytes)}`, "good");
            ctx.refresh();
          } catch (error) {
            toast(error.message, "critical");
            event.target.disabled = false;
          }
        },
      }),
    ),
    body: h(
      "div",
      {},
      report ? h("div", { class: "card-body tight" }, restoreResult(report)) : null,
      h(
        "div",
        { class: "card-body tight row wrap", style: { gap: "18px" } },
        h("span", { class: "muted small mono", text: data.directory }),
        disk ? h("span", { class: "muted small", text: disk }) : null,
        data.schedule.nextRunAt
          ? h("span", {
              class: "muted small",
              title: fmt.dateTime(data.schedule.nextRunAt),
              text: `next ${fmt.relative(data.schedule.nextRunAt)}`,
            })
          : null,
      ),
      data.backups.length
        ? dataTable({
            columns: [
              { key: "name", label: "File", render: (row) => h("span", { class: "mono", text: row.name }) },
              { key: "size", label: "Size", align: "right", render: (row) => fmt.bytes(row.bytes) },
              {
                key: "created",
                label: "Created",
                render: (row) =>
                  h("span", { class: "muted", title: fmt.dateTime(row.createdAt), text: fmt.relative(row.createdAt) }),
              },
              {
                key: "actions",
                label: "",
                class: "actions",
                render: (row) => [
                  h("button", {
                    class: "btn small",
                    text: "Restore",
                    onclick: (event) => restoreBackup(row, event.target, ctx),
                  }),
                  h("button", {
                    class: "btn small",
                    text: "Download",
                    onclick: () =>
                      download(`/admin/api/backups/${encodeURIComponent(row.name)}/download`),
                  }),
                  h("button", {
                    class: "btn small danger",
                    text: "Delete",
                    onclick: async () => {
                      const ok = await confirm({
                        title: "Delete this backup?",
                        body: `${row.name} (${fmt.bytes(row.bytes)}) is removed from disk.`,
                        confirmLabel: "Delete",
                        danger: true,
                      });
                      if (!ok) return;
                      await api.del(`/admin/api/backups/${encodeURIComponent(row.name)}`);
                      toast("Backup deleted", "good");
                      ctx.refresh();
                    },
                  }),
                ],
              },
            ],
            rows: data.backups,
          })
        : emptyState(
            "No backups yet",
            "Take one now, upload one, or wait for the scheduled run.",
            "storage",
          ),
    ),
  });
}

/**
 * Swaps the live database for a backup's contents.
 *
 * The word has to be typed: this replaces every row on the server, and unlike
 * the other destructive tools there is no partial version of it to preview.
 */
async function restoreBackup(row, button, ctx) {
  const ok = await confirm({
    title: `Restore from ${row.name}?`,
    body: h(
      "div",
      { class: "stack", style: { gap: "10px" } },
      h("p", {
        style: { margin: 0 },
        text: `Every user, save, snapshot and setting is replaced with what this backup held ${fmt.relative(row.createdAt)}. Anything recorded since then is gone.`,
      }),
      h("p", {
        class: "muted small",
        style: { margin: 0 },
        text: "A backup of the current database is taken first, so this can be undone by restoring that one. Save files on disk are untouched — run the storage integrity scan afterwards to reconcile them.",
      }),
    ),
    confirmLabel: "Restore",
    danger: true,
    requireText: "restore",
  });
  if (!ok) return;

  button.disabled = true;
  const label = button.textContent;
  button.textContent = "Restoring…";

  try {
    restoreReport = await api.post(`/admin/api/backups/${encodeURIComponent(row.name)}/restore`);
    toast(restoreReport.summary, "good");
    ctx.refresh();
  } catch (error) {
    toast(error.message, "critical");
    button.disabled = false;
    button.textContent = label;
  }
}

function restoreResult(report) {
  return h(
    "div",
    { class: "alert info" },
    icon("good", 18),
    h(
      "div",
      { class: "stack", style: { flex: 1, gap: "6px" } },
      h("div", { class: "title", text: report.summary }),
      h(
        "div",
        { class: "detail" },
        "Previous database saved as ",
        h("span", { class: "mono", text: report.safetyBackup }),
        ".",
      ),
      h("div", { class: "detail muted small", text: report.next }),
    ),
    h("button", {
      class: "btn small",
      text: "Check storage",
      onclick: () => navigate("/storage"),
    }),
  );
}

function actionCard(action, ctx) {
  const output = h("div", {});

  const run = async (event) => {
    if (action.danger) {
      const ok = await confirm({
        title: action.title,
        body: action.description,
        confirmLabel: "Run it",
        danger: true,
      });
      if (!ok) return;
    }

    const button = event.target;
    button.disabled = true;
    const label = button.textContent;
    button.textContent = "Running…";

    try {
      const response = await api.post(`/admin/api/maintenance/${action.id}`, {});
      /* The report *is* the outcome — refresh only the chrome, or a
         re-render would wipe what the operator just asked for. */
      output.replaceChildren(result(response.result));
      toast(response.result.summary, "good");
      ctx.refreshChrome?.();
    } catch (error) {
      output.replaceChildren(
        h("div", { class: "alert critical" }, icon("critical", 16), h("div", { class: "detail", text: error.message })),
      );
    } finally {
      button.disabled = false;
      button.textContent = label;
    }
  };

  return card({
    title: action.title,
    actions: h("button", { class: `btn ${action.danger ? "danger" : ""}`, text: "Run", onclick: run }),
    body: h(
      "div",
      { class: "card-body", style: { display: "grid", gap: "12px" } },
      h("p", { class: "muted small", style: { margin: 0 }, text: action.description }),
      action.danger ? h("div", {}, pill("destructive", "critical")) : null,
      scheduleLine(action),
      output,
    ),
  });
}

/**
 * What the same job does when nobody is watching.
 *
 * Every button here is also a task on the Schedule screen, and an operator
 * about to run one by hand deserves to know it is due in an hour anyway.
 */
function scheduleLine(action) {
  if (!action.schedule) return null;

  const scheduled = action.schedule !== "off";
  return h(
    "div",
    { class: "row", style: { gap: "6px" } },
    icon("calendar", 13),
    h("span", {
      class: "muted small",
      text: scheduled ? `Also runs ${action.schedule}` : "Not scheduled",
    }),
    h("a", { class: "small", href: "#/schedule", text: scheduled ? "Change" : "Schedule it" }),
  );
}

/** Whatever the action reports, rendered generically: every tool returns a
 *  summary plus its own counters, and all of them are worth showing. */
function result(payload) {
  const rows = Object.entries(payload)
    .filter(([key]) => key !== "summary")
    .map(([key, value]) =>
      h(
        "div",
        { class: "row" },
        h("span", { class: "muted small", text: fmt.label(key) }),
        h("span", { class: "spacer", style: { flex: 1 } }),
        h("span", {
          class: "small num",
          text: key.toLowerCase().endsWith("bytes") ? fmt.bytes(value) : formatValue(value),
        }),
      ),
    );

  return h(
    "div",
    { class: "alert info" },
    icon("good", 16),
    h(
      "div",
      { class: "stack", style: { flex: 1 } },
      h("div", { class: "title", text: payload.summary }),
      ...rows,
    ),
  );
}

function formatValue(value) {
  if (Array.isArray(value)) return value.length ? `${value.length}` : "none";
  if (typeof value === "number") return fmt.number(value);
  return String(value);
}

function exportCard() {
  return card({
    title: "Export inventory",
    actions: h("button", {
      class: "btn",
      text: "Download JSON",
      onclick: async (event) => {
        event.target.disabled = true;
        try {
          const data = await api.get("/admin/api/maintenance/export");
          const blob = new Blob([JSON.stringify(data, null, 2)], { type: "application/json" });
          const url = URL.createObjectURL(blob);
          const link = h("a", {
            href: url,
            download: `hydra-server-inventory-${new Date().toISOString().slice(0, 10)}.json`,
          });
          document.body.append(link);
          link.click();
          link.remove();
          URL.revokeObjectURL(url);
          toast("Inventory downloaded", "good");
        } catch (error) {
          toast(error.message, "critical");
        } finally {
          event.target.disabled = false;
        }
      },
    }),
    body: h(
      "div",
      { class: "card-body" },
      h("p", {
        class: "muted small",
        style: { margin: 0 },
        text: "Every user, snapshot, backup and emulation save as JSON — what the server believes it holds. Useful for diffing two points in time or answering questions off-line. Not a backup of the save data itself; that is the storage directory.",
      }),
    ),
  });
}
