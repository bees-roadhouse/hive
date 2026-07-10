/// Per-user namespace visibility: admins see everything; everyone else sees
/// global (NULL-scoped) entries plus their own namespace (plus explicit
/// shares/@mentions, applied separately).
#[derive(Clone, Debug)]
pub enum Visibility {
    All,
    Namespace(String),
}
