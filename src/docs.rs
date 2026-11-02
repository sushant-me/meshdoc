//! Typed CRDT documents.
//!
//! Documents are `yrs` (Yjs) documents. We do not implement a CRDT; we project
//! four typed views onto a CRDT that already has convergence guarantees:
//!
//! | Type | Structure |
//! |---|---|
//! | Checklist   | `data.items` map: item_id -> {text, done, doneBy, doneAt, pos} |
//! | SupplyForm  | `data.fields` map: field_id -> {value, unit} |
//! | GeoPoint    | `data`: lat, lon, label, kind |
//! | Note        | `data.text`: a YText |
//!
//! Items are keyed by a random id rather than an array index, so two people adding
//! rows at the same time cannot fight over an index. Ordering is by a fractional
//! `pos`, so an insert between two rows never renumbers anything.
//!
//! Every document carries a `schema` version in `meta`. Migration is a pure
//! function of the document, so replicas running different app versions converge
//! after migration instead of forking.

use serde::{Deserialize, Serialize};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{Doc, Map, MapRef, Out, ReadTxn, Transact, Update};

use crate::crypto::PublicIdentity;
use crate::ids::DocId;

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum DocType {
    Checklist,
    SupplyForm,
    GeoPoint,
    Note,
}

impl DocType {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocType::Checklist => "checklist",
            DocType::SupplyForm => "supply_form",
            DocType::GeoPoint => "geo_point",
            DocType::Note => "note",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "checklist" => Some(DocType::Checklist),
            "supply_form" => Some(DocType::SupplyForm),
            "geo_point" => Some(DocType::GeoPoint),
            "note" => Some(DocType::Note),
            _ => None,
        }
    }
    pub fn all() -> [DocType; 4] {
        [DocType::Checklist, DocType::SupplyForm, DocType::GeoPoint, DocType::Note]
    }
}

impl std::fmt::Display for DocType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChecklistItem {
    pub id: String,
    pub text: String,
    pub done: bool,
    pub done_by: Option<String>,
    pub done_at: Option<i64>,
    pub pos: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SupplyField {
    pub id: String,
    pub value: String,
    pub unit: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct GeoPointValue {
    pub lat: f64,
    pub lon: f64,
    pub label: String,
    pub kind: String,
}

/// A typed view over a CRDT document.
pub struct Document {
    id: DocId,
    doc: Doc,
}

impl Document {
    /// Create a new, empty document of the given type.
    pub fn create(id: DocId, ty: DocType, author: &PublicIdentity, title: &str) -> Self {
        let doc = Doc::new();
        // IMPORTANT: every get_or_insert_* call must happen *before* a transaction is
        // opened. Calling one while a write transaction is live deadlocks yrs.
        let meta = doc.get_or_insert_map("meta");
        let data = doc.get_or_insert_map("data");
        {
            let mut txn = doc.transact_mut();
            meta.insert(&mut txn, "type", ty.as_str());
            meta.insert(&mut txn, "schema", SCHEMA_VERSION as f64);
            meta.insert(&mut txn, "createdBy", author.to_hex());
            meta.insert(&mut txn, "title", title);
            // Ensure the data root has the shape this document type expects.
            match ty {
                DocType::Checklist => {
                    data.insert(&mut txn, "items", yrs::MapPrelim::default());
                }
                DocType::SupplyForm => {
                    data.insert(&mut txn, "fields", yrs::MapPrelim::default());
                }
                DocType::GeoPoint => {
                    data.insert(&mut txn, "lat", 0.0f64);
                    data.insert(&mut txn, "lon", 0.0f64);
                    data.insert(&mut txn, "label", "");
                    data.insert(&mut txn, "kind", "landmark");
                }
                DocType::Note => {
                    // A note body is a last-writer-wins string register, not a text
                    // CRDT: "replace the body" is the semantic, and an LWW register
                    // converges deterministically for it. See the note in `migrate`.
                    data.insert(&mut txn, "body", "");
                }
            }
        }
        Self { id, doc }
    }

    /// Wrap an existing CRDT document (e.g. one loaded from the store).
    pub fn from_doc(id: DocId, doc: Doc) -> Self { Self { id, doc } }

    pub fn id(&self) -> DocId { self.id }

    pub fn doc(&self) -> &Doc { &self.doc }

    pub fn into_doc(self) -> Doc { self.doc }

    pub fn ty(&self) -> DocType {
        let meta = self.doc.get_or_insert_map("meta");
        let txn = self.doc.transact();
        meta.get_as::<_, String>(&txn, "type")
            .ok()
            .and_then(|s| DocType::parse(&s))
            .unwrap_or(DocType::Note)
    }

    pub fn schema(&self) -> u32 {
        let meta = self.doc.get_or_insert_map("meta");
        let txn = self.doc.transact();
        meta.get_as::<_, u32>(&txn, "schema").unwrap_or(0)
    }

    pub fn title(&self) -> String {
        let meta = self.doc.get_or_insert_map("meta");
        let txn = self.doc.transact();
        meta.get_as::<_, String>(&txn, "title").unwrap_or_default()
    }

    pub fn set_title(&self, t: &str) {
        let meta = self.doc.get_or_insert_map("meta");
        let mut txn = self.doc.transact_mut();
        meta.insert(&mut txn, "title", t);
    }

    // ---------------------------------------------------------------- checklist

    fn items_map_read(&self) -> Option<MapRef> {
        let data = self.doc.get_or_insert_map("data");
        let txn = self.doc.transact();
        match data.get(&txn, "items") {
            Some(Out::YMap(m)) => Some(m),
            _ => None,
        }
    }

    /// Append an item to the end of the checklist. Returns its id.
    pub fn add_item(&self, text: &str) -> String {
        let pos = self.iter_items().last().map(|i| i.pos + 1.0).unwrap_or(0.0);
        self.add_item_at(text, pos)
    }

    /// Insert an item at a given ordering position.
    pub fn add_item_at(&self, text: &str, pos: f64) -> String {
        let id = new_item_id();
        let data = self.doc.get_or_insert_map("data");
        let mut txn = self.doc.transact_mut();
        // Lazily materialise the container, in the same transaction that writes to
        // it, so a local edit on a doc whose container is absent still works.
        let items = match data.get(&txn, "items") {
            Some(Out::YMap(m)) => m,
            _ => data.insert(&mut txn, "items", yrs::MapPrelim::default()),
        };
        let m = items.insert(&mut txn, id.clone(), yrs::MapPrelim::default());
        m.insert(&mut txn, "text", text);
        m.insert(&mut txn, "done", false);
        m.insert(&mut txn, "pos", pos);
        id
    }

    pub fn set_item_done(&self, item_id: &str, done: bool, by: &PublicIdentity) -> bool {
        let now = now_unix();
        let Some(items) = self.items_map_read() else { return false };
        let mut txn = self.doc.transact_mut();
        let Some(Out::YMap(m)) = items.get(&txn, item_id) else { return false };
        m.insert(&mut txn, "done", done);
        if done {
            m.insert(&mut txn, "doneBy", by.to_hex());
            m.insert(&mut txn, "doneAt", now);
        } else {
            m.remove(&mut txn, "doneBy");
            m.remove(&mut txn, "doneAt");
        }
        true
    }

    pub fn remove_item(&self, item_id: &str) -> bool {
        let Some(items) = self.items_map_read() else { return false };
        let mut txn = self.doc.transact_mut();
        items.remove(&mut txn, item_id).is_some()
    }

    pub fn iter_items(&self) -> Vec<ChecklistItem> {
        let Some(items) = self.items_map_read() else { return vec![] };
        let txn = self.doc.transact();
        let mut out = vec![];
        for (id, val) in items.iter(&txn) {
            let Out::YMap(m) = val else { continue };
            let text = m.get_as::<_, String>(&txn, "text").unwrap_or_default();
            let done = m.get_as::<_, bool>(&txn, "done").unwrap_or(false);
            let done_by = m.get_as::<_, String>(&txn, "doneBy").ok();
            let done_at = m.get_as::<_, i64>(&txn, "doneAt").ok();
            let pos = m.get_as::<_, f64>(&txn, "pos").unwrap_or(0.0);
            out.push(ChecklistItem { id: id.to_string(), text, done, done_by, done_at, pos });
        }
        // Convergent ordering. Two replicas can append concurrently and produce the
        // same `pos`, so the id is a deterministic tie-break: every replica then
        // agrees on the order even when the positions collide.
        out.sort_by(|a, b| {
            a.pos
                .partial_cmp(&b.pos)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.id.cmp(&b.id))
        });
        out
    }

    /// (done, total)
    pub fn checklist_progress(&self) -> (usize, usize) {
        let items = self.iter_items();
        (items.iter().filter(|i| i.done).count(), items.len())
    }

    // -------------------------------------------------------------- supply form

    fn fields_map_read(&self) -> Option<MapRef> {
        let data = self.doc.get_or_insert_map("data");
        let txn = self.doc.transact();
        match data.get(&txn, "fields") {
            Some(Out::YMap(m)) => Some(m),
            _ => None,
        }
    }

    pub fn set_field(&self, field: &str, value: &str, unit: &str) -> bool {
        let data = self.doc.get_or_insert_map("data");
        let mut txn = self.doc.transact_mut();
        let fields = match data.get(&txn, "fields") {
            Some(Out::YMap(m)) => m,
            _ => data.insert(&mut txn, "fields", yrs::MapPrelim::default()),
        };
        match fields.get(&txn, field) {
            Some(Out::YMap(m)) => {
                m.insert(&mut txn, "value", value);
                m.insert(&mut txn, "unit", unit);
            }
            _ => {
                let m = fields.insert(&mut txn, field.to_string(), yrs::MapPrelim::default());
                m.insert(&mut txn, "value", value);
                m.insert(&mut txn, "unit", unit);
            }
        }
        true
    }

    pub fn get_field(&self, field: &str) -> Option<SupplyField> {
        let fields = self.fields_map_read()?;
        let txn = self.doc.transact();
        match fields.get(&txn, field) {
            Some(Out::YMap(m)) => Some(SupplyField {
                id: field.to_string(),
                value: m.get_as::<_, String>(&txn, "value").unwrap_or_default(),
                unit: m.get_as::<_, String>(&txn, "unit").unwrap_or_default(),
            }),
            _ => None,
        }
    }

    pub fn iter_fields(&self) -> Vec<SupplyField> {
        let Some(fields) = self.fields_map_read() else { return vec![] };
        let txn = self.doc.transact();
        let mut out = vec![];
        for (id, val) in fields.iter(&txn) {
            let Out::YMap(m) = val else { continue };
            out.push(SupplyField {
                id: id.to_string(),
                value: m.get_as::<_, String>(&txn, "value").unwrap_or_default(),
                unit: m.get_as::<_, String>(&txn, "unit").unwrap_or_default(),
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    // ----------------------------------------------------------------- geopoint

    pub fn set_location(&self, lat: f64, lon: f64, label: &str, kind: &str) {
        let data = self.doc.get_or_insert_map("data");
        let mut txn = self.doc.transact_mut();
        data.insert(&mut txn, "lat", lat);
        data.insert(&mut txn, "lon", lon);
        data.insert(&mut txn, "label", label);
        data.insert(&mut txn, "kind", kind);
    }

    pub fn location(&self) -> GeoPointValue {
        let data = self.doc.get_or_insert_map("data");
        let txn = self.doc.transact();
        GeoPointValue {
            lat: data.get_as::<_, f64>(&txn, "lat").unwrap_or(0.0),
            lon: data.get_as::<_, f64>(&txn, "lon").unwrap_or(0.0),
            label: data.get_as::<_, String>(&txn, "label").unwrap_or_default(),
            kind: data.get_as::<_, String>(&txn, "kind").unwrap_or_default(),
        }
    }

    // --------------------------------------------------------------------- note

    pub fn set_text(&self, s: &str) {
        let data = self.doc.get_or_insert_map("data");
        let mut txn = self.doc.transact_mut();
        data.insert(&mut txn, "body", s);
    }

    pub fn append_text(&self, s: &str) {
        let current = self.text();
        self.set_text(&format!("{current}{s}"));
    }

    pub fn text(&self) -> String {
        let data = self.doc.get_or_insert_map("data");
        let txn = self.doc.transact();
        data.get_as::<_, String>(&txn, "body").unwrap_or_default()
    }

    // -------------------------------------------------------------- CRDT plumbing

    /// Encode the full state as a CRDT update.
    pub fn encode_state(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.encode_state_as_update_v1(&yrs::StateVector::default())
    }

    /// Encode only what `remote` is missing. This is the anti-entropy primitive:
    /// the sender computes it from the receiver's state vector, so we never send
    /// what the peer already has.
    pub fn encode_diff(&self, remote_sv: &[u8]) -> Result<Vec<u8>, DocError> {
        let sv = yrs::StateVector::decode_v1(remote_sv).map_err(|_| DocError::BadStateVector)?;
        let txn = self.doc.transact();
        Ok(txn.encode_state_as_update_v1(&sv))
    }

    /// Hex of the CRDT state vector. Two converged replicas must be identical;
    /// used by the diagnostics in the convergence tests.
    pub fn state_vector_hex(&self) -> String {
        use crate::ids::hex;
        hex(&self.state_vector())
    }

    pub fn state_vector(&self) -> Vec<u8> {
        let txn = self.doc.transact();
        txn.state_vector().encode_v1()
    }

    /// Apply an update produced by another replica.
    pub fn apply_update(&self, update: &[u8]) -> Result<(), DocError> {
        let u = Update::decode_v1(update).map_err(|_| DocError::BadUpdate)?;
        let mut txn = self.doc.transact_mut();
        txn.apply_update(u).map_err(|_| DocError::Apply)?;
        Ok(())
    }

    // ------------------------------------------------------------------ migration

    /// Migrate to [`SCHEMA_VERSION`].
    ///
    /// This only records the schema version; it deliberately does **not** create
    /// containers. Containers belong to the document's origin, which creates them
    /// once in [`Document::create`]. A replica that created its own container while
    /// simultaneously receiving the origin's would end up with two competing Yjs
    /// objects under the same key, and which one is visible would depend on client-id
    /// ordering — i.e. convergence would become a coin flip. Lazy creation in the
    /// mutators below covers the (rare) case of a local edit on a document whose
    /// container is genuinely absent.
    pub fn migrate(&self) -> u32 {
        let from = self.schema();
        if from >= SCHEMA_VERSION { return from; }
        let meta = self.doc.get_or_insert_map("meta");
        let mut txn = self.doc.transact_mut();
        meta.insert(&mut txn, "schema", SCHEMA_VERSION as f64);
        from
    }

    /// Deterministic plain-text rendering, used by the CLI and by tests as a
    /// convergence oracle.
    pub fn render(&self) -> String {
        let mut s = String::new();
        s.push_str(&format!("[{}] {}\n", self.ty().as_str(), self.title()));
        match self.ty() {
            DocType::Checklist => {
                let (done, total) = self.checklist_progress();
                s.push_str(&format!("  progress: {done}/{total}\n"));
                for it in self.iter_items() {
                    s.push_str(&format!("  [{}] {} ({})\n", if it.done { "x" } else { " " }, it.text, &it.id[..8]));
                }
            }
            DocType::SupplyForm => {
                for f in self.iter_fields() {
                    s.push_str(&format!("  {} = {} {}\n", f.id, f.value, f.unit));
                }
            }
            DocType::GeoPoint => {
                let g = self.location();
                s.push_str(&format!("  ({:.5}, {:.5}) {} [{}]\n", g.lat, g.lon, g.label, g.kind));
            }
            DocType::Note => {
                s.push_str(&format!("  {}\n", self.text()));
            }
        }
        s
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum DocError {
    BadStateVector,
    BadUpdate,
    Apply,
    BadType,
}

impl std::fmt::Display for DocError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocError::BadStateVector => write!(f, "malformed state vector"),
            DocError::BadUpdate => write!(f, "malformed CRDT update"),
            DocError::Apply => write!(f, "CRDT rejected the update"),
            DocError::BadType => write!(f, "unknown document type"),
        }
    }
}
impl std::error::Error for DocError {}

fn new_item_id() -> String {
    use rand::RngCore;
    let mut b = [0u8; 6];
    rand::rngs::OsRng.fill_bytes(&mut b);
    let mut s = String::with_capacity(12);
    const T: &[u8; 16] = b"0123456789abcdef";
    for x in b { s.push(T[(x >> 4) as usize] as char); s.push(T[(x & 0xf) as usize] as char); }
    s
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::Identity;

    fn id() -> PublicIdentity { Identity::generate().public() }

    #[test]
    fn checklist_add_and_toggle() {
        let d = Document::create(DocId::random(), DocType::Checklist, &id(), "Supplies");
        let a = d.add_item("water");
        d.add_item("blankets");
        assert_eq!(d.checklist_progress(), (0, 2));
        assert!(d.set_item_done(&a, true, &id()));
        assert_eq!(d.checklist_progress(), (1, 2));
        let items = d.iter_items();
        assert_eq!(items[0].text, "water");
        assert!(items[0].done);
        assert!(items[0].done_by.is_some());
    }

    #[test]
    fn two_replicas_converge_on_independent_edits() {
        let did = DocId::random();
        let d1 = Document::create(did, DocType::Checklist, &id(), "Shared");
        let a = d1.add_item("water");
        // Replica 2 starts from replica 1's full state.
        let d2 = Document::from_doc(did, Doc::new());
        d2.apply_update(&d1.encode_state()).unwrap();
        assert_eq!(d2.iter_items().len(), 1);

        // Independent concurrent edits on each replica.
        d1.add_item("blankets");
        assert!(d2.set_item_done(&a, true, &id()));
        d2.add_item("radio");

        // Exchange diffs both ways using state vectors (anti-entropy).
        let sv2 = d2.state_vector();
        let sv1 = d1.state_vector();
        let to2 = d1.encode_diff(&sv2).unwrap();
        let to1 = d2.encode_diff(&sv1).unwrap();
        d2.apply_update(&to2).unwrap();
        d1.apply_update(&to1).unwrap();

        // Both converge to the same rendered state.
        assert_eq!(d1.render(), d2.render());
        assert_eq!(d1.iter_items().len(), 3);
        assert_eq!(d1.checklist_progress(), (1, 3));
    }

    #[test]
    fn applying_same_update_twice_is_idempotent() {
        let did = DocId::random();
        let d1 = Document::create(did, DocType::Note, &id(), "n");
        d1.set_text("hello");
        let full = d1.encode_state();
        let d2 = Document::from_doc(did, Doc::new());
        d2.apply_update(&full).unwrap();
        d2.apply_update(&full).unwrap();
        assert_eq!(d2.text(), "hello");
        assert_eq!(d1.render(), d2.render());
    }

    #[test]
    fn supply_form_fields() {
        let d = Document::create(DocId::random(), DocType::SupplyForm, &id(), "Needs");
        d.set_field("rice", "25", "kg");
        d.set_field("rice", "30", "kg");
        assert_eq!(d.get_field("rice").unwrap().value, "30");
        assert_eq!(d.iter_fields().len(), 1);
    }

    #[test]
    fn geopoint_roundtrip() {
        let d = Document::create(DocId::random(), DocType::GeoPoint, &id(), "Clinic");
        d.set_location(27.7172, 85.3240, "Kathmandu clinic", "medical");
        let g = d.location();
        assert!((g.lat - 27.7172).abs() < 1e-9);
        assert_eq!(g.kind, "medical");
    }

    #[test]
    fn concurrent_note_writes_converge_to_one_value() {
        // A note body is a last-writer-wins register, so two concurrent whole-body
        // writes converge to exactly one of the two values — deterministically the
        // same one on every replica.
        let did = DocId::random();
        let d1 = Document::create(did, DocType::Note, &id(), "note");
        d1.set_text("base");
        let base = d1.encode_state();

        let d2 = Document::from_doc(did, yrs::Doc::new());
        d2.apply_update(&base).unwrap();
        d1.set_text("from A");
        d2.set_text("from B");
        let u1 = d1.encode_state();
        let u2 = d2.encode_state();
        d1.apply_update(&u2).unwrap();
        d2.apply_update(&u1).unwrap();

        assert_eq!(d1.text(), d2.text(), "both replicas must agree");
        assert!(
            d1.text() == "from A" || d1.text() == "from B",
            "the winner must be one of the concurrent writes, got {:?}",
            d1.text()
        );
    }

    #[test]
    fn note_append_is_visible_to_peers() {
        let did = DocId::random();
        let d1 = Document::create(did, DocType::Note, &id(), "n");
        d1.set_text("a");
        d1.append_text("b");
        let d2 = Document::from_doc(did, yrs::Doc::new());
        d2.apply_update(&d1.encode_state()).unwrap();
        assert_eq!(d2.text(), "ab");
    }

    #[test]
    fn update_arriving_before_its_dependency_still_converges() {
        // C0 = creation, C1 = a write that depends on C0.
        let did = DocId::random();
        let origin = Document::create(did, DocType::Note, &id(), "n");
        let c0 = origin.encode_state();

        let writer = Document::from_doc(did, yrs::Doc::new());
        writer.apply_update(&c0).unwrap();
        let sv_before = writer.state_vector();
        writer.set_text("revision 27");
        let c1 = writer.encode_diff(&sv_before).unwrap();

        // Replica that sees C1 first, then C0.
        let late = Document::from_doc(did, yrs::Doc::new());
        late.apply_update(&c1).unwrap();
        late.apply_update(&c0).unwrap();

        // Replica that sees them in order.
        let eager = Document::from_doc(did, yrs::Doc::new());
        eager.apply_update(&c0).unwrap();
        eager.apply_update(&c1).unwrap();

        assert_eq!(late.text(), "revision 27", "out-of-order dependency must resolve");
        assert_eq!(eager.text(), "revision 27");
        assert_eq!(late.text(), eager.text());
    }

    #[test]
    fn schema_migration_is_idempotent() {
        let d = Document::create(DocId::random(), DocType::Checklist, &id(), "x");
        assert_eq!(d.migrate(), SCHEMA_VERSION); // already current, no-op
        assert_eq!(d.schema(), SCHEMA_VERSION);
    }

    #[test]
    fn ordering_is_stable_across_merge() {
        let did = DocId::random();
        let d1 = Document::create(did, DocType::Checklist, &id(), "order");
        d1.add_item("first");
        let d2 = Document::from_doc(did, Doc::new());
        d2.apply_update(&d1.encode_state()).unwrap();
        d1.add_item("second");
        d1.add_item("third");
        let sv = d2.state_vector();
        d2.apply_update(&d1.encode_diff(&sv).unwrap()).unwrap();
        let texts: Vec<String> = d2.iter_items().into_iter().map(|i| i.text).collect();
        assert_eq!(texts, vec!["first", "second", "third"]);
    }
}
