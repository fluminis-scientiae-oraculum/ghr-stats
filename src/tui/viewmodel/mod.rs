//! The ViewModel: everything a view needs, already derived from the Model
//! (`App`) — typed statuses, styling, and copy. Views become pure renderers, and
//! each presentation rule lives in exactly one place.
//!
//! The derivation functions are pure over primitive inputs (not `&App`), so they
//! are unit-tested once and cannot silently diverge across views — the antidote
//! to the "fix here, forgot there" bugs that motivated this layer. Views extract
//! the primitives from `App` (via small `App` accessors) and render the result.

pub(crate) mod copy;
pub(crate) mod status;
pub(crate) mod style;
