use std::fmt;

pub struct Entity {
    pub name: String,
    pub kind: String,
}

impl Entity {
    pub fn new(name: String, kind: String) -> Self {
        Entity { name, kind }
    }

    pub fn display_name(&self) -> String {
        format!("{} ({})", self.name, self.kind)
    }
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.name)
    }
}

pub enum ParseError {
    Empty,
    InvalidSyntax(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Empty => write!(f, "empty content"),
            ParseError::InvalidSyntax(msg) => write!(f, "invalid syntax: {}", msg),
        }
    }
}
