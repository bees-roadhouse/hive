// hive-core: the store layer (Postgres store, schema/migrations, pgq query
// helpers) extracted from the api crate so worker/mail — and later the desktop
// shell — depend on the data layer without the HTTP surface.

pub mod acting;
pub mod auth;
pub mod db;
pub mod pgq;
pub mod store;

pub use acting::ActingScope;

mod visibility;
pub use visibility::Visibility;
