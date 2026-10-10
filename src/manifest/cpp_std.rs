use std::fmt;
use std::str::FromStr;

/// A C++ language standard, as written in `Yam.toml` (`std = "c++26"`).
///
/// In a manifest this is a *minimum*: the lowest standard the project's own code and
/// its public interface need. Each project compiles all of its own files at one
/// standard, `max(own std, declared std of each direct dependency)`, so the ordering
/// here matters. See `docs/build.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CppStd {
    Cpp11,
    Cpp14,
    Cpp17,
    Cpp20,
    Cpp23,
    Cpp26,
}

impl CppStd {
    /// Every supported standard, oldest first.
    pub const ALL: [Self; 6] = [
        Self::Cpp11,
        Self::Cpp14,
        Self::Cpp17,
        Self::Cpp20,
        Self::Cpp23,
        Self::Cpp26,
    ];

    /// The standard used when a manifest doesn't specify one.
    pub const DEFAULT: Self = Self::Cpp26;

    /// The spelling used in `Yam.toml`, e.g. `c++26`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cpp11 => "c++11",
            Self::Cpp14 => "c++14",
            Self::Cpp17 => "c++17",
            Self::Cpp20 => "c++20",
            Self::Cpp23 => "c++23",
            Self::Cpp26 => "c++26",
        }
    }

    /// Whether `import std;` is available at this standard.
    #[must_use]
    pub const fn supports_import_std(self) -> bool {
        matches!(self, Self::Cpp23 | Self::Cpp26)
    }
}

impl fmt::Display for CppStd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The value wasn't one of `c++11`, `c++14`, `c++17`, `c++20`, `c++23` or `c++26`.
///
/// C++98/03 are deliberately unsupported: such code usually builds as C++11 with
/// small fixes, and libc++'s C++03 mode is maintenance-only.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid std '{0}'; expected one of: c++11, c++14, c++17, c++20, c++23, c++26")]
pub struct ParseCppStdError(pub String);

impl FromStr for CppStd {
    type Err = ParseCppStdError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|std| std.as_str() == value)
            .ok_or_else(|| ParseCppStdError(value.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_standard() {
        for std in CppStd::ALL {
            assert_eq!(std.as_str().parse::<CppStd>(), Ok(std));
        }
    }

    #[test]
    fn rejects_unknown_spellings() {
        for bad in ["c++98", "c++03", "C++26", "26", "gnu++26", ""] {
            assert!(bad.parse::<CppStd>().is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn orders_oldest_to_newest() {
        assert!(CppStd::Cpp11 < CppStd::Cpp14);
        assert!(CppStd::Cpp17 < CppStd::Cpp20);
        assert!(CppStd::Cpp23 < CppStd::Cpp26);
        assert_eq!(CppStd::ALL.into_iter().max(), Some(CppStd::Cpp26));
    }

    #[test]
    fn import_std_needs_cpp23() {
        assert!(!CppStd::Cpp20.supports_import_std());
        assert!(CppStd::Cpp23.supports_import_std());
    }
}
