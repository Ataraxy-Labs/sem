//! The shared data-flow IR. A language front end lowers one file into a
//! [`DfFile`]; the engine reads only these types.
//!
//! Deliberately flow-insensitive and small: per function, the assignments,
//! calls, returns and field/attribute accesses, each carrying the *names*
//! its value is computed from. No control flow, no ordering — a later
//! assignment reaches an earlier use. That over-approximates (more flows,
//! never fewer) within what the front end understood; anything it did not
//! understand is recorded as a [`Dynamic`] marker, never dropped.

/// Which front end produced a file (selects the model set and separators).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Python,
    Ts,
    Go,
    Rust,
}

impl Lang {
    pub fn for_path(path: &str) -> Option<Lang> {
        let ext = path.rsplit_once('.').map(|(_, e)| e)?;
        Some(match ext {
            "py" => Lang::Python,
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Lang::Ts,
            "go" => Lang::Go,
            "rs" => Lang::Rust,
            _ => return None,
        })
    }

    /// Separator of a qualified name in the models (`os.environ`,
    /// `std::process::Command`, `net/http.Request`).
    pub fn sep(self) -> &'static str {
        match self {
            Lang::Rust => "::",
            _ => ".",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::Ts => "ts",
            Lang::Go => "go",
            Lang::Rust => "rust",
        }
    }
}

/// An imported name: `local` stands for the qualified `path` (in the
/// language's own separator). `spec` is the module specifier for JS/TS
/// (`./util`, `express`), resolved by the caller to a repo file or not.
#[derive(Clone, Debug, PartialEq)]
pub struct Import {
    pub local: String,
    pub path: String,
    /// JS/TS only: the specifier and the imported member (`None` = the
    /// namespace / default object).
    pub spec: Option<String>,
    pub member: Option<String>,
}

/// A name read inside an expression: `base` is the first identifier,
/// `chain` the dotted access text as far as it is a plain chain
/// (`request.args` for `request.args.get(..)`'s receiver).
#[derive(Clone, Debug, PartialEq)]
pub struct Read {
    pub base: String,
    pub chain: String,
    /// Byte offset of `base` (joins the call-graph pipeline's `Ref` sites).
    pub at: u32,
    pub row: u32,
}

/// The inputs an expression's value is computed from.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Val {
    pub reads: Vec<Read>,
    /// Calls whose results flow into the value (indices into `DfFn::calls`).
    pub calls: Vec<u32>,
}

impl Val {
    pub fn is_empty(&self) -> bool {
        self.reads.is_empty() && self.calls.is_empty()
    }
    pub fn extend(&mut self, o: Val) {
        self.reads.extend(o.reads);
        self.calls.extend(o.calls);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Callee {
    /// A plain name or dotted chain: `f`, `os.getenv`, `std::fs::read`.
    /// `at` is the byte offset of the last segment (the pipeline's site key).
    Path { chain: String, base: String },
    /// `recv.name(..)` where `recv` is not a plain chain (`f().g()`), or
    /// the receiver is a local value: `recv` is its value.
    Method { recv: Val, recv_chain: Option<String>, name: String },
    /// Anything else (`fns[i](..)`, `(a or b)(..)`).
    Dynamic,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub callee: Callee,
    /// Positional arguments, in order.
    pub args: Vec<Val>,
    /// Keyword / named arguments.
    pub kwargs: Vec<(String, Val)>,
    /// `f(*xs)` / `f(**kw)` / `f(...xs)`: positions unknown.
    pub splat: bool,
    /// Closures passed as arguments: `(arg index, parameter names)`.
    pub callbacks: Vec<(u32, Vec<String>)>,
    /// `new X(..)` (JS/TS): constructs `X`.
    pub construct: bool,
    pub at: u32,
    pub row: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Place {
    /// A new local binding (`let`, `const`, `:=`, a Python assignment).
    Local(String),
    /// An assignment to an existing name: a local if the function binds it,
    /// else module state (a global) when the file declares one.
    Name(String),
    /// `base.chain = ..` / `base[i] = ..`: field-insensitive, the whole
    /// object `base` (a local, `self`, a module global) is written.
    Attr { base: String, chain: String, at: u32 },
    /// A target the front end cannot name (`f().x = ..`).
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Stmt {
    Assign { to: Vec<Place>, from: Val, row: u32 },
    /// An evaluated expression (its calls matter for their effects).
    Eval(Val),
    Return(Val),
}

/// Something the front end saw but cannot model: reflection, `eval`,
/// dynamic attribute access, string-keyed dispatch.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Dynamic {
    pub row: u32,
    pub what: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Param {
    pub name: String,
    /// The declared type as written (`*http.Request`, `Request`).
    pub ty: Option<String>,
    /// The default value as written (Python: `Depends(get_db)`).
    pub default: Option<String>,
}

/// A parameter of a closure / nested function inlined into its enclosing
/// function (its name is a local there), with what the front end saw of
/// it: its declared type and the decorators of the nested definition.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosureParam {
    pub name: String,
    /// Position among the closure's parameters.
    pub index: u32,
    pub ty: Option<String>,
    /// Decorators of the nested definition, as callee chains (`mcp.tool`).
    pub decorators: Vec<String>,
    pub row: u32,
    pub default: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct DfFn {
    pub name: String,
    /// 0-based rows of the definition.
    pub row: u32,
    pub end_row: u32,
    /// The class / impl type the function is a method of.
    pub owner: Option<String>,
    /// The receiver name (`self`, `this`, a Go receiver) if a method.
    pub self_name: Option<String>,
    pub params: Vec<Param>,
    /// Decorators of the definition, as callee chains without arguments
    /// (`@mcp.tool()` -> `mcp.tool`, `@app.route("/x")` -> `app.route`).
    pub decorators: Vec<String>,
    /// Parameters of closures inlined into this function.
    pub closure_params: Vec<ClosureParam>,
    pub calls: Vec<Call>,
    pub stmts: Vec<Stmt>,
    pub dynamic: Vec<Dynamic>,
    /// Module-level code of the file (not a function).
    pub is_module: bool,
    /// Cyclomatic (McCabe: decisions + 1) and cognitive (nesting-weighted)
    /// complexity of the body.
    pub cyclomatic: u32,
    pub cognitive: u32,
}

#[derive(Clone, Debug, Default)]
pub struct DfFile {
    pub path: String,
    pub imports: Vec<Import>,
    /// Names assigned at module level (module state).
    pub globals: Vec<String>,
    /// Classes / types with their declared bases (for `this.m()` overrides).
    pub classes: Vec<(String, Vec<String>)>,
    /// Function 0 is the module-level pseudo-function.
    pub fns: Vec<DfFn>,
}
