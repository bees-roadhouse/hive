// Contact cards (Phase 3, slice 1). A contact card is the canonical rich
// person record: a CUSTOM ENTITY of a built-in `contact` entity type. It
// reuses the entity_types + entities machinery wholesale — there is NO new
// table and NO fold/schema change. The `contact` type is an ordinary custom
// entity type (slug "contact", which is deliberately NOT on the reserved
// list) seeded idempotently; instances are ordinary custom entities with
// `type = "contact"`.
//
// Two seams live here:
//   * `ensure_contact_type()` — idempotent seed of the type + its standard
//     fields, callable from the app on first Contacts-pane use.
//   * `contact_type_ensure_payloads()` — the SYNC, Core-level planner that
//     both the seed and the journal `[contact:]` emergence share, so the
//     type is materialised the same way from either entry point (and, in
//     emergence, in the same op-log batch as the entry that named it).
//
// slice 2: identities link to a contact card via a Ref field — leave the
// seam. An identity (people row / actor) will point at its card here.

use anyhow::Result;
use hive_shared::EntityTypeView;
use serde_json::{json, Value};

use super::{new_id, now_iso, to_json, Core, Draft, Store};

/// The built-in contact entity type's slug. It is the `kind` string on every
/// contact instance's records (search/links rows), so it is immutable.
pub const CONTACT_TYPE_SLUG: &str = "contact";

/// The standard fields every contact card carries, in display order:
/// `(slug, label, field_type)`. All are optional; the entity's `title` holds
/// the primary display name (full/preferred name). Users extend the type with
/// their OWN fields via `entity_types_update` (see the detail view). `birthday`
/// is a Date the UI reads to show a calculated age; `notes` is the manual
/// notes section (multiline in the UI).
const CONTACT_FIELDS: &[(&str, &str, &str)] = &[
    ("birthday", "Birthday", "date"),
    ("birth_name", "Birth name", "text"),
    ("nickname", "Nickname", "text"),
    ("additional_names", "Additional names", "text"),
    ("email", "Email", "text"),
    ("phone", "Phone", "text"),
    ("address", "Address", "text"),
    ("organization", "Organization", "text"),
    ("title", "Title", "text"),
    ("notes", "Notes", "text"),
];

impl Store {
    /// Ensure the `contact` entity type exists with its standard fields, and
    /// return its view. Idempotent: if the type is already present (created by
    /// a prior call, a user, or the importer) it is returned untouched — the
    /// standard fields are NEVER duplicated. Safe to call on every Contacts
    /// pane mount.
    pub async fn ensure_contact_type(&self) -> Result<EntityTypeView> {
        self.run(|core| {
            if let Some(view) = super::entity_types::entity_type_get(core, CONTACT_TYPE_SLUG)? {
                return Ok(view);
            }
            let (type_id, payloads) = contact_type_ensure_payloads(core)?;
            // payloads is non-empty here (the type was absent); commit each as
            // its own entity.create record, exactly like entity_types_create.
            let ts = now_iso();
            let batch: Vec<Draft> = payloads
                .iter()
                .map(|p| Draft::new(crate::oplog::kind::ENTITY_CREATE, "system", &ts, p.clone()))
                .collect();
            core.commit(batch)?;
            let view = super::entity_types::entity_type_get(core, &type_id)?
                .expect("contact type just seeded");
            Ok(view)
        })
        .await
    }
}

/// Sync, Core-level planner shared by the seed and journal emergence: return
/// the contact type's `type_id` plus the entity.create PAYLOADS (each a
/// `{kind, id, fields}` map) needed to bring it into existence. When the type
/// already exists the payload list is EMPTY and the existing id is returned —
/// so callers append nothing. The payloads are ordered type-then-fields so a
/// consumer that applies them in order (the journal.append `emerged` array,
/// or a record batch) always creates the type row before its field rows.
pub(crate) fn contact_type_ensure_payloads(core: &Core) -> Result<(String, Vec<Value>)> {
    if let Some(view) = super::entity_types::entity_type_get(core, CONTACT_TYPE_SLUG)? {
        return Ok((view.id, Vec::new()));
    }
    let ts = now_iso();
    let type_id = new_id("etype");
    let mut payloads = Vec::with_capacity(CONTACT_FIELDS.len() + 1);
    payloads.push(json!({"kind": "entity_type", "id": type_id, "fields": {
        "slug": CONTACT_TYPE_SLUG, "name": "Contact", "name_plural": "Contacts",
        "description": "People you know — the canonical record of who someone is.",
        "icon": "", "color": "", "board_field": null, "archived": false,
        "created_by": "system", "created_at": ts, "updated_at": ts,
    }}));
    for (i, (slug, label, field_type)) in CONTACT_FIELDS.iter().enumerate() {
        payloads.push(
            json!({"kind": "entity_field", "id": new_id("efield"), "fields": {
                "type_id": type_id, "slug": slug, "label": label,
                "field_type": field_type, "required": false, "position": i as i64,
                "options": to_json(&Vec::<String>::new()), "ref_kind": null,
                "archived": false, "created_at": ts, "updated_at": ts,
            }}),
        );
    }
    Ok((type_id, payloads))
}
