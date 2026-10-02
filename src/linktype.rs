//! Types of links a dotfile can be installed with.

use crate::error::{Error, Result};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    NoLink,
    Link,
    LinkChildren,
    Absolute,
    Relative,
}

impl LinkType {
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "nolink" => Ok(Self::NoLink),
            "link" => Ok(Self::Link),
            "link_children" => Ok(Self::LinkChildren),
            "absolute" => Ok(Self::Absolute),
            "relative" => Ok(Self::Relative),
            _ => Err(Error::Config(format!("bad LinkTypes value: \"{s}\""))),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::NoLink => "nolink",
            Self::Link => "link",
            Self::LinkChildren => "link_children",
            Self::Absolute => "absolute",
            Self::Relative => "relative",
        }
    }
}

impl fmt::Display for LinkType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
