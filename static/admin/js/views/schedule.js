import { h, icon } from "/assets/shared/js/dom.js";
import * as fmt from "/assets/shared/js/format.js";
import { api } from "/assets/shared/js/api.js";
import {
  card,
  pill,
  statTile,
  emptyState,
  openDrawer,
  confirm,
  toast,
  detailPanel,
} from "/assets/shared/js/components/ui.js";
import { dataTable } from "/assets/shared/js/components/table.js";
import { navigate } from "/assets/shared/js/router.js";

export default {
  title: "Schedule",
  subtitle: "What runs on its own, what starts it, and how the last run went",

  async render(ctx) {
    const data = await api.get("/admin/api/schedule");

    return h(
      "div",
      { class: "grid" },
      summaryCard(data),
      tasksCard(data, ctx),
    );
  },
};

function summaryCard(data) {
  const enabled = data.tasks.filter((task) => task.enabled);
  const failing = data.tasks.filter((task) => task.lastStatus === "error");

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
          sub: next ? next.title : "nothing on a timer",
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
      h("div", {
        class: "title",
        text: `${fmt.plural(failing.length, "task")} failed on the last run`,
      }),
      h("div", {
        class: "detail",
        text: failing.map((task) => `${task.title}: ${task.lastSummary}`).join(" · "),
      }),
    ),
  );
}

function tasksCard(data, ctx) {
  return card({
    title: "Tasks",
    subtitle: "open one to change what starts it, or to read its log",
    body: dataTable({
      columns: [
        {
          key: "task",
          label: "Task",
          render: (task) =>
            h(
              "div",
              { class: "stack", style: { minWidth: 0 } },
              h("span", { class: "strong", text: task.title }),
              h("span", { class: "muted small truncate", title: task.description, text: task.description }),
            ),
        },
        {
          key: "triggers",
          label: "Runs when",
          render: (task) => triggerPills(task),
        },
        {
          key: "last",
          label: "Last run",
          render: (task) =>
            task.lastRunAt
              ? h(
                  "div",
                  { class: "row", style: { gap: "8px" } },
                  task.lastStatus === "error" ? pill("failed", "critical") : pill("ok", "good"),
                  h("span", {
                    class: "muted small",
                    title: `${fmt.dateTime(task.lastRunAt)} — ${task.lastSummary ?? ""}`,
                    text: fmt.relative(task.lastRunAt),
                  }),
                )
              : h("span", { class: "muted", text: "never" }),
        },
        {
          key: "next",
          label: "Next run",
          render: (task) => nextRun(task),
        },
        {
          key: "actions",
          label: "",
          class: "actions",
          render: (task) => [
            h("button", {
              class: `btn small${task.enabled ? " primary" : ""}`,
              text: task.enabled ? "On" : "Off",
              title: task.enabled ? "Runs automatically — click to pause" : "Paused — click to run it automatically",
              onclick: async (event) => {
                event.target.disabled = true;
                try {
                  const response = await api.put(
                    `/admin/api/schedule/${encodeURIComponent(task.id)}`,
                    { enabled: !task.enabled },
                  );
                  toast(response.summary, "good");
                } catch (error) {
                  toast(error.message, "critical");
                } finally {
                  ctx.refresh();
                }
              },
            }),
            h(
              "button",
              {
                class: "btn small icon-only",
                "aria-label": `Open ${task.title}`,
                onclick: () => openTask(task, data, ctx),
              },
              icon("chevronRight", 14),
            ),
          ],
        },
      ],
      rows: data.tasks,
      onRow: (task) => openTask(task, data, ctx),
    }),
  });
}

function triggerPills(task) {
  if (!task.enabled) return pill("off", "warning");
  if (!task.triggers.length) return h("span", { class: "muted small", text: "on demand only" });

  return h(
    "div",
    { class: "row wrap", style: { gap: "4px" } },
    ...task.triggers.slice(0, 2).map((trigger) => pill(trigger.label, TRIGGER_TONES[trigger.type] ?? "")),
    task.triggers.length > 2 ? pill(`+${task.triggers.length - 2}`) : null,
  );
}

const TRIGGER_TONES = {
  every: "",
  startup: "accent",
  afterTask: "accent",
  onEvent: "accent",
  condition: "warning",
};

function nextRun(task) {
  if (task.running) return pill("running now", "accent");
  if (!task.enabled) return h("span", { class: "muted", text: "—" });
  if (!task.nextRunAt) {
    return h("span", {
      class: "muted small",
      text: task.triggers.length ? "when it is asked for" : "—",
    });
  }

  return h("span", {
    title: `${utcStamp(task.nextRunAt)} UTC`,
    text: fmt.relative(task.nextRunAt),
  });
}

function openTask(initial, data, ctx) {
  let task = initial;
  let draft = clone(task.triggers);
  let log = null;

  const body = h("div", { class: "stack", style: { gap: "18px" } });
  const statusHost = h("div", { style: { minWidth: 0 } });
  const triggerHost = h("div", { style: { minWidth: 0 } });
  const logHost = h("div", { style: { minWidth: 0 } });
  body.append(statusHost, triggerHost, logHost);

  const dirty = () => canonical(draft) !== canonical(task.triggers);

  const adopt = (updated) => {
    task = updated;
    draft = clone(task.triggers);
  };

  const paintStatus = painter(statusHost, () => statusSection(task, runNow, toggle));
  const paintTriggers = painter(triggerHost, () =>
    triggerSection(draft, data.vocabulary, {
      dirty: dirty(),
      onChange: (index, trigger) => {
        draft = draft.map((existing, at) => (at === index ? trigger : existing));
        paintTriggers();
      },
      onAdd: (kind) => {
        draft = [...draft, blankTrigger(kind, task, data.vocabulary)];
        paintTriggers();
      },
      onRemove: (index) => {
        draft = draft.filter((_, at) => at !== index);
        paintTriggers();
      },
      onSave: save,
      onDiscard: () => {
        draft = clone(task.triggers);
        paintTriggers();
      },
    }),
  );
  const paintLog = painter(logHost, () =>
    logSection(task, log, async (button) => {
      button.disabled = true;
      try {
        log = await loadLog(task.id);
        paintLog();
      } catch (error) {
        toast(error.message, "critical");
        button.disabled = false;
      }
    }),
  );

  const paint = () => {
    paintStatus();
    paintTriggers();
    paintLog();
  };

  async function save(button) {
    button.disabled = true;
    button.textContent = "Saving…";
    try {
      const response = await api.put(`/admin/api/schedule/${encodeURIComponent(task.id)}`, {
        triggers: draft,
      });
      toast(response.summary, "good");
      adopt(response.task);
      paint();
      ctx.refresh();
    } catch (error) {
      toast(error.message, "critical");
      paintTriggers();
    }
  }

  async function toggle(value, control) {
    control.disabled = true;
    try {
      const response = await api.put(`/admin/api/schedule/${encodeURIComponent(task.id)}`, {
        enabled: value,
      });
      toast(response.summary, "good");
      task = response.task;
      paint();
      ctx.refresh();
    } catch (error) {
      toast(error.message, "critical");
      paint();
    }
  }

  async function runNow(button) {
    button.disabled = true;
    button.textContent = "Running…";
    try {
      const response = await api.post(`/admin/api/schedule/${encodeURIComponent(task.id)}/run`);
      toast(response.result.summary, "good");
      task = response.task;
    } catch (error) {
      toast(error.message, "critical");
      const { task: refreshed } = await api
        .get(`/admin/api/schedule/${encodeURIComponent(task.id)}`)
        .catch(() => ({ task }));
      task = refreshed;
    }
    log = await loadLog(task.id).catch(() => log);
    paintStatus();
    paintLog();
    ctx.refresh();
  }

  const warnOnUnload = (event) => {
    if (!dirty()) return;
    event.preventDefault();
    event.returnValue = "";
  };
  addEventListener("beforeunload", warnOnUnload);

  paint();
  loadLog(task.id)
    .then((runs) => {
      log = runs;
      paintLog();
    })
    .catch(() => {});

  openDrawer({
    title: initial.title,
    subtitle: initial.description,
    body,
    beforeClose: async () => {
      if (!dirty()) {
        removeEventListener("beforeunload", warnOnUnload);
        return true;
      }
      const leave = await confirm({
        title: "Leave without saving?",
        body: `The triggers for ${task.title} have been changed but not saved. Closing this now throws those changes away.`,
        confirmLabel: "Discard changes",
        danger: true,
      });
      if (leave) removeEventListener("beforeunload", warnOnUnload);
      return leave;
    },
  });
}

async function loadLog(id) {
  const { runs } = await api.get(`/admin/api/schedule/${encodeURIComponent(id)}/runs`);
  return runs;
}

function painter(host, build) {
  let queued = false;
  return () => {
    if (queued) return;
    queued = true;
    queueMicrotask(() => {
      queued = false;
      host.replaceChildren(build());
    });
  };
}

function clone(value) {
  return JSON.parse(JSON.stringify(value ?? []));
}

function canonical(value) {
  if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
  if (value && typeof value === "object") {
    return `{${Object.keys(value)
      .filter((key) => key !== "label" && value[key] !== undefined)
      .sort()
      .map((key) => `${key}:${canonical(value[key])}`)
      .join(",")}}`;
  }
  return JSON.stringify(value ?? null);
}

function statusSection(task, runNow, toggle) {
  const enabled = h("input", {
    type: "checkbox",
    checked: task.enabled,
    "aria-label": `Run ${task.title} automatically`,
    onchange: (event) => toggle(event.target.checked, event.target),
  });

  return section(
    "Status",
    h(
      "div",
      { class: "stack", style: { gap: "12px" } },
      h(
        "dl",
        { class: "kv" },
        h("dt", { text: "Schedule" }),
        h("dd", { text: task.summary }),
        h("dt", { text: "Next run" }),
        h("dd", {}, drawerNextRun(task)),
        h("dt", { text: "Last run" }),
        h("dd", {}, lastRun(task)),
      ),
      h(
        "div",
        { class: "row wrap", style: { gap: "10px" } },
        h("button", {
          class: "btn primary",
          text: task.running ? "Running…" : "Run now",
          disabled: task.running,
          onclick: (event) => runNow(event.target),
        }),
        h(
          "label",
          { class: "checkline", title: "Run this automatically" },
          enabled,
          h("span", { text: task.enabled ? "Runs automatically" : "Paused" }),
        ),
      ),
    ),
  );
}

function drawerNextRun(task) {
  if (task.running) return pill("running now", "accent");
  if (!task.enabled) return h("span", { class: "muted", text: "not scheduled" });
  if (!task.nextRunAt) {
    return h("span", {
      class: "muted",
      text: task.triggers.length ? "no timer — whenever a trigger fires" : "on demand only",
    });
  }
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

const TRIGGER_KINDS = [
  { type: "every", label: "On a timer" },
  { type: "startup", label: "When the server starts" },
  { type: "afterTask", label: "After another task" },
  { type: "onEvent", label: "When an event is recorded" },
  { type: "condition", label: "When a number crosses a line" },
];

function triggerSection(draft, vocabulary, handlers) {
  const add = h(
    "select",
    {
      class: "input",
      style: { maxWidth: "230px" },
      disabled: draft.length >= vocabulary.limits.maxTriggers,
      onchange: (event) => {
        const kind = event.target.value;
        event.target.value = "";
        if (kind) handlers.onAdd(kind);
      },
    },
    h("option", { value: "", selected: true, text: "Add a trigger…" }),
    ...TRIGGER_KINDS.map((kind) => h("option", { value: kind.type, text: kind.label })),
  );

  const save = h("button", {
    class: "btn primary",
    text: "Save changes",
    disabled: !handlers.dirty,
    onclick: (event) => handlers.onSave(event.target),
  });

  return section(
    h(
      "div",
      { class: "row", style: { gap: "8px" } },
      h("span", { text: "Runs when" }),
      handlers.dirty ? pill("unsaved", "warning") : null,
    ),
    h(
      "div",
      { class: "stack", style: { gap: "12px" } },
      h("span", {
        class: "muted small",
        text: "Any one of these starts the task. Times are UTC. Changes apply when you save.",
      }),
      draft.length
        ? h(
            "div",
            { class: "stack", style: { gap: "10px" } },
            ...draft.map((trigger, index) =>
              triggerRow(trigger, vocabulary, {
                onChange: (updated) => handlers.onChange(index, updated),
                onRemove: () => handlers.onRemove(index),
              }),
            ),
          )
        : h("span", {
            class: "muted small",
            text: "Nothing starts this on its own — Run now is the only way it happens.",
          }),
      add,
      h(
        "div",
        { class: "row wrap", style: { gap: "8px" } },
        save,
        h("button", {
          class: "btn",
          text: "Discard",
          disabled: !handlers.dirty,
          onclick: () => handlers.onDiscard(),
        }),
      ),
    ),
  );
}


function blankTrigger(type, task, vocabulary) {
  if (type === "every") return { type, count: 1, unit: "day", atMinute: 180 };
  if (type === "startup") return { type, delayMinutes: 5 };
  if (type === "afterTask") {
    const other = vocabulary.tasks.find((entry) => entry.id !== task.id);
    return { type, task: other?.id ?? task.id, delayMinutes: 0 };
  }
  if (type === "onEvent") return { type, kinds: [], minGapMinutes: 60 };

  const metric = vocabulary.metrics[0];
  return {
    type,
    metric: metric.metric,
    comparison: metric.comparison,
    value: metric.bytes ? 5 * 1024 ** 3 : 10,
    minGapMinutes: 60,
  };
}

function triggerRow(trigger, vocabulary, { onChange, onRemove }) {
  const fields = {
    every: everyFields,
    startup: startupFields,
    afterTask: afterFields,
    onEvent: eventFields,
    condition: conditionFields,
  }[trigger.type];

  return h(
    "div",
    {
      class: "stack",
      style: {
        gap: "8px",
        padding: "12px",
        border: "1px solid var(--border)",
        borderRadius: "10px",
        background: "var(--surface-2)",
      },
    },
    h(
      "div",
      { class: "row", style: { gap: "8px" } },
      h("span", {
        class: "small strong",
        text: TRIGGER_KINDS.find((kind) => kind.type === trigger.type)?.label ?? trigger.type,
      }),
      h("span", { class: "spacer", style: { flex: 1 } }),
      h(
        "button",
        {
          class: "btn small ghost icon-only",
          "aria-label": "Remove this trigger",
          title: "Remove this trigger",
          onclick: () => onRemove(),
        },
        icon("trash", 14),
      ),
    ),
    fields
      ? fields(trigger, vocabulary, onChange)
      : h("span", { class: "muted small", text: "This trigger was made by a newer version." }),
  );
}

function everyFields(trigger, vocabulary, onChange) {
  const unit = vocabulary.units.find((entry) => entry.unit === trigger.unit) ?? { timeOfDay: false };
  const edit = (patch) => onChange({ ...trigger, ...patch });

  const parts = [
    h("span", { class: "muted small", text: "Every" }),
    number(trigger.count, 1, 999, (value) => edit({ count: value })),
    h(
      "select",
      {
        class: "input",
        style: { width: "auto" },
        "aria-label": "Unit",
        onchange: (event) => {
          const next = event.target.value;
          const known = vocabulary.units.find((entry) => entry.unit === next);
          edit(
            {
              unit: next,
              atMinute: known?.timeOfDay ? trigger.atMinute ?? 180 : undefined,
              weekday: next === "week" ? trigger.weekday ?? 6 : undefined,
              day: next === "month" ? trigger.day ?? 1 : undefined,
            },
            event.target,
          );
        },
      },
      ...vocabulary.units.map((entry) =>
        h("option", {
          value: entry.unit,
          selected: entry.unit === trigger.unit,
          text: trigger.count === 1 ? entry.unit : `${entry.unit}s`,
        }),
      ),
    ),
  ];

  if (trigger.unit === "week") {
    parts.push(
      h("span", { class: "muted small", text: "on" }),
      h(
        "select",
        {
          class: "input",
          style: { width: "auto" },
          "aria-label": "Weekday",
          onchange: (event) => edit({ weekday: Number(event.target.value) }, event.target),
        },
        ...WEEKDAYS.map((day, index) =>
          h("option", { value: index, selected: (trigger.weekday ?? 6) === index, text: day }),
        ),
      ),
    );
  }

  if (trigger.unit === "month") {
    parts.push(
      h("span", { class: "muted small", text: "on day" }),
      number(trigger.day ?? 1, 1, 31, (value) => edit({ day: value })),
    );
  }

  if (unit.timeOfDay) {
    parts.push(
      h("span", { class: "muted small", text: "at" }),
      h("input", {
        class: "input",
        type: "time",
        style: { width: "auto" },
        value: clock(trigger.atMinute ?? 0),
        "aria-label": "Run time, UTC",
        onchange: (event) => {
          const minutes = parseClock(event.target.value);
          if (minutes === null) {
            toast("That isn't a time of day", "critical");
            return;
          }
          edit({ atMinute: minutes }, event.target);
        },
      }),
      h("span", { class: "muted small", text: timeHint(trigger.atMinute ?? 0) }),
    );
  }

  return h("div", { class: "row wrap", style: { gap: "8px" } }, ...parts);
}

function startupFields(trigger, _vocabulary, onChange) {
  return h(
    "div",
    { class: "row wrap", style: { gap: "8px" } },
    number(trigger.delayMinutes ?? 0, 0, 10080, (value) =>
      onChange({ ...trigger, delayMinutes: value }),
    ),
    h("span", { class: "muted small", text: "minutes after the server starts" }),
  );
}

function afterFields(trigger, vocabulary, onChange) {
  return h(
    "div",
    { class: "row wrap", style: { gap: "8px" } },
    h("span", { class: "muted small", text: "After" }),
    h(
      "select",
      {
        class: "input",
        style: { width: "auto" },
        "aria-label": "Task to follow",
        onchange: (event) => onChange({ ...trigger, task: event.target.value }),
      },
      ...vocabulary.tasks.map((entry) =>
        h("option", { value: entry.id, selected: entry.id === trigger.task, text: entry.title }),
      ),
    ),
    h("span", { class: "muted small", text: "finishes, wait" }),
    number(trigger.delayMinutes ?? 0, 0, 10080, (value) =>
      onChange({ ...trigger, delayMinutes: value }),
    ),
    h("span", { class: "muted small", text: "min" }),
  );
}

function eventFields(trigger, vocabulary, onChange) {
  const kinds = trigger.kinds ?? [];

  return h(
    "div",
    { class: "stack", style: { gap: "8px" } },
    h(
      "div",
      { class: "row wrap", style: { gap: "6px" } },
      ...(kinds.length
        ? kinds.map((kind) =>
            h(
              "button",
              {
                class: "btn small",
                title: "Stop listening for this",
                onclick: (event) =>
                  onChange(
                    { ...trigger, kinds: kinds.filter((existing) => existing !== kind) },
                    event.target,
                  ),
              },
              h("span", { class: "mono", text: kind }),
              icon("close", 12),
            ),
          )
        : [h("span", { class: "muted small", text: "any event at all" })]),
    ),
    h(
      "div",
      { class: "row wrap", style: { gap: "8px" } },
      h(
        "select",
        {
          class: "input",
          style: { width: "auto" },
          "aria-label": "Event family",
          onchange: (event) => {
            const kind = event.target.value;
            event.target.value = "";
            if (!kind || kinds.includes(kind)) return;
            onChange({ ...trigger, kinds: [...kinds, kind] });
          },
        },
        h("option", { value: "", selected: true, text: "Listen for…" }),
        ...vocabulary.eventKinds.map((kind) => h("option", { value: kind, text: kind })),
      ),
      h("span", { class: "muted small", text: "at most once every" }),
      number(trigger.minGapMinutes ?? 0, 0, 10080, (value) =>
        onChange({ ...trigger, minGapMinutes: value }),
      ),
      h("span", { class: "muted small", text: "min" }),
    ),
  );
}

function conditionFields(trigger, vocabulary, onChange) {
  const metric = vocabulary.metrics.find((entry) => entry.metric === trigger.metric);
  const bytes = metric?.bytes ?? false;

  return h(
    "div",
    { class: "stack", style: { gap: "8px" } },
    h(
      "div",
      { class: "row wrap", style: { gap: "8px" } },
      h("span", { class: "muted small", text: "When" }),
      h(
        "select",
        {
          class: "input",
          style: { width: "auto" },
          "aria-label": "What to watch",
          onchange: (event) => {
            const next = vocabulary.metrics.find((entry) => entry.metric === event.target.value);
            onChange(
              {
                ...trigger,
                metric: next.metric,
                comparison: next.comparison,
                value: next.bytes ? 5 * 1024 ** 3 : 10,
              },
              event.target,
            );
          },
        },
        ...vocabulary.metrics.map((entry) =>
          h("option", { value: entry.metric, selected: entry.metric === trigger.metric, text: entry.label }),
        ),
      ),
      h(
        "select",
        {
          class: "input",
          style: { width: "auto" },
          "aria-label": "Above or below",
          onchange: (event) => onChange({ ...trigger, comparison: event.target.value }),
        },
        ...["above", "below"].map((value) =>
          h("option", { value, selected: value === trigger.comparison, text: `is ${value}` }),
        ),
      ),
      ...(bytes
        ? byteValue(trigger, onChange)
        : [number(trigger.value, 0, 1_000_000_000, (value) => onChange({ ...trigger, value }))]),
    ),
    h(
      "div",
      { class: "row wrap", style: { gap: "8px" } },
      h("span", { class: "muted small", text: "checked at most once every" }),
      number(trigger.minGapMinutes ?? 0, 0, 10080, (value) =>
        onChange({ ...trigger, minGapMinutes: value }),
      ),
      h("span", { class: "muted small", text: "min" }),
      metric
        ? h("span", {
            class: "muted small",
            text: `· now: ${bytes ? fmt.bytes(metric.now) : fmt.number(metric.now)}`,
          })
        : null,
    ),
  );
}

function byteValue(trigger, onChange) {
  const gigabyte = 1024 ** 3;
  const inGigabytes = trigger.value >= gigabyte || trigger.value === 0;
  const scale = inGigabytes ? gigabyte : 1024 ** 2;
  const shown = Math.round((trigger.value / scale) * 10) / 10;

  const change = (value, unitScale) =>
    onChange({ ...trigger, value: Math.round(value * unitScale) });

  return [
    number(shown, 0, 100000, (value) => change(value, scale), { step: "0.1" }),
    h(
      "select",
      {
        class: "input",
        style: { width: "auto" },
        "aria-label": "Size unit",
        onchange: (event) => change(shown, event.target.value === "GB" ? gigabyte : 1024 ** 2),
      },
      ...["MB", "GB"].map((unit) =>
        h("option", { value: unit, selected: unit === (inGigabytes ? "GB" : "MB"), text: unit }),
      ),
    ),
  ];
}

function number(value, min, max, onCommit, { step } = {}) {
  return h("input", {
    class: "input num",
    type: "number",
    min,
    max,
    step: step ?? "1",
    value,
    style: { width: "88px" },
    onchange: (event) => {
      const parsed = Number(event.target.value);
      if (!Number.isFinite(parsed) || parsed < min || parsed > max) {
        toast(`That has to be between ${min} and ${max}`, "critical");
        event.target.value = value;
        return;
      }
      onCommit(parsed);
    },
  });
}

const WEEKDAYS = ["Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday", "Sunday"];

function logSection(task, runs, reload) {
  const body = !runs
    ? h("span", { class: "muted small", text: "Loading…" })
    : h(
        "div",
        { style: { minWidth: 0 } },
        dataTable({
          columns: [
            {
              key: "at",
              label: "When",
              render: (run) =>
                h(
                  "div",
                  { class: "stack" },
                  h("span", { style: { whiteSpace: "nowrap" }, text: fmt.relative(run.startedAt) }),
                  h("span", {
                    class: "muted small",
                    style: { whiteSpace: "nowrap" },
                    text: `${utcStamp(run.startedAt)} UTC`,
                  }),
                ),
            },
            {
              key: "status",
              label: "Outcome",
              render: (run) =>
                run.status === "error" ? pill("failed", "critical") : pill("ok", "good"),
            },
            {
              key: "trigger",
              label: "Started by",
              render: (run) => pill(STARTED_BY[run.trigger] ?? run.trigger),
            },
            { key: "took", label: "Took", align: "right", render: (run) => took(run.durationMs) },
            {
              key: "summary",
              label: "What happened",
              render: (run) => h("span", { class: "small", text: run.summary }),
            },
          ],
          rows: runs,
          expand: (run) =>
            detailPanel(
              [
                ["Started", fmt.dateTime(run.startedAt)],
                ["Finished", fmt.dateTime(run.finishedAt)],
                ["Took", took(run.durationMs)],
                ["Outcome", run.status],
                ["Started by", STARTED_BY[run.trigger] ?? run.trigger],
                run.reason ? ["Why", run.reason] : null,
                ["Summary", run.summary],
              ],
              run.detail,
            ),
          empty: emptyState(
            "Nothing has run yet",
            "The log fills in as this task runs.",
            "clock",
          ),
        }),
      );

  return section(
    "Run log",
    h(
      "div",
      { class: "stack", style: { gap: "12px", minWidth: 0 } },
      h(
        "div",
        { class: "row", style: { gap: "8px" } },
        h("span", { class: "muted small", text: "Most recent first — open a run for what it changed." }),
        h("span", { class: "spacer", style: { flex: 1 } }),
        h("button", { class: "btn small", text: "Refresh", onclick: (event) => reload(event.target) }),
      ),
      body,
    ),
  );
}

const STARTED_BY = {
  timer: "on a timer",
  startup: "server start",
  after: "another task",
  event: "an event",
  condition: "a threshold",
  manual: "by hand",
  schedule: "on a timer",
};

function section(title, body) {
  const heading = h("h3", { style: { margin: 0, fontSize: "13px" } });
  if (typeof title === "string") heading.textContent = title;
  else heading.append(title);

  return h("section", { class: "stack", style: { gap: "10px", minWidth: 0 } }, heading, body);
}

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

function timeHint(minutes) {
  if (new Date().getTimezoneOffset() === 0) return "UTC — your clock too";
  const date = new Date();
  date.setUTCHours(Math.floor(minutes / 60), minutes % 60, 0, 0);
  return `UTC — ${date.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" })} where you are`;
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
