//! The build engine: scanning source files for their module dependencies (with a
//! persistent cache), and the order to compile a target's files in.

pub mod entry;
pub mod plan;
pub mod scan;
pub mod scan_cache;

pub use entry::defines_main;
pub use plan::{
    Plan, PlanError, STD_MODULE, SourceFile, Step, check_one_main, has_extension,
    is_interface_file, plan,
};
pub use scan::{Deps, Provided, ScanError, ScanJob, scan};
pub use scan_cache::{CacheError, FlagsHash, Key, ScanCache};
