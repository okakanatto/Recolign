//! Recolign engine: everything about reading and comparing the files.
//!
//! Pipeline: read-only snapshot → encoding check → strict index of the
//! delimited (CSV, TSV, …) or fixed-width text with original positions → column mapping → key validation → matching →
//! classification and changed columns → optional pattern aggregation →
//! views that return only the requested range.

pub mod cancel;
pub mod columns;
pub mod compare;
pub mod encoding;
pub mod error;
pub mod export;
pub mod fixed;
pub mod groups;
pub mod pattern;
pub mod pipeline;
pub mod preview;
pub mod source;
pub mod sysmem;
pub mod table;
pub mod tokenizer;
pub mod view;

pub use cancel::{CancelToken, Progress, SideProgress, Stage};
pub use columns::{ColumnMapping, ColumnPair};
pub use compare::{CompareSpec, Comparison, Counts, Kind, TrimRule, Unit, NONE};
pub use encoding::TextEncoding;
pub use error::{CompareError, LoadError, Side};
pub use export::{ExportEncoding, ExportError, ExportFormat, ExportRequest};
pub use fixed::{FixedWidth, WidthUnit};
pub use pattern::{Pattern, Patterns};
pub use pipeline::{CompareRequest, Outcome, SideInput};
pub use table::{LoadOptions, Table};
pub use view::{CopyRequest, CopySide, Filter, Order, RangeRequest, Sort, SortBy, TextHit, View};
