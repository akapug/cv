//! Operation-owned, bounded temporary storage for ordered records and exact string membership.
mod exact;
mod ordered;
pub(crate) use exact::ExactStrings;
pub(crate) use ordered::{Ordered, Record};
