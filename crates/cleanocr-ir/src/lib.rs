//! Canonical Document Intermediate Representation (IR) and reading-order sorting for `cleanOCR`.

pub mod reading_order;
pub mod schema;

pub use reading_order::{
    ReadingOrderConfig, sort_reading_order, sort_reading_order_with_config,
};
pub use schema::{
    AxisValue, BBox, Block, BlockKind, ChartAxis, ChartIr, ChartKind, ChartPoint, ChartSeries,
    ConfidenceWarning, DocumentIr, DocumentMetadata, IR_SCHEMA_VERSION, ListItem, OverlayAsset,
    OverlayKind, PageIngestMode, PageIr, TableCell, TableIr, TextLine, TextSource, TextSpan,
};
