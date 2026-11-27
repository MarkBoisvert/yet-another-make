//! The build graph: every step a build can run, and the steps each one needs first.
//!
//! - **`Std`**: the `std` module (`std.pcm` and `std.o`), when anything imports `std`.
//! - **`Compile`**: one source file of one target. It needs the steps that produce
//!   the modules it imports, from its own target or the project library, and `Std`
//!   if it imports `std`.
//! - **`Archive`**: the library's `.a`, from all of its compiles.
//! - **`Link`**: a bin's executable, from its compiles, the library's archive when it
//!   links the library, and `Std` when there is one (`std.o` is linked in).
//!
//! Nodes come in an order where every node follows the ones it needs, so running
//! them in order is always valid. The parallel scheduler (#25) runs independent
//! nodes at the same time instead.

use std::collections::HashMap;

use super::plan::{Plan, STD_MODULE, SourceFile};

/// What a node does. Targets and steps are indices into what the graph was built
/// from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Std,
    /// `step` indexes the target's plan steps.
    Compile {
        target: usize,
        step: usize,
    },
    Archive {
        target: usize,
    },
    Link {
        target: usize,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub action: Action,
    /// The nodes this one needs, as indices into the graph.
    pub needs: Vec<usize>,
}

/// One target, as the graph sees it.
pub struct GraphTarget<'a> {
    pub is_lib: bool,
    /// For a bin: whether it links the project library.
    pub links_lib: bool,
    pub files: &'a [SourceFile],
    pub plan: &'a Plan,
}

/// The graph for `targets`, which hold at most one library, listed first.
#[must_use]
pub fn graph(targets: &[GraphTarget<'_>]) -> Vec<Node> {
    let mut nodes = Vec::new();
    let std = targets.iter().any(|t| t.plan.imports_std).then(|| {
        nodes.push(Node {
            action: Action::Std,
            needs: Vec::new(),
        });
        0
    });

    let mut lib = None;
    for (index, target) in targets.iter().enumerate() {
        // Which node provides each module this target may import.
        let mut providers: HashMap<&str, usize> = match &lib {
            Some((_, lib_providers)) if target.links_lib && !target.is_lib => {
                HashMap::clone(lib_providers)
            }
            _ => HashMap::new(),
        };
        let mut compiles = Vec::with_capacity(target.plan.steps.len());
        for (position, step) in target.plan.steps.iter().enumerate() {
            let file = &target.files[step.file];
            let mut needs: Vec<usize> = Vec::new();
            for module in &file.deps.requires {
                let node = if &**module == STD_MODULE {
                    std
                } else {
                    providers.get(&**module).copied()
                };
                if let Some(node) = node
                    && !needs.contains(&node)
                {
                    needs.push(node);
                }
            }
            let id = nodes.len();
            nodes.push(Node {
                action: Action::Compile {
                    target: index,
                    step: position,
                },
                needs,
            });
            if let Some(module) = &step.provides {
                providers.insert(module, id);
            }
            compiles.push(id);
        }

        let mut needs = compiles;
        if target.is_lib {
            nodes.push(Node {
                action: Action::Archive { target: index },
                needs,
            });
            lib = Some((nodes.len() - 1, providers));
        } else {
            if target.links_lib
                && let Some((archive, _)) = &lib
            {
                needs.push(*archive);
            }
            needs.extend(std);
            nodes.push(Node {
                action: Action::Link { target: index },
                needs,
            });
        }
    }
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::plan;
    use crate::engine::scan::{Deps, ModuleName, Provided};
    use std::collections::HashSet;

    fn file(path: &str, provides: Option<&str>, requires: &[&str]) -> SourceFile {
        SourceFile {
            path: path.into(),
            deps: Deps {
                provides: provides.map(|name| Provided {
                    name: name.into(),
                    is_interface: true,
                }),
                requires: requires.iter().map(|r| (*r).into()).collect(),
            },
        }
    }

    #[test]
    fn nodes_follow_what_they_need() {
        use Action::{Archive, Compile, Link, Std};
        let lib_files = [
            file("a.cppm", Some("a"), &["std", "b"]),
            file("b.cppm", Some("b"), &[]),
            file("a.cpp", None, &["a"]),
        ];
        let lib_plan = plan::plan("lib", &lib_files, &HashSet::<ModuleName>::new()).unwrap();
        let bin_files = [file("main.cpp", None, &["a", "std"])];
        let available = HashSet::from([ModuleName::from("a"), ModuleName::from("b")]);
        let bin_plan = plan::plan("bin", &bin_files, &available).unwrap();
        let nodes = graph(&[
            GraphTarget {
                is_lib: true,
                links_lib: false,
                files: &lib_files,
                plan: &lib_plan,
            },
            GraphTarget {
                is_lib: false,
                links_lib: true,
                files: &bin_files,
                plan: &bin_plan,
            },
        ]);

        let actions: Vec<Action> = nodes.iter().map(|n| n.action).collect();
        assert_eq!(
            actions,
            [
                Std,
                Compile { target: 0, step: 0 }, // b.cppm
                Compile { target: 0, step: 1 }, // a.cppm
                Compile { target: 0, step: 2 }, // a.cpp
                Archive { target: 0 },
                Compile { target: 1, step: 0 }, // main.cpp
                Link { target: 1 },
            ]
        );
        assert_eq!(nodes[1].needs, Vec::<usize>::new());
        assert_eq!(nodes[2].needs, [0, 1]);
        assert_eq!(nodes[3].needs, [2]);
        assert_eq!(nodes[4].needs, [1, 2, 3]);
        assert_eq!(nodes[5].needs, [2, 0]);
        assert_eq!(nodes[6].needs, [5, 4, 0]);
        for (index, node) in nodes.iter().enumerate() {
            assert!(node.needs.iter().all(|&need| need < index), "{index}");
        }
    }

    #[test]
    fn no_std_node_without_import_std() {
        let files = [file("main.cpp", None, &[])];
        let plan = plan::plan("bin", &files, &HashSet::<ModuleName>::new()).unwrap();
        let nodes = graph(&[GraphTarget {
            is_lib: false,
            links_lib: false,
            files: &files,
            plan: &plan,
        }]);
        assert_eq!(
            nodes.iter().map(|n| n.action).collect::<Vec<_>>(),
            [
                Action::Compile { target: 0, step: 0 },
                Action::Link { target: 0 }
            ]
        );
        assert_eq!(nodes[1].needs, [0]);
    }
}
