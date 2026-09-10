//! The strict `major.minor.patch` version that the manifest, the `Hello` frame
//! and the release tool all agree on. No `v` prefix, no pre-release suffix.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::UpdateError;

/// Ordering is lexicographic on the triple, which is exactly what deriving
/// `Ord` over the fields in this order gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// The version this binary was built from; every workspace crate shares it.
    pub fn current() -> Self {
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("CARGO_PKG_VERSION is not a major.minor.patch version")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = UpdateError;

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        let mut fields = [0u32; 3];
        let mut parts = raw.split('.');

        for field in &mut fields {
            *field = parts
                .next()
                .filter(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|part| part.parse().ok())
                .ok_or_else(|| invalid(raw))?;
        }
        if parts.next().is_some() {
            return Err(invalid(raw));
        }

        Ok(Version {
            major: fields[0],
            minor: fields[1],
            patch: fields[2],
        })
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        raw.parse().map_err(serde::de::Error::custom)
    }
}

fn invalid(raw: &str) -> UpdateError {
    UpdateError::Version(format!("{raw:?} is not a major.minor.patch version"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(raw: &str) -> Version {
        raw.parse().expect("should parse")
    }

    #[test]
    fn parses_a_plain_triple() {
        assert_eq!(
            parse("0.2.0"),
            Version {
                major: 0,
                minor: 2,
                patch: 0
            }
        );
        assert_eq!(
            parse("10.0.3"),
            Version {
                major: 10,
                minor: 0,
                patch: 3
            }
        );
    }

    #[test]
    fn accepts_leading_zeros() {
        assert_eq!(
            parse("01.2.3"),
            Version {
                major: 1,
                minor: 2,
                patch: 3
            }
        );
    }

    #[test]
    fn rejects_everything_that_is_not_a_bare_triple() {
        for raw in [
            "v1.2.3",
            "1.2",
            "1.2.3.4",
            "1.2.3-rc1",
            " 1.2.3",
            "1.2.3 ",
            "",
            "1..3",
            "1.2.x",
            "+1.2.3",
            "1.2.3\n",
        ] {
            assert!(
                raw.parse::<Version>().is_err(),
                "{raw:?} should not parse as a version"
            );
        }
    }

    #[test]
    fn orders_by_field_not_by_text() {
        assert!(parse("0.9.9") < parse("0.10.0"));
        assert!(parse("0.10.0") < parse("1.0.0"));
        assert!(parse("1.0.0") > parse("0.99.99"));
    }

    #[test]
    fn display_round_trips() {
        for raw in ["0.0.0", "0.2.0", "10.20.30"] {
            assert_eq!(parse(raw).to_string(), raw);
        }
    }

    #[test]
    fn current_parses() {
        assert_eq!(Version::current().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn serde_uses_the_string_form() {
        let version = parse("1.2.3");
        let json = serde_json::to_string(&version).expect("should serialise");
        assert_eq!(json, "\"1.2.3\"");
        assert_eq!(
            serde_json::from_str::<Version>(&json).expect("should deserialise"),
            version
        );
        assert!(serde_json::from_str::<Version>("\"1.2\"").is_err());
    }
}
