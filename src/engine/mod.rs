//! The build engine: scanning source files for their module dependencies (with a
//! persistent cache), the order to compile a target's files in, the build graph, the
//! record of what each output was built from, and the state file that keeps it all.

pub mod depfile;
pub mod entry;
pub mod graph;
pub mod headers;
pub mod outputs;
pub mod plan;
pub mod scan;
pub mod scan_cache;
pub mod state;

pub use entry::defines_main;
pub use graph::{Action, GraphTarget, Node, graph};
pub use headers::{HeaderRecord, headers_from_depfile};
pub use outputs::{CommandHash, CommandHasher, Outputs};
pub use plan::{
    Plan, PlanError, STD_MODULE, SourceFile, Step, check_one_main, has_extension,
    is_interface_file, plan,
};
pub use scan::{Deps, Provided, ScanError, ScanJob, scan};
pub use scan_cache::{FlagsHash, HeaderDigest, Key, ScanCache};
pub use state::{State, StoreError};
