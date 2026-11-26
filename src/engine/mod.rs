//! The build engine: reading what each source file declares and imports, and the
//! order to compile a target's files in.

pub mod plan;
pub mod scan;

pub use plan::{
    Plan, PlanError, STD_MODULE, SourceFile, Step, has_extension, is_interface_file, plan,
};
pub use scan::{Import, ModuleDecl, Scan};
