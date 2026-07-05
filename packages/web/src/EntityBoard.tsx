// EntityBoard — /e/:slug — the generic board every user-defined entity type
// gets for free. Presentation comes from kindForType (registry icon/color
// through safe fallbacks), rows render through the same EntityList engine as
// the built-in boards, and the create/edit form is GENERATED from the type's
// field registry — text/number/bool/date/choice/ref each map to one input.
// When the type declares a board_field (a choice field), the flat list gives
// way to grouped columns in that field's option order.

import { createMemo, createResource, createSignal, For, Show, type Component } from "solid-js";
import { useParams } from "@solidjs/router";
import type { CustomEntity, EntityFieldView, EntityTypeView } from "@hive/shared";
import { api } from "./api.ts";
import { EntityList } from "./EntityList.tsx";
import { kindForType } from "./kinds.ts";
import { Icon } from "./icons.tsx";
import { EmptyState, SectionHead } from "./primitives.tsx";
import { relTime } from "./lib.tsx";
import { liveRev } from "./live.ts";

/** Option list for a ref field's target kind: [id, label] pairs. */
async function refOptions(refKind: string): Promise<[string, string][]> {
  switch (refKind) {
    case "person":
      return (await api.people()).map((p) => [p.id, p.name]);
    case "topic":
      return (await api.topics()).map((t) => [t.id, t.name]);
    case "project":
      return (await api.projects()).map((p) => [p.id, p.name]);
    case "task":
      return (await api.tasks()).map((t) => [t.id, t.title]);
    default:
      return (await api.entities(refKind)).map((e) => [e.id, e.title]);
  }
}

/** Registry-validation errors arrive as `400 {"error","issues":[...]}` inside
 * the thrown message — surface the issue lines when they parse, else raw. */
function friendlyError(e: unknown): string {
  const msg = e instanceof Error ? e.message : String(e);
  const brace = msg.indexOf("{");
  if (brace >= 0) {
    try {
      const body = JSON.parse(msg.slice(brace));
      if (Array.isArray(body.issues)) {
        return body.issues.map((i: { field: string; message: string }) => `${i.field}: ${i.message}`).join(" · ");
      }
      if (body.error) return body.error;
    } catch {
      /* raw message below */
    }
  }
  return msg;
}

/** One generated input per registry field. Values live in a plain record the
 * form owns; empty string / unchecked means "clear on save" (sent as null). */
const FieldInput: Component<{
  field: EntityFieldView;
  value: unknown;
  onChange: (v: unknown) => void;
}> = (props) => {
  const [options] = createResource(
    () => (props.field.field_type === "ref" ? props.field.ref_kind : null),
    (kind) => refOptions(kind),
  );
  return (
    <label class="sw dim sm" style={{ "align-items": "baseline", gap: "0.4rem" }}>
      {props.field.label}
      {props.field.required ? " *" : ""}
      <Show when={props.field.field_type === "text"}>
        <input value={(props.value as string) ?? ""} onInput={(e) => props.onChange(e.currentTarget.value)} />
      </Show>
      <Show when={props.field.field_type === "number"}>
        <input
          type="number"
          value={props.value == null ? "" : String(props.value)}
          onInput={(e) => props.onChange(e.currentTarget.value === "" ? null : Number(e.currentTarget.value))}
        />
      </Show>
      <Show when={props.field.field_type === "bool"}>
        <input
          type="checkbox"
          checked={props.value === true}
          onChange={(e) => props.onChange(e.currentTarget.checked)}
        />
      </Show>
      <Show when={props.field.field_type === "date"}>
        <input
          type="date"
          value={(props.value as string) ?? ""}
          onInput={(e) => props.onChange(e.currentTarget.value || null)}
        />
      </Show>
      <Show when={props.field.field_type === "choice"}>
        <select value={(props.value as string) ?? ""} onChange={(e) => props.onChange(e.currentTarget.value || null)}>
          <option value="">—</option>
          <For each={props.field.options}>{(o) => <option value={o}>{o}</option>}</For>
        </select>
      </Show>
      <Show when={props.field.field_type === "ref"}>
        <select value={(props.value as string) ?? ""} onChange={(e) => props.onChange(e.currentTarget.value || null)}>
          <option value="">—</option>
          <For each={options() ?? []}>{([id, label]) => <option value={id}>{label}</option>}</For>
        </select>
      </Show>
    </label>
  );
};

/** The generated create/edit panel. On save every live field key is sent
 * explicitly (value or null) so clears behave deterministically. */
const EntityForm: Component<{
  type: EntityTypeView;
  existing: CustomEntity | null;
  onDone: () => void;
  onCancel: () => void;
}> = (props) => {
  const live = () => props.type.fields.filter((f) => !f.archived);
  const [title, setTitle] = createSignal(props.existing?.title ?? "");
  const [scope, setScope] = createSignal<"global" | "me">(props.existing?.user_scope ? "me" : "global");
  const [values, setValues] = createSignal<Record<string, unknown>>({
    ...(props.existing?.fields ?? {}),
  });
  const [busy, setBusy] = createSignal(false);
  const [err, setErr] = createSignal<string | null>(null);

  const save = async (e: Event) => {
    e.preventDefault();
    setBusy(true);
    setErr(null);
    const fields: Record<string, unknown> = {};
    for (const f of live()) {
      const v = values()[f.slug];
      fields[f.slug] = v === undefined || v === "" ? null : v;
    }
    try {
      if (props.existing) {
        await api.patchEntity(props.existing.id, { title: title().trim(), fields, scope: scope() });
      } else {
        await api.createEntity({ type: props.type.slug, title: title().trim(), fields, scope: scope() });
      }
      props.onDone();
    } catch (e2) {
      setErr(friendlyError(e2));
    } finally {
      setBusy(false);
    }
  };

  const remove = async () => {
    if (!props.existing) return;
    setBusy(true);
    setErr(null);
    try {
      await api.deleteEntity(props.existing.id);
      props.onDone();
    } catch (e2) {
      setErr(friendlyError(e2));
    } finally {
      setBusy(false);
    }
  };

  return (
    <form class="composer" onSubmit={save}>
      <div class="source-form">
        <input
          class="grow"
          placeholder={`${props.type.name} title`}
          value={title()}
          onInput={(e) => setTitle(e.currentTarget.value)}
        />
        <select value={scope()} onChange={(e) => setScope(e.currentTarget.value as "global" | "me")}>
          <option value="global">everyone</option>
          <option value="me">just me</option>
        </select>
      </div>
      <div class="source-form" style={{ "flex-wrap": "wrap", "margin-top": "0.6rem" }}>
        <For each={live()}>
          {(f) => (
            <FieldInput
              field={f}
              value={values()[f.slug]}
              onChange={(v) => setValues({ ...values(), [f.slug]: v })}
            />
          )}
        </For>
      </div>
      <div class="source-form" style={{ "margin-top": "0.6rem" }}>
        <button class="primary" type="submit" disabled={busy() || !title().trim()}>
          {props.existing ? "save" : `add ${props.type.name.toLowerCase()}`}
        </button>
        <button type="button" class="ghost" onClick={props.onCancel}>cancel</button>
        <Show when={props.existing}>
          <button type="button" class="ghost danger" onClick={remove} disabled={busy()}>delete</button>
        </Show>
      </div>
      <Show when={err()}>
        <p class="dim sm" style={{ color: "var(--danger)" }}>{err()}</p>
      </Show>
    </form>
  );
};

/** Compact field summary for a row: first few non-text values as "label value".
 * `skip` drops fields already shown elsewhere (the badge, the board column). */
function fieldMetas(t: EntityTypeView, e: CustomEntity, skip?: string | null): string[] {
  const out: string[] = [];
  for (const f of t.fields) {
    if (f.archived || f.slug === t.board_field || f.slug === skip) continue;
    const v = e.fields[f.slug];
    if (v == null || v === "") continue;
    if (f.field_type === "bool") {
      if (v === true) out.push(f.label.toLowerCase());
      continue;
    }
    if (f.field_type === "ref") continue; // ids aren't readable metas
    out.push(`${f.label.toLowerCase()} ${v}`);
    if (out.length >= 3) break;
  }
  return out;
}

export const EntityBoard: Component = () => {
  const params = useParams();
  const [types] = createResource(
    () => ({ _r: liveRev(), slug: params.slug }),
    () => api.entityTypes(true),
  );
  const ty = createMemo(() => (types.latest ?? []).find((t) => t.slug === params.slug));
  const [items] = createResource(
    () => ({ _r: liveRev(), slug: params.slug }),
    (k) => api.entities(k.slug),
  );
  const [editing, setEditing] = createSignal<CustomEntity | "new" | null>(null);

  const kind = createMemo(() => (ty() ? kindForType(ty()!) : null));
  const boardField = createMemo(() => {
    const t = ty();
    if (!t?.board_field) return null;
    return t.fields.find((f) => f.slug === t.board_field && !f.archived) ?? null;
  });

  // board_field grouping: option order, then a "—" column for unset rows.
  const columns = createMemo(() => {
    const bf = boardField();
    if (!bf) return null;
    const list = items.latest ?? [];
    const cols = bf.options.map((o) => ({
      name: o,
      items: list.filter((e) => e.fields[bf.slug] === o),
    }));
    const none = list.filter((e) => {
      const v = e.fields[bf.slug];
      return v == null || v === "";
    });
    if (none.length) cols.push({ name: "—", items: none });
    return cols;
  });

  return (
    <Show when={ty()} fallback={<EmptyState icon="hex" title="No such collection." hint="Pick a board from ⌘K, or define this type in Admin." />}>
      {(t) => (
        <section>
          <SectionHead title={t().name_plural} icon={kind()!.icon} count={(items.latest ?? []).length}>
            <button class="ghost" onClick={() => setEditing(editing() === "new" ? null : "new")}>
              {editing() === "new" ? "cancel" : `+ new ${t().name.toLowerCase()}`}
            </button>
          </SectionHead>
          <Show when={t().description}>
            <p class="dim sm">{t().description}</p>
          </Show>
          <Show when={t().archived}>
            <p class="dim sm">This type is archived — existing records stay readable, new ones can't be added.</p>
          </Show>

          <Show when={editing() === "new"}>
            <EntityForm type={t()} existing={null} onDone={() => setEditing(null)} onCancel={() => setEditing(null)} />
          </Show>
          <Show when={(() => { const e = editing(); return e !== "new" ? e : null; })()} keyed>
            {(e) => (
              <EntityForm
                type={t()}
                existing={e}
                onDone={() => setEditing(null)}
                onCancel={() => setEditing(null)}
              />
            )}
          </Show>

          <Show
            when={columns()}
            fallback={
              <EntityList
                config={{
                  kind: kind()!,
                  fetch: () => api.entities(params.slug),
                  row: {
                    title: (e: CustomEntity) => e.title,
                    badges: (e) => {
                      const bf = t().fields.find((f) => f.field_type === "choice" && !f.archived);
                      const v = bf ? e.fields[bf.slug] : null;
                      return typeof v === "string" && v ? [{ label: v }] : [];
                    },
                    metas: (e) => {
                      const badged = t().fields.find((f) => f.field_type === "choice" && !f.archived);
                      return [...fieldMetas(t(), e, badged?.slug), relTime(e.updated_at)];
                    },
                    onClick: (e) => setEditing(e),
                  },
                }}
              />
            }
          >
            {(cols) => (
              <div class="board">
                <For each={cols()}>
                  {(col) => (
                    <div class="col">
                      <h4>
                        {col.name} <span class="dim">{col.items.length}</span>
                      </h4>
                      <For each={col.items}>
                        {(e) => (
                          <div class="card" onClick={() => setEditing(e)}>
                            <div>{e.title}</div>
                            <div class="dim sm">{fieldMetas(t(), e).join(" · ")}</div>
                          </div>
                        )}
                      </For>
                    </div>
                  )}
                </For>
              </div>
            )}
          </Show>

          <Show when={!(items.latest ?? []).length && columns()}>
            <EmptyState icon={kind()!.icon} title={kind()!.empty.title} hint={kind()!.empty.hint} />
          </Show>

          <p class="dim sm" style={{ "margin-top": "1rem" }}>
            <Icon name="search" size={12} /> Findable in keyword search; semantic recall for custom kinds arrives later.
          </p>
        </section>
      )}
    </Show>
  );
};
