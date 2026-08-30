/**
 * The maintenance schedule: what this server runs on its own.
 *
 * One card per task, each carrying everything there is to know about it —
 * whether it runs at all, how often, at what time, when it next comes due,
 * how the last run went, and the log of the ones before that. Edits save as
 * they are made: there is no form to submit, so the screen never shows a
 * schedule the server hasn't got.
 */

import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import { card, pill, statTile, emptyState, toast } from "/assets/shared/js/components/ui.js";
import { navigate } from "/assets/shared/js/router.js";

export default {
  title: "Schedule",
  subtitle: "What runs on its own, when it runs, and how the last run went",

  async render(ctx) {
    const data = await api.get("/admin/api/schedule");

    return h(
      "div",
      { class: "grid" },
      summaryCard(data),
      h("div", { class: "grid cols-2" }, ...data.tasks.map((task) => taskCard(task, data, ctx))),
      explainerCard(),
    );
  },
};

// ------------------------------------------------------------------ summary

function summaryCard(data) {
  const enabled = data.tasks.filter((task) => task.enabled);
  const failing = data.tasks.filter((task) => task.lastStatus === "error");

  /* The soonest run across every enabled task — the answer to "is anything
     going to happen tonight", which is the whole point of the screen. */
  const next = enabled
    .filter((task) => task.nextRunAt)
    .sort((a, b) => new Date(a.nextRunAt) - new Date(b.nextRunAt))[0];

  return card({
    title: "Schedule",
    subtitle: `all times UTC — it is ${utcClock(data.now)} UTC now`,
    body: h(
      "div",
      { class: "card-body", style: { display: "grid", gap: "14px" } },
      failing.length ? failureAlert(failing) : null,
      h(
        "div",
        { class: "grid cols-3" },
        statTile({
          label: "Running automatically",
          value: `${enabled.length}`,
          sub: `of ${fmt.plural(data.tasks.length, "task")}`,
        }),
        statTile({
          label: "Next run",
          value: next ? fmt.relative(next.nextRunAt) : "—",
          sub: next ? next.title : "nothing is scheduled",
        }),
        statTile({
          label: "Failed last run",
          value: `${failing.length}`,
          sub: failing.length ? "the task's log says why" : "everything went through",
          tone: failing.length ? "critical" : "",
        }),
      ),
    ),
  });
}

function failureAlert(failing) {
  return h(
    "div",
    { class: "alert critical" },
    icon("critical", 18),
    h(
      "div",
      { class: "stack", style: { flex: 1 } },
      h("div", { class: "title", text: `${fmt.plural(failing.length, "task")} failed on the last run` }),
      h("div", {
        class: "detail",
        text: failing.map((task) => `${task.title}: ${task.lastSummary}`).join(" · "),
      }),
    ),
  );
}

// --------------------------------------------------------------------- task

/**
 * A task, and every control for it.
 *
 * The card owns a copy of the task and repaints itself from whatever the
 * server last said, so a refused edit snaps the controls back to the stored
 * schedule instead of leaving the screen claiming something untrue.
 */
function taskCard(initial, meta, ctx) {
  const host = h("div", {});
  let task = initial;
  let log = null;

  const apply = (updated) => {
    task = updated;
    paint();
    /* The Overview alerts a failed task, and the sidebar counts move when a
       job deletes something. */
    ctx.refreshChrome?.();
  };

  const save = async (patch, control) => {
    control.disabled = true;
    try {
      const response = await api.put(`/admin/api/schedule/${encodeURIComponent(task.id)}`, patch);
      toast(response.summary, "good");
      apply(response.task);
    } catch (error) {
      toast(error.message, "critical");
      /* Repaint from the unchanged task, so the control snaps back to what is
         actually stored rather than to what was asked for. */
      paint();
    }
  };

  const runNow = async (button) => {
    button.disabled = true;
    button.textContent = "Running…";
    try {
      const response = await api.post(`/admin/api/schedule/${encodeURIComponent(task.id)}/run`);
      toast(response.result.summary, "good");
      if (log) log = await loadLog(task.id);
      apply(response.task);
    } catch (error) {
      toast(error.message, "critical");
      /* A failed run is still a run: re-read the task so its status and log
         show what just happened rather than what came before. */
      const { task: refreshed } = await api
        .get(`/admin/api/schedule/${encodeURIComponent(task.id)}`)
        .catch(() => ({ task }));
      if (log) log = await loadLog(task.id).catch(() => log);
      apply(refreshed);
    }
  };

  const toggleLog = async (button) => {
    if (log) {
      log = null;
      paint();
      return;
    }
    button.disabled = true;
    try {
      log = await loadLog(task.id);
      paint();
    } catch (error) {
      toast(error.message, "critical");
      button.disabled = false;
    }
  };

  const paint = () => host.replaceChildren(render());

  function render() {
    const enabled = h("input", {
      type: "checkbox",
      checked: task.enabled,
      "aria-label": `Run ${task.title} automatically`,
      onchange: (event) => save({ enabled: event.target.checked }, event.target),
    });

    const frequency = h(
      "select",
      {
        class: "input",
        "aria-label": "How often",
        onchange: (event) => save({ intervalMinutes: Number(event.target.value) }, event.target),
      },
      ...frequencyOptions(meta.frequencies, task.intervalMinutes),
    );

    /* A time of day only means something for a cadence that is a whole number
       of days; for anything shorter the field says so rather than take a
       value the server would ignore. */
    const daily = isDaily(meta.frequencies, task.intervalMinutes);
    const time = h("input", {
      class: "input",
      type: "time",
      value: daily ? clock(task.atMinute ?? 0) : "",
      disabled: !daily,
      "aria-label": "Run time, UTC",
      onchange: (event) => {
        const minutes = parseClock(event.target.value);
        if (minutes === null) {
          toast("That isn't a time of day", "critical");
          paint();
          return;
        }
        save({ atMinute: minutes }, event.target);
      },
    });

    const controls = h(
      "div",
      { class: "card-body", style: { display: "grid", gap: "14px" } },
      h("p", { class: "muted small", style: { margin: 0 }, text: task.description }),
      h(
        "div",
        { class: "grid cols-2", style: { gap: "12px" } },
        h("div", { class: "field" }, h("label", { text: "How often" }), frequency),
        h(
          "div",
          { class: "field" },
          h("label", { text: "Run time (UTC)" }),
          time,
          h("span", { class: "hint", text: timeHint(daily, task.atMinute ?? 0) }),
        ),
      ),
      h(
        "dl",
        { class: "kv" },
        h("dt", { text: "Next run" }),
        h("dd", {}, nextRun(task)),
        h("dt", { text: "Last run" }),
        h("dd", {}, lastRun(task)),
      ),
      h(
        "div",
        { class: "row wrap", style: { gap: "8px" } },
        h("button", {
          class: "btn primary",
          text: task.running ? "Running…" : "Run now",
          disabled: task.running,
          onclick: (event) => runNow(event.target),
        }),
        h("button", {
          class: "btn",
          text: log ? "Hide log" : "Show log",
          onclick: (event) => toggleLog(event.target),
        }),
      ),
    );

    return card({
      title: task.title,
      subtitle: task.enabled ? task.scheduleLabel : `paused — would run ${task.cadenceLabel}`,
      actions: h(
        "label",
        { class: "checkline", title: "Run this automatically" },
        enabled,
        h("span", { text: task.enabled ? "On" : "Off" }),
      ),
      /* The log sits beside the controls rather than inside their grid: a grid
         item won't shrink below its content, and a long line would push the
         card over its neighbour instead of wrapping inside it. */
      body: h("div", {}, controls, log ? logSection(log) : null),
    });
  }

  paint();
  return host;
}

async function loadLog(id) {
  const { runs } = await api.get(`/admin/api/schedule/${encodeURIComponent(id)}/runs`);
  return runs;
}

function nextRun(task) {
  if (task.running) return pill("running now", "accent");
  if (!task.enabled) return h("span", { class: "muted", text: "not scheduled" });
  if (!task.nextRunAt) return h("span", { class: "muted", text: "—" });

  return h(
    "div",
    { class: "stack" },
    h("span", { text: fmt.relative(task.nextRunAt) }),
    h("span", { class: "muted small", text: `${utcStamp(task.nextRunAt)} UTC` }),
  );
}

function lastRun(task) {
  if (!task.lastRunAt) return h("span", { class: "muted", text: "never" });

  return h(
    "div",
    { class: "stack" },
    h(
      "div",
      { class: "row wrap", style: { gap: "8px" } },
      task.lastStatus === "error" ? pill("failed", "critical") : pill("ok", "good"),
      h("span", {
        class: "muted small",
        title: fmt.dateTime(task.lastRunAt),
        text: fmt.relative(task.lastRunAt),
      }),
      task.lastDurationMs === null || task.lastDurationMs === undefined
        ? null
        : h("span", { class: "muted small num", text: took(task.lastDurationMs) }),
    ),
    h("span", { class: "small", text: task.lastSummary ?? "" }),
  );
}

/** The log of one task: what ran, who started it, and what it changed. */
function logSection(runs) {
  return h(
    "div",
    {
      class: "card-body",
      style: { borderTop: "1px solid var(--border)", display: "grid", gap: "12px" },
    },
    runs.length
      ? h(
          "div",
          { class: "stack", style: { gap: "12px" } },
          h("span", { class: "muted small", text: "Run log — most recent first" }),
          ...runs.map(logRow),
        )
      : emptyState("Nothing has run yet", "The log fills in as this task runs.", "clock"),
  );
}

function logRow(run) {
  return h(
    "div",
    { class: "stack", style: { gap: "3px" } },
    h(
      "div",
      { class: "row wrap", style: { gap: "8px" } },
      run.status === "error" ? pill("failed", "critical") : pill("ok", "good"),
      h("span", {
        class: "muted small",
        title: `${fmt.dateTime(run.startedAt)} · ${utcStamp(run.startedAt)} UTC`,
        text: fmt.relative(run.startedAt),
      }),
      h("span", { class: "muted small", text: run.trigger === "manual" ? "by hand" : "on schedule" }),
      h("span", { class: "muted small num", text: took(run.durationMs) }),
    ),
    h("span", {
      class: "small",
      /* The counters the job reported, for the run whose one-line summary
         raises a question it doesn't answer. */
      title: run.detail ? JSON.stringify(run.detail, null, 2) : "",
      text: run.summary,
    }),
  );
}

function explainerCard() {
  return card({
    title: "How the schedule works",
    actions: h("button", {
      class: "btn",
      text: "Run something now",
      onclick: () => navigate("/maintenance"),
    }),
    body: h(
      "div",
      { class: "card-body", style: { display: "grid", gap: "10px" } },
      h("p", {
        class: "muted small",
        style: { margin: 0 },
        text: "The server runs these itself — there is no cron entry to add. A run missed while the server was down happens once on the next check, not once per period it slept through, and a job never runs twice at the same time however it was started.",
      }),
      h("p", {
        class: "muted small",
        style: { margin: 0 },
        text: "Times are UTC so they don't move under a daylight-saving change. A cadence of a day or more lands on the run time you set; anything shorter simply runs that often. Every run — scheduled or by hand — is also recorded in History, so a webhook can carry it somewhere you'll see it.",
      }),
    ),
  });
}

// ------------------------------------------------------------------ helpers

function frequencyOptions(frequencies, current) {
  const options = frequencies.map((entry) =>
    h("option", {
      value: entry.minutes,
      selected: entry.minutes === current,
      text: capitalize(entry.label),
    }),
  );

  /* A cadence set through the API that isn't one of the offered ones still
     has to be selectable, or opening this screen would silently change it. */
  if (!frequencies.some((entry) => entry.minutes === current)) {
    options.unshift(h("option", { value: current, selected: true, text: `Every ${current} minutes` }));
  }
  return options;
}

function isDaily(frequencies, minutes) {
  const known = frequencies.find((entry) => entry.minutes === minutes);
  return known ? known.timeOfDay : minutes % 1440 === 0;
}

function capitalize(text) {
  return text.replace(/^./, (char) => char.toUpperCase());
}

/** A minute of the UTC day as HH:MM. */
function clock(minutes) {
  const value = (((Number(minutes) || 0) % 1440) + 1440) % 1440;
  return `${String(Math.floor(value / 60)).padStart(2, "0")}:${String(value % 60).padStart(2, "0")}`;
}

function parseClock(value) {
  const match = /^(\d{1,2}):(\d{2})$/.exec(value ?? "");
  if (!match) return null;
  const minutes = Number(match[1]) * 60 + Number(match[2]);
  return minutes >= 0 && minutes < 1440 ? minutes : null;
}

/**
 * What the run time means to whoever is reading it — theirs is the clock they
 * will be asleep in. A reader already on UTC is told that instead of being
 * shown the same number twice.
 */
function timeHint(daily, minutes) {
  if (!daily) return "only for cadences of a day or more";
  if (new Date().getTimezoneOffset() === 0) return "your clock is UTC too";
  return `${localTime(minutes)} where you are`;
}

function localTime(minutes) {
  const date = new Date();
  date.setUTCHours(Math.floor(minutes / 60), minutes % 60, 0, 0);
  return date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

function utcStamp(iso) {
  const date = new Date(iso);
  return Number.isNaN(+date) ? "—" : date.toISOString().slice(0, 16).replace("T", " ");
}

function utcClock(iso) {
  const date = new Date(iso ?? Date.now());
  return Number.isNaN(+date) ? "—" : date.toISOString().slice(11, 16);
}

function took(ms) {
  const value = Number(ms) || 0;
  return value < 1000 ? `${value} ms` : fmt.duration(value / 1000);
}
